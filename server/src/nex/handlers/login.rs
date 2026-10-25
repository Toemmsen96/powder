//! LoginProtocol: the first two NEX calls of every session.

use super::{HandlerDeps, HandlerFn, Result};
use crate::core::sessions::NexChannel;
use crate::nex::dispatch::{Ctx, HandlerError};
use crate::nex::rmc::{parse_guid, Reader, Writer};
use serde_json::json;
use std::net::IpAddr;
use std::sync::Arc;

pub const HANDLERS: &[(&str, HandlerFn)] = &[
    ("LoginProtocol::LoginWithToken_V1", login_with_token),
    ("LoginProtocol::Register_V1", register),
];

pub fn is_loopback(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host == "localhost",
    }
}

/// params: type map {ClientVersionInfo: 1}, String token, ClientVersionInfo, String. The token is the ticket our
/// HTTPS login issued, which ties this connection to that player.
/// outputs (reader FUN_7ff661b5bf30): GUID principal (= profile id), String StationURL, bool redirected.
/// redirected MUST be false: true makes the client reconnect elsewhere.
///
/// The connection becomes the session's NexChannel (its outbox), for server-initiated requests.
pub fn login_with_token(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let token = r.string()?;
    let ip = ctx.conn.addr.ip().to_string();
    let Some(session) = deps.accounts.session_for_ticket(&token, &ip)? else {
        return Err(HandlerError::Rmc("Authentication".into(), 1));
    };
    let principal = parse_guid(session.profile_id())?;
    ctx.conn.profile_id = Some(session.profile_id().to_string());
    let channel: Arc<dyn NexChannel> = ctx.conn.outbox.clone();
    session.channels.set_nex(Some(channel));
    ctx.conn.session = Some(session.clone());
    if let Some(mods) = &deps.mods {
        mods.lock().emit("nex_connected", &json!({
            "profile_id": session.profile_id(), "name": session.player.name, "session_id": session.session_id,
            "peer": ip, "pid": ctx.conn.pid,
        }));
    }
    log::info!(target: "powder.rmc", "NEX login: {} ({}) from {}", session.player.name, session.profile_id(),
               ctx.conn.addr);
    Ok(Writer::new().guid(&principal).string(&ctx.station_url).bool(false).take())
}

/// params: type map (empty), List<String> = the client's own station URLs (one per NIC).
/// outputs (reader FUN_7ff661b5c020): u32 connection id, String public URL = the client's address as this server
/// sees it (other keys, e.g. sid, kept from its first URL). The game on the server's own PC connects from
/// 127.0.0.1, which other players can't reach: it gets public_host instead (it goes into the station URLs the game
/// hands to other players for peer-to-peer).
pub fn register(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let urls = r.strings()?;
    let mut host = ctx.conn.addr.ip().to_string();
    let public_host = &deps.config.server.public_host;
    if is_loopback(&host) && !is_loopback(public_host) {
        host = public_host.clone();
    }
    let mut keys: Vec<(String, String)> = vec![("address".into(), host), ("port".into(), ctx.conn.addr.port().to_string())];
    if let Some((_, rest)) = urls.first().and_then(|u| u.split_once(":/")) {
        for kv in rest.split(';') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            if !k.is_empty() && !keys.iter().any(|(have, _)| have == k) {
                keys.push((k.into(), v.into()));
            }
        }
    }
    let public = format!("prudp:/{}", keys.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";"));
    Ok(Writer::new().u32(ctx.conn.pid).string(&public).take())
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use crate::config::Config;
    use crate::core::identity::LoginRequest;
    use crate::nex::connection::{Connection, SendFn};
    use crate::nex::dispatch::Ctx;
    use crate::nex::prudp::{Packet, CONNECT, DATA, FLAG_ACK, FLAG_KEY_EXCHANGE, FLAG_NEED_ACK, FLAG_RELIABLE, SYN};
    use crate::nex::rmc::{self, Message, Reader, Writer};
    use crate::nex::secure;
    use crate::nex::server::NexServer;
    use p256::ecdsa::SigningKey;
    use parking_lot::Mutex;
    use std::net::SocketAddr;
    use std::sync::Arc;

    /// A game client speaking PRUDP + the secure handshake to a NexServer driven by hand.
    struct Game {
        srv: NexServer,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        addr: SocketAddr,
        key: [u8; 16],
        seq: u16,
        now: f64,
    }

    impl Game {
        fn connect(srv: NexServer, sent: Arc<Mutex<Vec<Vec<u8>>>>, addr: &str) -> Game {
            let mut g = Game { srv, sent, addr: addr.parse().unwrap(), key: [0; 16], seq: 2, now: 0.0 };
            g.srv.received(0.0, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), g.addr);
            let (blob, eph) = secure::client_key();
            let connect = Packet { kind: CONNECT, flags: FLAG_NEED_ACK | FLAG_RELIABLE | FLAG_KEY_EXCHANGE,
                                   sequence_id: 1, connection_signature: [1, 2, 3, 4], key_blob: blob,
                                   ..Default::default() };
            g.srv.received(0.1, &connect.encode(), g.addr);
            let ack = Packet::decode(g.sent.lock().last().unwrap()).unwrap();
            g.key = secure::client_session_key(&eph, &ack.key_blob).unwrap();
            g.sent.lock().clear();
            g
        }

        /// Send a call in 730-byte fragments, like the game.
        fn call(&mut self, protocol: &str, call_id: u32, method: &str, params: &[u8]) {
            let req = rmc::build_request(protocol, call_id, method, params);
            let chunks: Vec<&[u8]> = req.chunks(730).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let p = Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: self.seq,
                                 fragment_id: if i == chunks.len() - 1 { 0 } else { i as u8 + 1 },
                                 payload: secure::encrypt_payload(&self.key, self.seq, chunk, false),
                                 ..Default::default() };
                self.now += 0.01;
                self.srv.received(self.now, &p.encode(), self.addr);
                self.seq += 1;
            }
        }

        /// The messages the server sent since the last read (acking each DATA packet).
        fn read(&mut self) -> Vec<Message> {
            let packets: Vec<Packet> = self.sent.lock().drain(..).map(|d| Packet::decode(&d).unwrap()).collect();
            let (mut out, mut parts) = (vec![], vec![]);
            for p in packets.into_iter().filter(|p| p.kind == DATA && !p.has(FLAG_ACK)) {
                let ack = Packet { kind: DATA, flags: FLAG_ACK, sequence_id: p.sequence_id,
                                   fragment_id: p.fragment_id, ..Default::default() };
                self.srv.received(self.now, &ack.encode(), self.addr);
                parts.extend(secure::decrypt_payload(&self.key, &p.payload, Some(p.sequence_id)).unwrap());
                if p.fragment_id == 0 {
                    out.push(rmc::parse(&std::mem::take(&mut parts)).unwrap());
                }
            }
            self.sent.lock().clear();
            out
        }
    }

    fn server(deps: &Arc<super::HandlerDeps>) -> (NexServer, Arc<Mutex<Vec<Vec<u8>>>>) {
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let s = sent.clone();
        let send_fn: SendFn = Arc::new(move |d, _| s.lock().push(d.to_vec()));
        let key = SigningKey::random(&mut rand::thread_rng());
        (NexServer::new(testing::dispatcher(deps), key, "127.0.0.1", 6005, send_fn), sent)
    }

    #[test]
    fn login_with_token_binds_the_connection_for_server_push() {
        let deps = testing::deps(Config::default());
        let session = deps.accounts.login(&LoginRequest::new("127.0.0.1", "", "")).unwrap();
        let (srv, sent) = server(&deps);
        let mut game = Game::connect(srv, sent, "127.0.0.1:3074");

        // LoginWithToken with our HTTPS ticket (3 KB-ish params -> several fragments, like the game)
        let params = Writer::new().u32(1).string("ClientVersionInfo").u16(1).string(&session.ticket)
            .u16(1).u16(0).u16(0).u32(0).string(&"x".repeat(2500)).take();
        game.call("LoginProtocol", 5, "LoginProtocol::LoginWithToken_V1", &params);
        let [resp]: [Message; 1] = game.read().try_into().unwrap();
        assert_eq!((resp.error, resp.method.as_str(), resp.call_id), (None, "LoginProtocol::LoginWithToken_V1*", 5));
        let mut out = Reader::new(&resp.body);
        assert_eq!(out.guid().unwrap().to_string(), session.profile_id());
        let url = out.string().unwrap();
        assert!(url.starts_with("prudp:/address=127.0.0.1;port=6005;CID=1;DID="), "{url}");
        assert!(!out.bool().unwrap() && out.remaining() == 0);
        // the game drops every server notification unless the station URL carries a non-zero DID
        let did: u32 = url.split(";DID=").nth(1).unwrap().split(';').next().unwrap().parse().unwrap();
        assert!(did > 0);
        assert_eq!(game.srv.conns[&game.addr].profile_id.as_deref(), Some(session.profile_id()));
        assert!(session.channels.nex().is_some(), "connection registered for server push");

        // unknown call -> RMC error, connection survives
        game.call("FooProtocol", 8, "FooProtocol::Bar_V1", b"");
        let [err]: [Message; 1] = game.read().try_into().unwrap();
        assert_eq!((err.error, err.call_id), (Some(("Core".into(), 2)), 8));

        // server push from outside the server: queued, sent on the next pass (here: the next tick)
        assert!(deps.sessions.push_nex(session.profile_id(), "NotificationProtocol",
                                       rmc::NOTIFICATION_METHOD, b"\x00", Some(0)));
        assert!(game.read().is_empty());
        game.srv.tick(game.now);
        let [pushed]: [Message; 1] = game.read().try_into().unwrap();
        assert!(pushed.is_request && pushed.call_id == 0 && pushed.method == rmc::NOTIFICATION_METHOD);
        assert_eq!(pushed.body, b"\x00");
        game.srv.tick(game.now + 1.0);
        assert!(game.sent.lock().is_empty(), "every server DATA packet was acknowledged");

        // the connection closes: the session forgets it and the player is reported gone
        let gone: Arc<Mutex<Vec<String>>> = Arc::default();
        let g = gone.clone();
        game.srv.on_disconnect = Some(Box::new(move |pid| g.lock().push(pid.to_string())));
        game.srv.drop_conn(game.addr, "test");
        assert!(session.channels.nex().is_none());
        assert_eq!(*gone.lock(), vec![session.profile_id().to_string()]);
        assert!(!deps.sessions.push_nex(session.profile_id(), "P", "P::M_V1", b"", None));
    }

    #[test]
    fn unknown_ticket_is_attributed_in_lenient_mode_and_refused_in_strict() {
        let deps = testing::deps(Config::default());
        let player = deps.accounts.default_profile_id().unwrap();
        let (srv, sent) = server(&deps);
        let mut game = Game::connect(srv, sent, "127.0.0.1:3074");
        let params = Writer::new().u32(0).string("emu.not-a-ticket-we-issued").take();
        game.call("LoginProtocol", 5, "LoginProtocol::LoginWithToken_V1", &params);
        let [resp]: [Message; 1] = game.read().try_into().unwrap();
        assert_eq!(Reader::new(&resp.body).guid().unwrap().to_string(), player);

        let mut config = Config::default();
        config.server.strict_tickets = true;
        let deps = testing::deps(config);
        let (srv, sent) = server(&deps);
        let mut game = Game::connect(srv, sent, "127.0.0.1:3074");
        game.call("LoginProtocol", 5, "LoginProtocol::LoginWithToken_V1", &params);
        let [resp]: [Message; 1] = game.read().try_into().unwrap();
        assert_eq!(resp.error, Some(("Authentication".into(), 1)));
        assert!(game.srv.conns[&game.addr].profile_id.is_none());
    }

    #[test]
    fn register_answers_the_public_address() {
        let mut config = Config::default();
        config.server.public_host = "203.0.113.7".into();
        let deps = testing::deps(config);
        let send_fn: SendFn = Arc::new(|_, _| {});
        let call = |addr: &str, urls: &[&str]| {
            let mut conn = Connection::new(addr.parse().unwrap(), send_fn.clone(), 0x1234, 0x3f, 0x31, 0.0);
            let mut ctx = Ctx { conn: &mut conn, station_url: String::new(), now: 0.0 };
            let urls: Vec<String> = urls.iter().map(|u| u.to_string()).collect();
            let params = Writer::new().u32(0).strings(&urls).take();
            let out = super::register(&deps, &mut ctx, &mut Reader::new(&params)).unwrap();
            let mut r = Reader::new(&out);
            (r.u32().unwrap(), r.string().unwrap())
        };
        // the game on the server's PC: loopback -> public_host; keys of its first URL kept after address/port
        assert_eq!(call("127.0.0.1:3074", &["prudp:/address=192.168.1.5;port=3074;sid=15;type=2", "prudp:/x=1"]),
                   (0x1234, "prudp:/address=203.0.113.7;port=3074;sid=15;type=2".into()));
        assert_eq!(call("198.51.100.2:50000", &[]), (0x1234, "prudp:/address=198.51.100.2;port=50000".into()));
        // byte-exact: u32 pid, String
        let send = send_fn.clone();
        let mut conn = Connection::new("198.51.100.2:1".parse().unwrap(), send, 2, 0x3f, 0x31, 0.0);
        let mut ctx = Ctx { conn: &mut conn, station_url: String::new(), now: 0.0 };
        let out = super::register(&deps, &mut ctx, &mut Reader::new(&[0, 0, 0, 0, 0, 0, 0, 0])).unwrap();
        assert_eq!(out, b"\x02\x00\x00\x00\x23\x00prudp:/address=198.51.100.2;port=1\x00");
        assert!(super::is_loopback("::1") && super::is_loopback("localhost") && !super::is_loopback("example.org"));
    }
}

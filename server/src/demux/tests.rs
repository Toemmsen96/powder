//! The demux end to end: whole sessions through [`run`] over an in-memory stream, and the service replies. Expected
//! bytes marked "Python" were made by the Python server's codec (powder/server/transport/demux/pb.py) for the same
//! messages.

use super::pb::{self, F, V};
use super::services::{answer, ServiceContext};
use super::*;
use crate::assets::{Assets, ConnectProducts};
use crate::db::Db;
use crate::repos::Repos;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

const PYTHON_PATCH: &str = "0000004b0a4908012a4508011201301800223568747470733a2f2f737461746963332e63646e2e7562692e63\
                            6f6d2f6f726269742f72656c65617365732f302f706174636865732f28c46830003800";
const PYTHON_UTILITY: &str = "000000160a14080032100801120c0a0a0a080a02555312024e41";
const PYTHON_AUTH: &str = "000000080a06080212020801";
const PYTHON_OPEN: &str = "0000000a0a0808031a0408011001";
const PYTHON_UNKNOWN_SERVICE: &str = "000000080a06080932020800";
const PYTHON_FRIENDS: &str = "0000002f122d0a2b08011227000000230a210804b2011c08011218080012120a057069642d6212054775657\
                              3741a0020001803";
const PYTHON_ACH: &str = "0000003c123a0a38080212340000003008071a2c0a057069642d6112230a210a090a035354501202504312140a08\
                          08011080a0f8fa050a08081e1080e2cfaa06";
const PYTHON_ACH_EMPTY: &str = "08071a130a00120f0a0d0a090a03535450120250431200";
const PYTHON_LEGACY: &str = "0a4f08041a4b080012034745541a242f5f706f776465722f636c6f756473617665732f6c65676163792d6c69\
                             73742f33323830221a7075626c69632d75626973657276696365732e7562692e636f6d2a00";
const PYTHON_PLAYTIME: &str = "0a06080312020800";

const PID_A: &str = "pid-a";
const PID_B: &str = "pid-b";

struct Fixture {
    deps: Arc<DemuxDeps>,
    _dir: tempfile::TempDir,
}

fn fixture_with(assets: Assets, config: Config) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config;
    config.server.data_dir = dir.path().to_path_buf();
    let config = Arc::new(config);
    let db = Arc::new(Db::memory().unwrap());
    db.migrate().unwrap();
    let repos = Arc::new(Repos::new(db));
    let accounts = Arc::new(AccountService::new(config.clone(), repos.clone(), Arc::new(SessionRegistry::new())));
    let cloudsaves = Arc::new(CloudSaveService::new(repos, dir.path()));
    let deps = DemuxDeps { config, assets: Arc::new(assets), accounts, cloudsaves, clients: Arc::new(ConnectClients::new()),
                           mods: None };
    Fixture { deps: Arc::new(deps), _dir: dir }
}

fn fixture() -> Fixture {
    fixture_with(Assets::default(), Config::default())
}

fn ctx<'a>(deps: &'a DemuxDeps, profile_id: Option<&'a str>) -> ServiceContext<'a> {
    ServiceContext { deps, profile_id }
}

/// Connect's side of one connection.
struct Client {
    io: DuplexStream,
    server: JoinHandle<anyhow::Result<()>>,
}

impl Client {
    fn connect(deps: &Arc<DemuxDeps>, port: u16) -> Client {
        let (io, server_io) = tokio::io::duplex(1 << 20);
        let peer: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let server = tokio::spawn(run(server_io, peer, deps.clone()));
        Client { io, server }
    }

    async fn send(&mut self, fields: &[F]) {
        self.io.write_all(&pb::frame(&pb::encode(fields))).await.unwrap();
    }

    /// One framed message, frame included.
    async fn recv_raw(&mut self) -> Vec<u8> {
        let mut head = [0u8; 4];
        tokio::time::timeout(std::time::Duration::from_secs(3), self.io.read_exact(&mut head)).await
            .expect("a reply").unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
        self.io.read_exact(&mut body).await.unwrap();
        [head.to_vec(), body].concat()
    }

    async fn request_raw(&mut self, rid: u64, kind: u32, body: Vec<F>) -> Vec<u8> {
        self.send(&[F::msg(1, vec![F::int(1, rid), F::msg(kind, body)])]).await;
        self.recv_raw().await
    }

    async fn request(&mut self, rid: u64, kind: u32, body: Vec<F>) -> Vec<F> {
        let raw = self.request_raw(rid, kind, body).await;
        let rsp = pb::sub(&pb::decode(&raw[4..]).unwrap(), 1).unwrap();
        assert_eq!(pb::int(&rsp, 1), Some(rid));
        pb::sub(&rsp, kind).unwrap()
    }

    async fn data_raw(&mut self, cid: u64, msg: &[u8]) -> Vec<u8> {
        self.send(&[F::msg(2, vec![F::msg(1, vec![F::int(1, cid), F::bytes(2, &pb::frame(msg))])])]).await;
        self.recv_raw().await
    }

    async fn data(&mut self, cid: u64, msg: &[u8]) -> Vec<F> {
        let raw = self.data_raw(cid, msg).await;
        let d = pb::sub(&pb::sub(&pb::decode(&raw[4..]).unwrap(), 2).unwrap(), 1).unwrap();
        assert_eq!(pb::int(&d, 1), Some(cid));
        pb::decode(pb::unframe(pb::bytes(&d, 2).unwrap()).unwrap()).unwrap()
    }

    async fn close(self) {
        drop(self.io);
        self.server.await.unwrap().unwrap();
    }
}

fn nested(rid: u64, kind: u32, body: V) -> Vec<u8> {
    pb::encode(&[F::msg(1, vec![F::int(1, rid), F(kind, body)])])
}

/// The reply body of a nested reply {1: {1 rid, kind: body}}.
fn nested_body(reply: &[F], rid: u64, kind: u32) -> Vec<F> {
    let r = pb::sub(reply, 1).unwrap();
    assert_eq!(pb::int(&r, 1), Some(rid));
    pb::sub(&r, kind).unwrap()
}

#[tokio::test]
async fn session() {
    let f = fixture();
    let mut c = Client::connect(&f.deps, 50001);
    c.send(&[F::msg(2, vec![F::msg(4, vec![F::int(1, 13380)])])]).await; // client version: no reply
    // patch info without a pack: "up to date" with the client's own version
    assert_eq!(hex::encode(c.request_raw(1, 5, vec![F::int(1, 0)]).await), PYTHON_PATCH);
    let utility = c.request_raw(0, 6, vec![F::str(1, "utility_service"), F::bytes(2, b"")]).await;
    assert_eq!(hex::encode(utility), PYTHON_UTILITY, "country and region from the config");

    let auth = c.request_raw(2, 2, vec![F::msg(1, vec![F::str(3, "any-ticket")]), F::str(2, "uplay_pc")]).await;
    assert_eq!(hex::encode(auth), PYTHON_AUTH, "lenient: unknown ticket ok");
    let me = f.deps.accounts.default_profile_id().unwrap();
    let live = f.deps.clients.snapshot();
    assert_eq!(live.len(), 1);
    assert_eq!((live[0].peer.as_str(), live[0].profile_id.as_deref()), ("127.0.0.1:50001", Some(me.as_str())));

    assert_eq!(hex::encode(c.request_raw(3, 3, vec![F::str(2, "ownership_service")]).await), PYTHON_OPEN);
    let ach = c.request(4, 3, vec![F::str(2, "ach_frontend")]).await;
    assert_eq!((pb::int(&ach, 1), pb::int(&ach, 2)), (Some(2), Some(1)));
    assert_eq!(f.deps.clients.snapshot()[0].services, ["ach_frontend", "ownership_service"]);

    // owned games: Steep's built-in products, cloud saves on (default)
    let reply = c.data(1, &nested(8, 2, V::Bytes(vec![]))).await;
    let body = nested_body(&reply, 8, 2);
    let products: Vec<Vec<F>> = pb::sub(&body, 2).unwrap().iter()
        .map(|p| match &p.1 { V::Bytes(b) => pb::decode(b).unwrap(), _ => panic!() }).collect();
    assert_eq!(products.len(), 62);
    let ids: Vec<u64> = products.iter().filter_map(|p| pb::int(p, 1)).collect();
    assert!(ids.contains(&3279) && ids.contains(&3280));
    assert!(products.iter().any(|p| pb::text(p, 17).contains("cloud_saves: yes")));
    assert_eq!((pb::int(&body, 1), pb::int(&body, 5), pb::int(&body, 6)), (Some(1), Some(0), Some(0)));

    // Uplay ticket (nested style): a fresh token, our request id
    let reply = c.data(1, &nested(9, 14, V::Msg(vec![F::int(1, 3280)]))).await;
    let token = nested_body(&reply, 9, 13);
    assert_eq!(pb::int(&token, 1), Some(1));
    assert!(pb::text(&token, 2).len() > 100);

    // flat style
    let reply = c.data(2, &pb::encode(&[F::int(1, 6), F::bytes(4, b"")])).await;
    assert_eq!(pb::int(&reply, 1), Some(6));

    c.send(&[F::msg(2, vec![F::msg(6, vec![F::int(1, 3280)])])]).await; // product started: no reply
    c.send(&[F::msg(2, vec![F::bytes(3, b"")])]).await; // keep-alive: no reply
    c.send(&[F::msg(2, vec![F::msg(1, vec![F::int(1, 99), F::bytes(2, &pb::frame(b"x"))])])]).await; // unknown: none
    c.send(&[F::msg(2, vec![F::msg(2, vec![F::int(1, 2)])])]).await; // ach_frontend closed
    let unknown = c.request_raw(9, 6, vec![F::str(1, "nope_service"), F::bytes(2, b"")]).await;
    assert_eq!(hex::encode(unknown), PYTHON_UNKNOWN_SERVICE, "connection still healthy");
    assert_eq!(f.deps.clients.snapshot()[0].services, ["ownership_service"]);
    c.close().await;
    assert!(f.deps.clients.snapshot().is_empty(), "gone from the live clients");
}

#[tokio::test]
async fn patch_check_on_its_own_connection_gets_the_reported_version() {
    // Connect asks for patch info on short connections of its own, without reporting its version there (as in the
    // real traffic); it must still hear its own version, not the recorded one.
    let recorded = pb::encode(&[F::int(1, 1), F::str(2, "release"), F::int(5, 100)]);
    let assets = Assets { products: ConnectProducts { patch_info: Some(recorded.clone()), products: vec![] },
                          ..Default::default() };
    let f = fixture_with(assets, Config::default());
    let mut first = Client::connect(&f.deps, 50002);
    assert_eq!(pb::encode(&first.request(1, 5, vec![]).await), recorded, "no version known: as recorded");
    let mut main = Client::connect(&f.deps, 50003);
    let mut check = Client::connect(&f.deps, 50004);
    main.send(&[F::msg(2, vec![F::msg(4, vec![F::int(1, 13380)])])]).await;
    main.request(1, 2, vec![F::msg(1, vec![F::str(3, "any-ticket")]), F::str(2, "uplay_pc")]).await; // push handled
    let patch = check.request(0, 5, vec![F::str(1, "174.0"), F::int(2, 0)]).await;
    assert_eq!(patch, vec![F::int(1, 1), F::str(2, "release"), F::int(5, 13380)]);
    assert_eq!(f.deps.repos().kv_get("server", "connect_version").unwrap(), Some(serde_json::json!(13380)),
               "kept across restarts");
    for c in [first, main, check] {
        c.close().await;
    }
}

#[tokio::test]
async fn data_for_an_open_service_comes_back_framed_like_python() {
    let f = fixture();
    let repos = f.deps.repos();
    for (pid, name) in [(PID_A, "Rider"), (PID_B, "Guest")] {
        repos.create_player(pid, pid, name).unwrap();
    }
    let mut c = Client::connect(&f.deps, 50005);
    // signed in as pid-a: with no accounts the default player is the first one created
    repos.kv_set("server", "default_player", &serde_json::json!(PID_A)).unwrap();
    c.request(1, 2, vec![F::msg(1, vec![F::str(3, "t")])]).await;
    c.request(2, 3, vec![F::str(2, "friends_service")]).await;
    c.request(3, 3, vec![F::str(2, "ach_frontend")]).await;
    let request = nested(4, 22, V::Msg(vec![F::int(1, 1)]));
    assert_eq!(hex::encode(c.data_raw(1, &request).await), PYTHON_FRIENDS);
    repos.earn_achievements(PID_A, "STP:PC", &BTreeMap::from([(1, 1_600_000_000), (30, 1_700_000_000)])).unwrap();
    let read = pb::encode(&[F::int(1, 7), F::msg(3, vec![F::msg(2, vec![F::str(1, "STP"), F::str(2, "PC")])])]);
    assert_eq!(hex::encode(c.data_raw(2, &read).await), PYTHON_ACH);
    c.close().await;
}

// ---- achievements ------------------------------------------------------------------------------------------------
fn write(rid: u64, product: (&str, &str), earned: &[(u64, u64)]) -> Vec<u8> {
    let key = vec![F::str(1, product.0), F::str(2, product.1), F::str(3, crate::constants::SPACE_ID)];
    let entries = earned.iter().map(|(i, t)| F::msg(1, vec![F::int(1, *i), F::int(2, *t)])).collect();
    pb::encode(&[F::int(1, rid), F::msg(4, vec![F::msg(1, vec![F::msg(1, vec![F::msg(1, key), F::msg(2, entries)])])])])
}

fn read(rid: u64) -> Vec<u8> {
    pb::encode(&[F::int(1, rid), F::msg(3, vec![F::msg(2, vec![F::str(1, "STP"), F::str(2, "PC")])])])
}

fn earned(reply: &[u8]) -> BTreeMap<u64, u64> {
    let body = pb::sub(&pb::decode(reply).unwrap(), 3).unwrap();
    let entry = pb::sub(&pb::sub(&body, 2).unwrap(), 1).unwrap();
    pb::decode(pb::bytes(&entry, 2).unwrap()).unwrap().iter().map(|e| {
        let V::Bytes(b) = &e.1 else { panic!() };
        let e = pb::decode(b).unwrap();
        (pb::int(&e, 1).unwrap(), pb::int(&e, 2).unwrap())
    }).collect()
}

#[test]
fn achievements_are_stored_and_returned() {
    let f = fixture();
    let deps = &f.deps;
    let me = ctx(deps, Some(PID_A));
    deps.repos().earn_achievements(PID_A, "STP:PC", &BTreeMap::from([(1, 1_600_000_000), (2, 1_600_000_100)])).unwrap();
    assert_eq!(earned(&answer(&me, "ach_frontend", &read(1)).unwrap()),
               BTreeMap::from([(1, 1_600_000_000), (2, 1_600_000_100)]));
    // Connect writes its whole known list: one new, one with a later time (earliest stays)
    let ack = answer(&me, "ach_frontend", &write(2, ("STP", "PC"), &[(1, 1_700_000_000), (30, 1_700_000_000)])).unwrap();
    assert_eq!(ack, [0x08, 0x02, 0x22, 0x00]);
    assert_eq!(earned(&answer(&me, "ach_frontend", &read(3)).unwrap()),
               BTreeMap::from([(1, 1_600_000_000), (2, 1_600_000_100), (30, 1_700_000_000)]));
    // another product is kept apart; another player sees nothing
    answer(&me, "ach_frontend", &write(4, ("XYZ", "PC"), &[(7, 5)]));
    let mut products = deps.repos().achievement_products(PID_A).unwrap();
    products.sort();
    assert_eq!(products, [("STP:PC".to_string(), 3), ("XYZ:PC".to_string(), 1)]);
    assert!(earned(&answer(&ctx(deps, Some(PID_B)), "ach_frontend", &read(5)).unwrap()).is_empty());
    // without a login nothing is stored, and the reply still has the (empty) earned list
    let nobody = ctx(deps, None);
    answer(&nobody, "ach_frontend", &write(6, ("STP", "PC"), &[(99, 1)]));
    assert!(!deps.repos().achievements(PID_A, "STP:PC").unwrap().iter().any(|(id, _)| *id == 99));
    assert_eq!(hex::encode(answer(&nobody, "ach_frontend", &read(7)).unwrap()), PYTHON_ACH_EMPTY);
}

#[test]
fn every_other_player_with_an_account_is_a_friend() {
    let f = fixture();
    let repos = f.deps.repos();
    for (pid, name) in [(PID_A, "Rider"), (PID_B, "Guest"), ("pid-c", "NoAccount")] {
        repos.create_player(pid, pid, name).unwrap();
    }
    let friends = |pid: &str| -> Vec<(String, String)> {
        let reply = answer(&ctx(&f.deps, Some(pid)), "friends_service", &nested(4, 22, V::Msg(vec![F::int(1, 1)])))
            .unwrap();
        let body = nested_body(&pb::decode(&reply).unwrap(), 4, 22);
        assert_eq!(pb::int(&body, 1), Some(1));
        body.iter().filter(|e| e.0 == 2).map(|e| {
            let V::Bytes(b) = &e.1 else { panic!() };
            let e = pb::decode(b).unwrap();
            assert_eq!((pb::int(&e, 1), pb::int(&e, 3)), (Some(0), Some(3)));
            let who = pb::sub(&e, 2).unwrap();
            (pb::text(&who, 1), pb::text(&who, 2))
        }).collect()
    };
    // without accounts: every other player, by name
    assert_eq!(friends(PID_A), [(PID_B.into(), "Guest".into()), ("pid-c".into(), "NoAccount".into())]);
    repos.create_account("acc-a", "a@example.org", "x", PID_A).unwrap();
    repos.create_account("acc-b", "b@example.org", "x", PID_B).unwrap();
    assert_eq!(friends(PID_A), [(PID_B.to_string(), "Guest".to_string())]);
    assert_eq!(friends(PID_B), [(PID_A.to_string(), "Rider".to_string())]);
}

// ---- cloud saves and ownership tokens ----------------------------------------------------------------------------
const PRODUCT: u64 = 3280;

fn md5_b64(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    crate::util::b64(&Md5::digest(data))
}

/// A CloudsaveUrlReq {1 token, 2 product, 3 method, 4* items} -> (status, [(header text, URL)]).
fn cloud_links(deps: &DemuxDeps, pid: Option<&str>, items: Vec<F>, method: u64) -> (u64, Vec<(String, String)>) {
    let mut req = vec![F::str(1, "ownership-token"), F::int(2, PRODUCT), F::int(3, method)];
    req.extend(items);
    let reply = answer(&ctx(deps, pid), "cloudsave_service", &nested(9, 2, V::Msg(req))).unwrap();
    let body = nested_body(&pb::decode(&reply).unwrap(), 9, 2);
    let links = body.iter().filter(|e| e.0 == 2).map(|e| {
        let V::Bytes(b) = &e.1 else { panic!() };
        let e = pb::decode(b).unwrap();
        (pb::text(&e, 1), pb::text(&e, 2))
    }).collect();
    (pb::int(&body, 1).unwrap(), links)
}

fn token_of(url: &str) -> &str {
    url.split("?t=").nth(1).unwrap()
}

#[test]
fn cloud_save_links() {
    let f = fixture();
    let deps = &f.deps;
    let data = b"save one".to_vec();
    let put = vec![F::msg(4, vec![F::str(1, "1"), F::msg(2, vec![F::int(1, data.len() as u64), F::str(2, &md5_b64(&data))])])];
    assert_eq!(cloud_links(deps, None, put.clone(), 1).0, 3, "denied without a login");
    let (status, links) = cloud_links(deps, Some(PID_A), put, 1);
    assert_eq!(status, 1);
    let (headers, url) = &links[0];
    assert_eq!(headers, &format!("Content-Length: 8\nContent-MD5: {}\n", md5_b64(&data)));
    assert!(url.starts_with("https://public-ubiservices.ubi.com/_powder/cloudsaves/3280/1?t="));
    // the HTTPS side redeems the link
    let link = deps.cloudsaves.resolve(token_of(url), PRODUCT, "1", Some("PUT")).unwrap();
    deps.cloudsaves.store(&link, &data, None).unwrap();

    let get = |slot: &str| vec![F::msg(4, vec![F::str(1, slot)])];
    assert_eq!(cloud_links(deps, Some(PID_A), get("2"), 2).0, 2, "not stored: error");
    assert_eq!(cloud_links(deps, Some(PID_A), get("../x"), 2).0, 2, "bad slot: error");
    let (status, links) = cloud_links(deps, Some(PID_A), get("1"), 2);
    assert_eq!((status, links[0].0.as_str()), (1, ""), "no headers for a download");
    let link = deps.cloudsaves.resolve(token_of(&links[0].1), PRODUCT, "1", Some("GET")).unwrap();
    assert_eq!(deps.cloudsaves.read(&link).unwrap(), data);
    assert_eq!(cloud_links(deps, Some(PID_B), get("1"), 2).0, 2, "another player's save is not theirs");
    assert_eq!(cloud_links(deps, Some(PID_A), get("1"), 9).0, 3, "unknown method");
}

#[test]
fn nothing_to_migrate_from_the_old_cloud_saves() {
    let f = fixture();
    let list = V::Msg(vec![F::int(1, PRODUCT), F::bytes(5, b""), F::str(6, "Rider"), F::str(7, "ownership-token")]);
    let reply = answer(&ctx(&f.deps, Some(PID_A)), "cloudsave_service", &nested(4, 3, list)).unwrap();
    assert_eq!(hex::encode(reply), PYTHON_LEGACY);
    let other = answer(&ctx(&f.deps, Some(PID_A)), "cloudsave_service", &nested(4, 3, V::Msg(vec![F::int(1, PRODUCT)])))
        .unwrap();
    assert_eq!(pb::int(&nested_body(&pb::decode(&other).unwrap(), 4, 3), 1), Some(1), "only listing is supported");
}

#[test]
fn ownership_token_signature_and_simple_acks() {
    let f = fixture();
    let me = ctx(&f.deps, Some(PID_A));
    for (kind, reply_kind) in [(28, 27), (26, 25)] {
        let reply = answer(&me, "ownership_service", &nested(5, kind, V::Msg(vec![F::int(1, PRODUCT)]))).unwrap();
        let body = nested_body(&pb::decode(&reply).unwrap(), 5, reply_kind);
        assert_eq!(pb::int(&body, 1), Some(1));
        assert!(pb::int(&body, 3).unwrap() > crate::util::unix_now() as u64);
    }
    let sig = nested_body(&pb::decode(&answer(&me, "ownership_service", &nested(5, 26, V::Bytes(vec![]))).unwrap()).unwrap(), 5, 25);
    assert_eq!(pb::bytes(&sig, 4).map(<[u8]>::len), Some(64));
    assert_eq!(hex::encode(answer(&me, "playtime_service", &nested(3, 2, V::Bytes(vec![]))).unwrap()), PYTHON_PLAYTIME);
    for (service, kind, reply_kind) in [("friends_service", 13, 13), ("friends_service", 19, 19), ("ownership_service", 19, 18)] {
        let reply = answer(&me, service, &nested(1, kind, V::Bytes(vec![]))).unwrap();
        assert_eq!(nested_body(&pb::decode(&reply).unwrap(), 1, reply_kind), [F::int(1, 1)]);
    }
    assert!(answer(&me, "ownership_service", &nested(1, 99, V::Bytes(vec![]))).is_none(), "unhandled: no reply");
}

#[test]
fn owned_games_from_the_pack_without_cloud_saves_when_off() {
    let entry = |pid: u64| pb::encode(&[F::int(1, pid), F::str(17, "root:\r\n  cloud_saves: yes\r\n")]);
    let assets = Assets { products: ConnectProducts { patch_info: None, products: vec![entry(3280), entry(1)] },
                          ..Default::default() };
    let mut config = Config::default();
    config.server.connect_cloud_saves = false;
    let f = fixture_with(assets, config);
    let reply = answer(&ctx(&f.deps, None), "ownership_service", &nested(7, 2, V::Bytes(vec![]))).unwrap();
    let products = pb::sub(&nested_body(&pb::decode(&reply).unwrap(), 7, 2), 2).unwrap();
    assert_eq!(products.len(), 2);
    for p in products {
        let V::Bytes(b) = p.1 else { panic!() };
        assert_eq!(pb::text(&pb::decode(&b).unwrap(), 17), "root:\r\n  cloud_saves: no\r\n");
    }
}

#[test]
fn built_in_owned_games_list_steep_for_both_versions() {
    let f = fixture();
    let reply = answer(&ctx(&f.deps, None), "ownership_service", &nested(7, 2, V::Msg(vec![]))).unwrap();
    let body = nested_body(&pb::decode(&reply).unwrap(), 7, 2);
    let expected = crate::builtin::products::entries(true, None);
    let got: Vec<Vec<u8>> = pb::sub(&body, 2).unwrap().into_iter()
        .map(|p| match p.1 { V::Bytes(b) => b, _ => panic!() }).collect();
    assert_eq!(got, expected, "byte for byte the built-in entries");
}

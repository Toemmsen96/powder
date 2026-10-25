//! What the HTTPS listener lets one client use: connections (in all and per address), time to finish the TLS
//! handshake and to send a request's headers, and request bodies (per request, by path, and all of them in memory
//! at once). Generous for real use (a few players, Ubisoft Connect's many parallel connections, MB-sized cloud
//! saves and photos), small enough that nobody can make the server run out of memory or connections without
//! signing in. The Python server (aiohttp) had none of these besides a 64 MiB body limit for every request.
//!
//! A request over its body limit gets a 413, one that does not send its body in time a 408 (UbiServices' error
//! shape, `Connection: close`); a connection over a connection limit is closed right after it is accepted.

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use hyper::body::Body;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Request bodies are read in full before the pipeline runs. Most calls send small JSON (the game's telemetry
/// batches and Connect's remotelog lines are KBs, entities and GraphQL queries less): at most this much.
pub const MAX_BODY: usize = 2 << 20;
/// Uploaded files: photos with their thumbnails (UGC create carries them inline, base64), ghosts and trails
/// (entity files), cloud saves. 64 MiB is what the Python server allowed for every request.
pub const MAX_UPLOAD: usize = 64 << 20;
/// Connect's chat channels (create one, post a message): a channel or a message is a few hundred bytes.
pub const MAX_CHANNEL_BODY: usize = 16 << 10;
/// The paths [`MAX_UPLOAD`] applies to (prefixes), and UGC create's exact paths.
const UPLOAD_PREFIXES: [&str; 3] = ["/_powder/entity-files/", "/_powder/ugc/", "/_powder/cloudsaves/"];
const UPLOAD_PATHS: [&str; 2] = ["/ugc/v1", "/ugc/v1/"];
static CHANNEL_PATH: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^/v1/spaces/[^/]+/channels(/[^/]+/messages)?$").unwrap());

/// The body limit for a request to this path, and the budget (and timeout) it counts against.
pub fn body_rules(path: &str) -> (usize, Class) {
    if UPLOAD_PREFIXES.iter().any(|p| path.starts_with(p)) || UPLOAD_PATHS.contains(&path) {
        (MAX_UPLOAD, Class::Upload)
    } else if CHANNEL_PATH.is_match(path) {
        (MAX_CHANNEL_BODY, Class::Small)
    } else {
        (MAX_BODY, Class::Small)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Class {
    Small,
    Upload,
}

/// One listener's limits; [`Limits::default`] is the server's.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Open connections in all (HTTP, websockets, the demux, handshakes in progress). Ubisoft Connect alone keeps
    /// a few dozen open (its embedded browser, the demux, the websocket); a few players fit many times over.
    pub max_connections: usize,
    /// Open connections from one address (one PC, or a household behind one NAT address).
    pub max_per_ip: usize,
    /// From accepting the TCP connection to the end of the TLS handshake.
    pub tls_handshake: Duration,
    /// To send a request's headers; also how long an idle keep-alive connection stays open (hyper starts the
    /// clock when it waits for the next request), as aiohttp's keep-alive timeout did (75 s).
    pub header_read: Duration,
    /// To send a request's body once its headers are in: a small one (2 MiB even at 64 KB/s) ...
    pub body: Duration,
    /// ... and an upload (64 MiB at about 1 Mbit/s).
    pub upload: Duration,
    /// Request bodies in memory at once (bytes), uploads and everything else apart (so uploads cannot hold up the
    /// small calls). A request whose body does not fit waits for room (within its body timeout).
    pub small_budget: usize,
    pub upload_budget: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_connections: 512, max_per_ip: 128, tls_handshake: Duration::from_secs(20),
                 header_read: Duration::from_secs(75), body: Duration::from_secs(60),
                 upload: Duration::from_secs(600), small_budget: 64 << 20, upload_budget: 256 << 20 }
    }
}

impl Limits {
    pub fn body_timeout(&self, class: Class) -> Duration {
        match class {
            Class::Small => self.body,
            Class::Upload => self.upload,
        }
    }
}

/// The budgets count KiB (a semaphore permit each).
const KIB: usize = 1024;
/// Refused connections are logged at most this often (an attack would flood the log otherwise).
const REFUSED_LOG_EVERY_S: u64 = 10;

/// A listener's limits as they are used: who is connected, the body budgets.
pub struct Gate {
    pub limits: Limits,
    connections: Arc<Semaphore>,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    small: Arc<Semaphore>,
    upload: Arc<Semaphore>,
    last_refusal_log: AtomicU64,
}

impl Gate {
    pub fn new(limits: Limits) -> Arc<Gate> {
        Arc::new(Gate { connections: Arc::new(Semaphore::new(limits.max_connections)), per_ip: Mutex::default(),
                        small: Arc::new(Semaphore::new(limits.small_budget / KIB)),
                        upload: Arc::new(Semaphore::new(limits.upload_budget / KIB)),
                        last_refusal_log: AtomicU64::new(0), limits })
    }

    /// A new connection from `ip`, if the limits allow it; it counts until the guard drops.
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<ConnGuard> {
        let Ok(permit) = self.connections.clone().try_acquire_owned() else {
            self.refused(ip, &format!("{} connections open", self.limits.max_connections));
            return None;
        };
        let mut per_ip = self.per_ip.lock();
        let n = per_ip.entry(ip).or_insert(0);
        if *n >= self.limits.max_per_ip {
            drop(per_ip);
            self.refused(ip, &format!("{} connections open from there", self.limits.max_per_ip));
            return None;
        }
        *n += 1;
        Some(ConnGuard { gate: self.clone(), ip, _permit: permit })
    }

    fn refused(&self, ip: IpAddr, why: &str) {
        let now = crate::util::unix_now() as u64;
        let last = self.last_refusal_log.load(Ordering::Relaxed);
        if now >= last + REFUSED_LOG_EVERY_S
            && self.last_refusal_log.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            log::warn!(target: "powder.tls", "connection from {ip} refused: {why} (further refusals in the next \
                                               {REFUSED_LOG_EVERY_S} s are not logged)");
        }
    }

    pub fn budget(&self, class: Class) -> &Arc<Semaphore> {
        match class {
            Class::Small => &self.small,
            Class::Upload => &self.upload,
        }
    }

    #[cfg(test)]
    pub fn open_from(&self, ip: IpAddr) -> usize {
        self.per_ip.lock().get(&ip).copied().unwrap_or(0)
    }
}

/// One admitted connection (kept by its task, and by the websocket that takes it over).
pub struct ConnGuard {
    gate: Arc<Gate>,
    ip: IpAddr,
    _permit: OwnedSemaphorePermit,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut per_ip = self.gate.per_ip.lock();
        if let Some(n) = per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                per_ip.remove(&self.ip);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum BodyError {
    TooLarge,
    Failed(String),
}

/// A request body in full, at most `limit` bytes, counted against `budget` as it arrives (so holding budget
/// takes sending the bytes); -> the bytes and the budget they hold (given back when dropped).
pub async fn read_body<B>(mut body: B, limit: usize, budget: &Arc<Semaphore>)
                          -> Result<(Bytes, Option<OwnedSemaphorePermit>), BodyError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    if body.size_hint().lower() > limit as u64 {
        return Err(BodyError::TooLarge); // Content-Length says so: refused before reading anything
    }
    let mut buf = BytesMut::new();
    let (mut held, mut held_kib): (Option<OwnedSemaphorePermit>, usize) = (None, 0);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| BodyError::Failed(e.to_string()))?;
        let Ok(data) = frame.into_data() else { continue }; // trailers
        let len = buf.len() + data.len();
        if len > limit {
            return Err(BodyError::TooLarge);
        }
        let need = len.div_ceil(KIB);
        if need > held_kib {
            let more = u32::try_from(need - held_kib).map_err(|e| BodyError::Failed(e.to_string()))?;
            let more = budget.clone().acquire_many_owned(more).await.map_err(|e| BodyError::Failed(e.to_string()))?;
            match &mut held {
                Some(h) => h.merge(more),
                None => held = Some(more),
            }
            held_kib = need;
        }
        buf.extend_from_slice(&data);
    }
    Ok((buf.freeze(), held))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{Full, StreamBody};
    use hyper::body::Frame;

    #[test]
    fn body_limits_by_path() {
        for path in ["/v3/profiles/me/events", "/v1/profiles/me/remotelog", "/v2/profiles/entities", "/ugc/v1/x/like",
                     "/v1/profiles/x/entities", "/_powder/account/login", "/v1/spaces/s/channels/c"] {
            assert_eq!(body_rules(path), (MAX_BODY, Class::Small), "{path}");
        }
        for path in ["/_powder/entity-files/abc", "/_powder/ugc/abc_thumbnail", "/_powder/cloudsaves/3280/1",
                     "/ugc/v1", "/ugc/v1/"] {
            assert_eq!(body_rules(path), (MAX_UPLOAD, Class::Upload), "{path}");
        }
        for path in ["/v1/spaces/abc/channels", "/v1/spaces/abc/channels/c-1/messages"] {
            assert_eq!(body_rules(path).0, MAX_CHANNEL_BODY, "{path}");
        }
    }

    fn chunked(chunks: Vec<Vec<u8>>) -> impl Body<Data = Bytes, Error = std::convert::Infallible> + Unpin {
        let frames = chunks.into_iter().map(|c| Ok(Frame::data(Bytes::from(c))));
        StreamBody::new(futures_util::stream::iter(frames.collect::<Vec<_>>()))
    }

    #[tokio::test]
    async fn bodies_within_their_limit_and_the_budget() {
        let budget = Arc::new(Semaphore::new(8)); // 8 KiB
        let (bytes, held) = read_body(Full::new(Bytes::from(vec![1u8; 3000])), 4000, &budget).await.unwrap();
        assert_eq!((bytes.len(), budget.available_permits()), (3000, 5));
        drop(held);
        assert_eq!(budget.available_permits(), 8);
        // Content-Length over the limit: refused at once; no length (chunked): once the bytes pass it
        assert_eq!(read_body(Full::new(Bytes::from(vec![0u8; 4001])), 4000, &budget).await.unwrap_err(),
                   BodyError::TooLarge);
        assert_eq!(read_body(chunked(vec![vec![0; 3000], vec![0; 1001]]), 4000, &budget).await.unwrap_err(),
                   BodyError::TooLarge);
        assert_eq!(budget.available_permits(), 8, "a refused body gives its budget back");
        let (bytes, held) = read_body(chunked(vec![vec![0; 1]; 2000]), 4000, &budget).await.unwrap();
        assert_eq!((bytes.len(), budget.available_permits()), (2000, 6), "counted in KiB, not per chunk");
        // no room: the body waits until there is
        let pending = read_body(Full::new(Bytes::from(vec![0u8; 7 * 1024])), 8 << 10, &budget);
        assert!(tokio::time::timeout(Duration::from_millis(50), pending).await.is_err());
        drop(held);
        let fits = read_body(Full::new(Bytes::from(vec![0u8; 7 * 1024])), 8 << 10, &budget);
        assert!(tokio::time::timeout(Duration::from_millis(50), fits).await.is_ok());
    }

    #[test]
    fn connections_per_address_and_in_all() {
        let gate = Gate::new(Limits { max_connections: 3, max_per_ip: 2, ..Limits::default() });
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let a1 = gate.admit(a).unwrap();
        let a2 = gate.admit(a).unwrap();
        assert!(gate.admit(a).is_none(), "per address");
        let b1 = gate.admit(b).unwrap();
        assert!(gate.admit(b).is_none(), "in all");
        drop(a1);
        assert_eq!(gate.open_from(a), 1);
        let b2 = gate.admit(b).unwrap();
        drop((a2, b1, b2));
        assert_eq!((gate.open_from(a), gate.open_from(b)), (0, 0));
        assert_eq!(gate.connections.available_permits(), 3);
    }
}

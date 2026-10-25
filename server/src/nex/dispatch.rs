//! RMC request -> reply. Resolution order:
//!   1. mods      (`api.before_rmc`)          may answer the call themselves
//!   2. handler   (stateful game logic)       may decline with Fallthrough
//!   3. recording (the game-data pack's rmc)  the real server's reply
//!   4. error     Core / 2                    logged as UNHANDLED so we learn what to add
//!
//! Only calls whose answer depends on per-player state or live ids get a handler. Everything else is answered
//! from the recordings, which are the real server's bytes (hand-built guesses of these structures were proven
//! wrong by the captures).

use super::connection::Connection;
use super::rmc::{self, Message, Reader};
use crate::assets::Assets;
use crate::mods::ModHost;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

/// What a handler gets besides the params: the calling connection (its player, once logged in).
pub struct Ctx<'a> {
    pub conn: &'a mut Connection,
    pub station_url: String,
    pub now: f64,
}

/// How a handler declines or fails.
#[derive(Debug)]
pub enum HandlerError {
    /// The recorded reply (if any) is used instead.
    Fallthrough,
    /// Answer the call with an RMC error (name, code) instead of outputs.
    Rmc(String, u16),
    /// A bug or bad params: logged, then as Fallthrough.
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for HandlerError {
    fn from(e: anyhow::Error) -> Self {
        HandlerError::Failed(e)
    }
}

pub type Handler = Box<dyn Fn(&mut Ctx, &mut Reader) -> Result<Vec<u8>, HandlerError> + Send + Sync>;

pub struct Dispatcher {
    pub assets: Arc<Assets>,
    pub mods: Option<Arc<Mutex<ModHost>>>,
    pub handlers: HashMap<String, Handler>,
}

impl Dispatcher {
    pub fn new(assets: Arc<Assets>, mods: Option<Arc<Mutex<ModHost>>>) -> Dispatcher {
        Dispatcher { assets, mods, handlers: HashMap::new() }
    }

    pub fn handle(&mut self, method: &str, f: Handler) {
        self.handlers.insert(method.to_string(), f);
    }

    pub fn dispatch(&self, ctx: &mut Ctx, msg: &Message) -> Vec<u8> {
        let (method, params) = (msg.method.as_str(), msg.body.as_slice());
        if let Some(mods) = &self.mods {
            let mod_ctx = json!({
                "call_id": msg.call_id, "protocol": msg.protocol, "method": method,
                "profile_id": ctx.conn.profile_id, "peer": ctx.conn.addr.ip().to_string(), "pid": ctx.conn.pid,
            });
            if let Some(out) = mods.lock().before_rmc(method, &mod_ctx, params) {
                return reply(msg, &out, "mod");
            }
        }
        if let Some(f) = self.handlers.get(method) {
            match f(ctx, &mut Reader::new(params)) {
                Ok(out) => return reply(msg, &out, "handler"),
                Err(HandlerError::Fallthrough) => {}
                Err(HandlerError::Rmc(name, code)) => {
                    log::warn!(target: "powder.rmc", "#{} {method} -> error {name}/{code}", msg.call_id);
                    return rmc::build_error(&msg.protocol, msg.call_id, &name, code);
                }
                Err(HandlerError::Failed(e)) => {
                    log::error!(target: "powder.rmc", "#{} {method}: handler failed, falling back to the recording: \
                                {e:#}", msg.call_id);
                }
            }
        }
        if let Some((out, how)) = self.assets.rmc.lookup(method, params) {
            return reply(msg, &out, &format!("recorded:{how}"));
        }
        log::warn!(target: "powder.rmc", "#{} {method} -> UNHANDLED (params {}B: {})", msg.call_id, params.len(),
                   hex::encode(&params[..params.len().min(64)]));
        rmc::build_error(&msg.protocol, msg.call_id, "Core", 2)
    }
}

fn reply(msg: &Message, outputs: &[u8], how: &str) -> Vec<u8> {
    log::info!(target: "powder.rmc", "#{} {} -> {how} ({}B)", msg.call_id, msg.method, outputs.len());
    rmc::build_response(&msg.protocol, msg.call_id, &msg.method, outputs)
}

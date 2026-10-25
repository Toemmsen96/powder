//! /_powder/: the server's own API for the launcher. So far the client mods (see client_mods.rs); the status,
//! account and ca.pem routes of routes_powder.py are not ported yet.

use super::{Ctx, Reply, Route};
use crate::client_mods;
use crate::util::unquote_path;

pub fn routes() -> Vec<Route> {
    vec![
        Route::new("GET", client_mods::INDEX_PATH, client_mod_index),
        Route::new("GET", "/_powder/mods/{rest:.+}", client_mod_file),
    ]
}

/// The mods' client files the launcher may download (only those its player approved get loaded).
fn client_mod_index(ctx: &Ctx) -> Reply {
    let index = match &ctx.state.mods {
        Some(mods) => client_mods::index(mods.lock().client_mods()),
        None => client_mods::index(&[]),
    };
    Reply::json(&index, "powder", 200)
}

/// The launcher quotes each path segment (a space is %20); after decoding, only listed files match.
fn client_mod_file(ctx: &Ctx) -> Reply {
    let rest = unquote_path(ctx.param("rest"));
    let data = ctx.state.mods.as_ref().and_then(|m| client_mods::file(m.lock().client_mods(), &rest));
    match data {
        Some(bytes) => Reply::new(200, "application/octet-stream", bytes, "powder"),
        None => Reply::new(404, "text/plain; charset=utf-8", b"404: Not Found".to_vec(), "powder"),
    }
}

//! Messages inside a demux service connection (ownership_service, friends_service, ...).
//!
//! Each service speaks its own protobuf, framed like the outer stream. Requests carry an id in one of two places,
//! and the reply echoes it in the same place:
//!
//!   nested  {1: {1: request_id, <kind>: request}}      most services
//!   flat    {1: request_id, <kind>: request}            ach_frontend
//!
//! `kind` (the field number of the request) says which request it is. A handler for (service, kind) builds the
//! reply (generated.rs, ownership.rs); anything else gets no reply and is logged as DEMUX UNHANDLED.

use super::pb::{self, F, V};
use super::{generated, ownership, DemuxDeps};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Nested,
    Flat,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServiceRequest {
    pub service: String,
    pub style: Style,
    pub request_id: u64,
    pub kind: u32,
    /// The request message of that kind (raw: bytes, or a varint).
    pub body: V,
}

impl ServiceRequest {
    /// The body as a message: None when it is a varint or not a message (empty bytes -> empty list).
    pub fn fields(&self) -> Option<Vec<F>> {
        match &self.body {
            V::Bytes(b) => pb::try_decode(b),
            _ => None,
        }
    }

    pub fn body_bytes(&self) -> &[u8] {
        match &self.body {
            V::Bytes(b) => b,
            _ => &[],
        }
    }
}

/// The first field other than the request id.
fn kind_field(fields: &[F]) -> Option<&F> {
    fields.iter().find(|f| f.0 != 1)
}

pub fn parse_request(service: &str, msg: &[u8]) -> Option<ServiceRequest> {
    let fields = pb::try_decode(msg).filter(|f| !f.is_empty())?;
    let make = |style, request_id, kind: &F| ServiceRequest {
        service: service.to_string(), style, request_id, kind: kind.0, body: kind.1.clone(),
    };
    match pb::first(&fields, 1) {
        Some(V::Bytes(top)) => {
            let sub = pb::try_decode(top)?;
            let rid = pb::int(&sub, 1)?;
            kind_field(&sub).map(|k| make(Style::Nested, rid, k))
        }
        Some(V::Int(rid)) => kind_field(&fields).map(|k| make(Style::Flat, *rid, k)),
        _ => None,
    }
}

/// A recorded reply with its request id replaced (same place as in the request).
pub fn with_request_id(reply: &[u8], request_id: u64) -> anyhow::Result<Vec<u8>> {
    let fields = pb::decode(reply)?;
    Ok(match pb::first(&fields, 1) {
        Some(V::Bytes(top)) => {
            let sub = pb::replace(&pb::decode(top)?, 1, V::Int(request_id));
            pb::encode(&pb::replace(&fields, 1, V::Bytes(pb::encode(&sub))))
        }
        _ => pb::encode(&pb::replace(&fields, 1, V::Int(request_id))),
    })
}

/// A reply in the request's style: `body` is the reply message for field `kind`.
pub fn make_reply(req: &ServiceRequest, kind: u32, body: V) -> Vec<u8> {
    let rid = F(1, V::Int(req.request_id));
    match req.style {
        Style::Nested => pb::encode(&[F::msg(1, vec![rid, F(kind, body)])]),
        Style::Flat => pb::encode(&[rid, F(kind, body)]),
    }
}

/// Who is asking: the deps and the player the connection signed in as.
pub struct ServiceContext<'a> {
    pub deps: &'a DemuxDeps,
    pub profile_id: Option<&'a str>,
}

/// Code for one service request: the full reply (see [`make_reply`]), or None for no reply.
pub type Handler = fn(&ServiceContext, &ServiceRequest) -> Option<Vec<u8>>;

pub fn handler(service: &str, kind: u32) -> Option<Handler> {
    ownership::handler(service, kind).or_else(|| generated::handler(service, kind))
}

pub fn answer(ctx: &ServiceContext, service: &str, msg: &[u8]) -> Option<Vec<u8>> {
    let Some(req) = parse_request(service, msg) else {
        log::warn!(target: "powder.demux", "DEMUX UNPARSED {service} message ({}B): {}", msg.len(),
                   hex::encode(&msg[..msg.len().min(48)]));
        return None;
    };
    let out = handler(service, req.kind).and_then(|f| f(ctx, &req));
    match &out {
        None => log::warn!(target: "powder.demux", "DEMUX UNHANDLED {service} #{} kind {} ({}B request): {}",
                           req.request_id, req.kind, msg.len(), pb::outline(&req.body, 0)),
        Some(o) => log::info!(target: "powder.demux", "demux {service} #{} kind {} -> {}B", req.request_id,
                              req.kind, o.len()),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &[u8] = b"made-up-token";

    #[test]
    fn request_styles_and_id_rewrite() {
        let nested = pb::encode(&[F::msg(1, vec![F::int(1, 7), F::msg(14, vec![F::int(1, 3280)])])]);
        let req = parse_request("ownership_service", &nested).unwrap();
        assert_eq!((req.style, req.request_id, req.kind), (Style::Nested, 7, 14));
        let flat = pb::encode(&[F::int(1, 5), F::bytes(4, b"")]);
        let req = parse_request("ach_frontend", &flat).unwrap();
        assert_eq!((req.style, req.request_id, req.kind), (Style::Flat, 5, 4));
        let recorded = pb::encode(&[F::msg(1, vec![F::int(1, 3), F::msg(13, vec![F::int(1, 1), F::bytes(2, TOKEN)])])]);
        let out = pb::sub(&pb::decode(&with_request_id(&recorded, 42).unwrap()).unwrap(), 1).unwrap();
        assert_eq!(pb::int(&out, 1), Some(42));
        assert_eq!(pb::bytes(&pb::sub(&out, 13).unwrap(), 2), Some(TOKEN));
        // flat recorded reply
        let flat_reply = with_request_id(&pb::encode(&[F::int(1, 1), F::bytes(4, b"")]), 9).unwrap();
        assert_eq!(flat_reply, [0x08, 0x09, 0x22, 0x00]);
    }

    #[test]
    fn replies_in_the_request_style() {
        let nested = parse_request("x", &pb::encode(&[F::msg(1, vec![F::int(1, 7), F::bytes(2, b"")])])).unwrap();
        assert_eq!(make_reply(&nested, 3, V::Msg(vec![F::int(1, 1)])), [0x0a, 0x06, 0x08, 0x07, 0x1a, 0x02, 0x08, 0x01]);
        let flat = parse_request("x", &[0x08, 0x05, 0x22, 0x00]).unwrap();
        assert_eq!(make_reply(&flat, 4, V::Bytes(vec![])), [0x08, 0x05, 0x22, 0x00]);
    }

    #[test]
    fn unparsable_requests() {
        assert!(parse_request("x", b"").is_none());
        assert!(parse_request("x", &[0x0a, 0x05, 0x01]).is_none(), "not protobuf");
        assert!(parse_request("x", &pb::encode(&[F::int(1, 5)])).is_none(), "no kind");
        assert!(parse_request("x", &pb::encode(&[F::str(2, "no id")])).is_none());
    }
}

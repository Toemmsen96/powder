//! NEX RMC message codec (Ubisoft's name-based variant), reconstructed from the game binary.
//!
//!   request : [u32 size][String protocol][u8 1][u32 callID][String "Proto::Method_V1"][params...]
//!   response: [u32 size][String protocol][u8 0][u8 1][u32 callID][String "Proto::Method_V1*"][outputs...]
//!   error   : [u32 size][String protocol][u8 0][u8 0][String errName][u16 code][u32 callID]
//!
//! String = [u16 len incl. NUL][bytes][NUL]. GUID = u32, u16, u16, then 4 x u16 (all little-endian): Windows
//! order except that each byte pair of the last 8 bytes is swapped. With plain Windows order the game sees a
//! different profile id, takes its player status for someone else's and treats the player as banned from
//! matchmaking. Many requests start their params with a type-version map: [u32 n] n x [String][u16].
//!
//! A response's method name is the request's plus "*": the client dispatcher compares against name + "*";
//! without it the reply counts as an unknown method and the game shows DE-005-03.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

pub const RESPONSE_SUFFIX: &str = "*";
pub const NOTIFICATION_METHOD: &str = "NotificationProtocol::ProcessNotificationEvent_V1";

/// Quazal writes the last 8 bytes of a GUID as four little-endian u16s: each byte pair is swapped against
/// Windows order. The same swap converts in both directions.
fn quazal_guid_order(b: &[u8; 16]) -> [u8; 16] {
    let mut out = *b;
    for i in (8..16).step_by(2) {
        out.swap(i, i + 1);
    }
    out
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("read past end ({}+{} > {})", self.pos, n, self.data.len());
        }
        let b = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn guid(&mut self) -> Result<Uuid> {
        let raw: [u8; 16] = self.take(16)?.try_into()?;
        Ok(Uuid::from_bytes_le(quazal_guid_order(&raw)))
    }

    pub fn string(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        if n == 0 {
            return Ok(String::new());
        }
        let raw = self.take(n)?;
        let end = raw.iter().rposition(|&c| c != 0).map(|i| i + 1).unwrap_or(0);
        Ok(String::from_utf8_lossy(&raw[..end]).into_owned())
    }

    pub fn strings(&mut self) -> Result<Vec<String>> {
        let n = self.u32()?;
        (0..n).map(|_| self.string()).collect()
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let b = &self.data[self.pos..];
        self.pos = self.data.len();
        b
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// A type-version map ([u32 n] n x [String name][u16 version]).
    pub fn type_map(&mut self) -> Result<Vec<(String, u16)>> {
        let n = self.u32()?;
        (0..n).map(|_| Ok((self.string()?, self.u16()?))).collect()
    }

    pub fn variant(&mut self) -> Result<Variant> {
        let t = self.u8()?;
        Ok(match t {
            V_NONE => Variant::None,
            V_INT64 => Variant::Int(self.i64()?),
            V_DOUBLE => Variant::Double(self.f64()?),
            V_BOOL => Variant::Bool(self.u8()? != 0),
            V_STRING => Variant::String(self.string()?),
            V_DATETIME => Variant::DateTime(self.u64()?),
            V_UINT64 => Variant::UInt(self.u64()?),
            _ => bail!("unknown Variant type {t}"),
        })
    }

    /// Map<String, Variant>, in order.
    pub fn variant_map(&mut self) -> Result<Vec<(String, Variant)>> {
        let n = self.u32()?;
        (0..n).map(|_| Ok((self.string()?, self.variant()?))).collect()
    }
}

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn guid(&mut self, g: &Uuid) -> &mut Self {
        self.buf.extend_from_slice(&quazal_guid_order(&g.to_bytes_le()));
        self
    }
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.u16((s.len() + 1) as u16);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        self
    }
    pub fn strings(&mut self, items: &[String]) -> &mut Self {
        self.u32(items.len() as u32);
        for s in items {
            self.string(s);
        }
        self
    }
    pub fn variant(&mut self, v: &Variant) -> &mut Self {
        match v {
            Variant::None => self.u8(V_NONE),
            Variant::Int(i) => self.u8(V_INT64).i64(*i),
            Variant::Double(f) => self.u8(V_DOUBLE).f64(*f),
            Variant::Bool(b) => self.u8(V_BOOL).bool(*b),
            Variant::String(s) => self.u8(V_STRING).string(s),
            Variant::DateTime(t) => self.u8(V_DATETIME).u64(*t),
            Variant::UInt(u) => self.u8(V_UINT64).u64(*u),
        }
    }
    pub fn bytes(&self) -> Vec<u8> {
        self.buf.clone()
    }
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

// Quazal Variant: u8 type + value. Seen in Steep: 1 int64, 2 double, 4 String, 6 uint64 (stat events and stat
// groups). 3 bool and 5 DateTime follow the Quazal definition but have not been observed yet.
pub const V_NONE: u8 = 0;
pub const V_INT64: u8 = 1;
pub const V_DOUBLE: u8 = 2;
pub const V_BOOL: u8 = 3;
pub const V_STRING: u8 = 4;
pub const V_DATETIME: u8 = 5;
pub const V_UINT64: u8 = 6;

#[derive(Clone, Debug, PartialEq)]
pub enum Variant {
    None,
    Int(i64),
    Double(f64),
    Bool(bool),
    String(String),
    DateTime(u64),
    UInt(u64),
}

impl Variant {
    pub fn kind(&self) -> u8 {
        match self {
            Variant::None => V_NONE,
            Variant::Int(_) => V_INT64,
            Variant::Double(_) => V_DOUBLE,
            Variant::Bool(_) => V_BOOL,
            Variant::String(_) => V_STRING,
            Variant::DateTime(_) => V_DATETIME,
            Variant::UInt(_) => V_UINT64,
        }
    }

    /// As an integer, when it is one (bools count, as in Python).
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Variant::Int(i) => Some(*i as i128),
            Variant::UInt(u) | Variant::DateTime(u) => Some(*u as i128),
            Variant::Bool(b) => Some(*b as i128),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Variant::Double(f) => Some(*f),
            other => other.as_i128().map(|i| i as f64),
        }
    }

    /// The value as it would print in Python's str() (for stat key templates like "level_stat:{id}").
    pub fn to_text(&self) -> String {
        match self {
            Variant::None => "None".into(),
            Variant::Int(i) => i.to_string(),
            Variant::UInt(u) | Variant::DateTime(u) => u.to_string(),
            Variant::Double(f) => crate::util::py_float(*f),
            Variant::Bool(b) => if *b { "True".into() } else { "False".into() },
            Variant::String(s) => s.clone(),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Variant::None => Value::Null,
            Variant::Int(i) => json!(i),
            Variant::UInt(u) | Variant::DateTime(u) => json!(u),
            Variant::Double(f) => json!(f),
            Variant::Bool(b) => json!(b),
            Variant::String(s) => json!(s),
        }
    }
}

pub fn variants_to_json(map: &[(String, Variant)]) -> Map<String, Value> {
    map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()
}

#[derive(Clone, Debug)]
pub struct Message {
    pub protocol: String,
    pub is_request: bool,
    pub call_id: u32,
    /// "Proto::Method_V1" (responses carry the "*" suffix)
    pub method: String,
    pub error: Option<(String, u16)>,
    /// params (request) / outputs (response)
    pub body: Vec<u8>,
}

pub fn parse(data: &[u8]) -> Result<Message> {
    let mut r = Reader::new(data);
    let size = r.u32()? as usize;
    if size != data.len() - 4 && size != data.len() {
        bail!("RMC size field {size} does not match message length {}", data.len());
    }
    let protocol = r.string()?;
    if r.u8()? == 1 {
        let call_id = r.u32()?;
        let method = r.string()?;
        return Ok(Message { protocol, is_request: true, call_id, method, error: None, body: r.rest().to_vec() });
    }
    if r.u8()? == 1 {
        let call_id = r.u32()?;
        let method = r.string()?;
        return Ok(Message { protocol, is_request: false, call_id, method, error: None, body: r.rest().to_vec() });
    }
    let err = (r.string()?, r.u16()?);
    let call_id = r.u32()?;
    Ok(Message { protocol, is_request: false, call_id, method: String::new(), error: Some(err), body: vec![] })
}

fn finish(w: &Writer) -> Vec<u8> {
    let mut out = (w.buf.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&w.buf);
    out
}

pub fn build_request(protocol: &str, call_id: u32, method: &str, params: &[u8]) -> Vec<u8> {
    finish(Writer::new().string(protocol).u8(1).u32(call_id).string(method).raw(params))
}

pub fn build_response(protocol: &str, call_id: u32, method: &str, outputs: &[u8]) -> Vec<u8> {
    finish(Writer::new().string(protocol).u8(0).u8(1).u32(call_id)
        .string(&format!("{method}{RESPONSE_SUFFIX}")).raw(outputs))
}

pub fn build_error(protocol: &str, call_id: u32, name: &str, code: u16) -> Vec<u8> {
    finish(Writer::new().string(protocol).u8(0).u8(0).string(name).u16(code).u32(call_id))
}

/// Params of a server-pushed NotificationProtocol::ProcessNotificationEvent_V1 (call id 0), as Ubisoft's
/// server sent them: type map {NotificationEvent: 1}, 14 zero bytes, u16 2, String JSON {"code", "params",
/// "facility"} (Python's default json.dumps spacing). The zero bytes and the 2 are copied as recorded.
pub fn notification_event(facility: &str, code: i64, params: Map<String, Value>) -> Vec<u8> {
    let mut obj = Map::new();
    obj.insert("code".into(), json!(code));
    obj.insert("params".into(), Value::Object(params));
    obj.insert("facility".into(), json!(facility));
    let text = crate::util::dumps(&Value::Object(obj));
    Writer::new().u32(1).string("NotificationEvent").u16(1).raw(&[0u8; 14]).u16(2).string(&text).bytes()
}

/// Quazal DateTime packing (verified against a captured GetGameStorageTime reply).
pub fn quazal_datetime(t: DateTime<Utc>) -> u64 {
    t.second() as u64 | (t.minute() as u64) << 6 | (t.hour() as u64) << 12 | (t.day() as u64) << 17
        | (t.month() as u64) << 22 | (t.year() as u64) << 26
}

pub fn parse_guid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|e| anyhow!("bad GUID {text:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_round_trip_in_quazal_order() {
        let g = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let bytes = Writer::new().guid(&g).bytes();
        assert_eq!(hex::encode(&bytes), "33221100554477669988bbaaddccffee");
        assert_eq!(Reader::new(&bytes).guid().unwrap(), g);
    }

    #[test]
    fn messages_round_trip() {
        let req = build_request("LoginProtocol", 7, "LoginProtocol::Register_V1", b"\x01\x02");
        let m = parse(&req).unwrap();
        assert!(m.is_request && m.call_id == 7 && m.method == "LoginProtocol::Register_V1" && m.body == b"\x01\x02");
        let resp = parse(&build_response("P", 9, "P::M_V1", b"x")).unwrap();
        assert!(!resp.is_request && resp.method == "P::M_V1*" && resp.body == b"x");
        let err = parse(&build_error("P", 3, "Core", 2)).unwrap();
        assert_eq!(err.error, Some(("Core".into(), 2)));
        assert_eq!(err.call_id, 3);
    }

    #[test]
    fn notification_text_has_python_spacing() {
        let mut p = Map::new();
        p.insert("session_id".into(), json!(5));
        let raw = notification_event("Matchmaking", 1, p);
        let mut r = Reader::new(&raw);
        assert_eq!(r.type_map().unwrap(), vec![("NotificationEvent".into(), 1)]);
        r.take(14).unwrap();
        assert_eq!(r.u16().unwrap(), 2);
        assert_eq!(r.string().unwrap(), r#"{"code": 1, "params": {"session_id": 5}, "facility": "Matchmaking"}"#);
    }
}

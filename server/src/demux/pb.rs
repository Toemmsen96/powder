//! Minimal protobuf wire format: enough to read and write Ubisoft Connect's demux messages.
//!
//! No schema compiler: a message is a list of fields `F(number, value)`; a value is a varint, bytes
//! (length-delimited: strings, sub-messages), a sub-message built on the fly, or fixed32/64 kept raw so a
//! decode/encode round trip is lossless.

use anyhow::{bail, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Int(u64),
    Bytes(Vec<u8>),
    Msg(Vec<F>),
    Fixed32([u8; 4]),
    Fixed64([u8; 8]),
}

#[derive(Clone, Debug, PartialEq)]
pub struct F(pub u32, pub V);

impl F {
    pub fn int(num: u32, v: impl Into<i128>) -> F {
        F(num, V::Int(v.into() as u64))
    }
    pub fn str(num: u32, s: &str) -> F {
        F(num, V::Bytes(s.as_bytes().to_vec()))
    }
    pub fn bytes(num: u32, b: &[u8]) -> F {
        F(num, V::Bytes(b.to_vec()))
    }
    pub fn msg(num: u32, fields: Vec<F>) -> F {
        F(num, V::Msg(fields))
    }
}

const VARINT: u64 = 0;
const FIXED64: u64 = 1;
const LEN: u64 = 2;
const FIXED32: u64 = 5;

pub fn read_varint(b: &[u8], mut i: usize) -> Result<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let Some(&x) = b.get(i) else { bail!("truncated varint") };
        i += 1;
        result |= ((x & 0x7F) as u64) << shift;
        if x < 0x80 {
            return Ok((result, i));
        }
        shift += 7;
        if shift > 63 {
            bail!("varint too long");
        }
    }
}

pub fn varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n != 0 {
            out.push(b | 0x80);
        } else {
            out.push(b);
            return;
        }
    }
}

pub fn decode(b: &[u8]) -> Result<Vec<F>> {
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let (key, next) = read_varint(b, i)?;
        i = next;
        let (num, wt) = ((key >> 3) as u32, key & 7);
        if num == 0 {
            bail!("field number 0");
        }
        let value = match wt {
            VARINT => {
                let (v, next) = read_varint(b, i)?;
                i = next;
                V::Int(v)
            }
            LEN => {
                let (n, next) = read_varint(b, i)?;
                let (start, end) = (next, end_of(next, n, b.len(), "truncated length-delimited field")?);
                i = end;
                V::Bytes(b[start..end].to_vec())
            }
            FIXED64 => {
                let (start, end) = (i, end_of(i, 8, b.len(), "truncated field")?);
                i = end;
                V::Fixed64(b[start..end].try_into()?)
            }
            FIXED32 => {
                let (start, end) = (i, end_of(i, 4, b.len(), "truncated field")?);
                i = end;
                V::Fixed32(b[start..end].try_into()?)
            }
            _ => bail!("unsupported wire type {wt}"),
        };
        out.push(F(num, value));
    }
    Ok(out)
}

/// `start + n` if that is within `len`: a length read off the wire may be anything (a huge varint must not wrap).
fn end_of(start: usize, n: u64, len: usize, what: &str) -> Result<usize> {
    match usize::try_from(n).ok().and_then(|n| start.checked_add(n)) {
        Some(end) if end <= len => Ok(end),
        _ => bail!("{what}"),
    }
}

/// The fields of `b`, or None when it isn't a message.
pub fn try_decode(b: &[u8]) -> Option<Vec<F>> {
    if b.is_empty() {
        return Some(vec![]);
    }
    decode(b).ok()
}

pub fn encode(fields: &[F]) -> Vec<u8> {
    let mut out = vec![];
    encode_into(fields, &mut out);
    out
}

fn encode_into(fields: &[F], out: &mut Vec<u8>) {
    for F(num, value) in fields {
        let num = *num as u64;
        match value {
            V::Int(v) => {
                varint(num << 3 | VARINT, out);
                varint(*v, out);
            }
            V::Bytes(b) => {
                varint(num << 3 | LEN, out);
                varint(b.len() as u64, out);
                out.extend_from_slice(b);
            }
            V::Msg(sub) => {
                let inner = encode(sub);
                varint(num << 3 | LEN, out);
                varint(inner.len() as u64, out);
                out.extend_from_slice(&inner);
            }
            V::Fixed32(b) => {
                varint(num << 3 | FIXED32, out);
                out.extend_from_slice(b);
            }
            V::Fixed64(b) => {
                varint(num << 3 | FIXED64, out);
                out.extend_from_slice(b);
            }
        }
    }
}

pub fn first(fields: &[F], num: u32) -> Option<&V> {
    fields.iter().find(|f| f.0 == num).map(|f| &f.1)
}

pub fn int(fields: &[F], num: u32) -> Option<u64> {
    match first(fields, num) {
        Some(V::Int(v)) => Some(*v),
        _ => None,
    }
}

pub fn bytes(fields: &[F], num: u32) -> Option<&[u8]> {
    match first(fields, num) {
        Some(V::Bytes(b)) => Some(b),
        _ => None,
    }
}

/// A length-delimited field decoded as a sub-message (None if absent or not a message).
pub fn sub(fields: &[F], num: u32) -> Option<Vec<F>> {
    bytes(fields, num).and_then(try_decode)
}

pub fn text(fields: &[F], num: u32) -> String {
    bytes(fields, num).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
}

/// Copy of `fields` with the first field `num` set to `value` (appended if absent).
pub fn replace(fields: &[F], num: u32, value: V) -> Vec<F> {
    let mut out = Vec::with_capacity(fields.len() + 1);
    let mut done = false;
    for f in fields {
        if f.0 == num && !done {
            out.push(F(num, value.clone()));
            done = true;
        } else {
            out.push(f.clone());
        }
    }
    if !done {
        out.push(F(num, value));
    }
    out
}

/// 4-byte big-endian length prefix (outer stream and the per-service sub-streams).
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// The payload of one framed message (service data carries exactly one).
pub fn unframe(data: &[u8]) -> Result<&[u8]> {
    if data.len() < 4 || u32::from_be_bytes(data[..4].try_into()?) as usize != data.len() - 4 {
        bail!("bad frame length");
    }
    Ok(&data[4..])
}

/// Field structure of a message for the log: numbers, small ints, short strings; long strings only by length.
pub fn outline(value: &V, depth: usize) -> String {
    let printable = |b: &[u8]| !b.is_empty() && b.iter().all(|c| (32..127).contains(c));
    match value {
        V::Int(i) => i.to_string(),
        V::Fixed32(b) => format!("<fixed32 {}>", hex::encode(b)),
        V::Fixed64(b) => format!("<fixed64 {}>", hex::encode(b)),
        V::Msg(f) => outline(&V::Bytes(encode(f)), depth),
        V::Bytes(b) => {
            if printable(b) {
                return if b.len() <= 40 { format!("{:?}", String::from_utf8_lossy(b)) } else { format!("<text {}>", b.len()) };
            }
            match if depth < 6 { try_decode(b) } else { None } {
                Some(fields) if !fields.is_empty() => {
                    let parts: Vec<String> = fields.iter().map(|F(n, v)| format!("{n}:{}", outline(v, depth + 1))).collect();
                    format!("{{{}}}", parts.join(" "))
                }
                _ => format!("<{}B>", b.len()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_helpers() {
        let msg = vec![F::int(1, 150), F::str(2, "dmx"), F::msg(3, vec![F::int(1, -1i64)]),
                       F(4, V::Fixed32([1, 2, 3, 4])), F(5, V::Fixed64([9; 8]))];
        let raw = encode(&msg);
        let back = decode(&raw).unwrap();
        assert_eq!(int(&back, 1), Some(150));
        assert_eq!(text(&back, 2), "dmx");
        assert_eq!(int(&sub(&back, 3).unwrap(), 1), Some(u64::MAX));
        assert_eq!(encode(&back), raw, "lossless");
        assert_eq!(&encode(&[F::int(1, 150)]), &[0x08, 0x96, 0x01]);
        let replaced = replace(&back, 1, V::Int(7));
        assert_eq!(int(&replaced, 1), Some(7));
        assert_eq!(unframe(&frame(b"abc")).unwrap(), b"abc");
        assert!(try_decode(&[0x0a, 0x05, 0x01]).is_none());
    }

    /// A length that would overflow `next + n` (it wrapped in release builds, then the slice panicked).
    #[test]
    fn huge_lengths_are_errors_not_panics() {
        for n in [u64::MAX, u64::MAX - 1, usize::MAX as u64 - 1] {
            let mut raw = vec![0x0a];
            varint(n, &mut raw);
            raw.push(0x01);
            assert!(decode(&raw).unwrap_err().to_string().contains("truncated"), "{n}");
            assert!(try_decode(&raw).is_none());
        }
        assert!(decode(&[0x09, 1, 2, 3]).is_err() && decode(&[0x0d, 1, 2, 3]).is_err(), "short fixed64 / fixed32");
    }
}

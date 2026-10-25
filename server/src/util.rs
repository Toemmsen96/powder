//! Small shared helpers: UbiServices timestamps and JSON written the way Ubisoft's (Python-shaped) replies are.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;
use std::fmt::Write as _;

/// UbiServices timestamp, e.g. 2026-10-04T17:23:02.892Z
pub fn iso_now() -> String {
    iso_at(Utc::now())
}

pub fn iso_in(delta_s: i64) -> String {
    iso_at(Utc::now() + Duration::seconds(delta_s))
}

pub fn iso_at(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn unix_now() -> f64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    d.as_secs_f64()
}

/// A URL-safe random token of `n` random bytes (like Python's secrets.token_urlsafe).
pub fn token_urlsafe(n: usize) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(n))
}

pub fn token_hex(n: usize) -> String {
    hex::encode(random_bytes(n))
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

pub fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text.trim()).ok()
}

/// JSON as Python's `json.dumps` writes it (separators ", " and ": ", non-ASCII escaped): what the game
/// got from Ubisoft's servers, byte for byte where that was recorded (e.g. NEX notification texts).
pub fn dumps(v: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, v, ", ", ": ");
    out
}

/// Compact JSON (separators "," and ":"), still ASCII-only.
pub fn dumps_compact(v: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, v, ",", ":");
    out
}

fn write_json(out: &mut String, v: &Value, item_sep: &str, key_sep: &str) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else {
                out.push_str(&py_float(n.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(s) => write_str(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_json(out, item, item_sep, key_sep);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_str(out, k);
                out.push_str(key_sep);
                write_json(out, item, item_sep, key_sep);
            }
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A float as Python's repr() writes it: shortest round-trip digits, positional between 1e-4 and 1e16,
/// otherwise scientific with a signed, at least two-digit exponent ("1e+16", "1.5e-05").
pub fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{f:e}"); // shortest digits: "1.5e-5", "-1e16"
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        let s = format!("{f}");
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_like_python() {
        let v = json!({"code": 1, "params": {"a": "x", "b": [1, 2.5]}, "facility": "Matchmaking", "n": null});
        assert_eq!(dumps(&v), r#"{"code": 1, "params": {"a": "x", "b": [1, 2.5]}, "facility": "Matchmaking", "n": null}"#);
        assert_eq!(dumps(&json!("é\u{1F600}\"")), r#""é😀\"""#);
        assert_eq!(dumps_compact(&json!({"a": [1, true]})), r#"{"a":[1,true]}"#);
    }

    #[test]
    fn floats_like_python_repr() {
        assert_eq!(py_float(1.0), "1.0");
        assert_eq!(py_float(0.1), "0.1");
        assert_eq!(py_float(123456.789), "123456.789");
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(1.5e-5), "1.5e-05");
        assert_eq!(py_float(0.0001), "0.0001");
        assert_eq!(py_float(-2.5e20), "-2.5e+20");
        assert_eq!(py_float(1e15), "1000000000000000.0");
    }
}

//! Steep NEX / Quazal Rendez-Vous PRUDP (classic "V0") packet codec, derived from the game's own traffic.
//!
//! Packet layout (little-endian):
//!   off size field
//!    0   1   source stream byte   (0x3f = client/game side, 0x31 = server side)
//!    1   1   dest   stream byte
//!    2   1   type_flags           type = b & 0x07 ; flags = b >> 3
//!    3   1   session_id
//!    4   4   signature            = the peer's connection_signature (0 in SYN)
//!    8   2   sequence_id          u16 LE
//!   --- by type ---
//!   SYN / CONNECT : +4  connection_signature  (this side's own; 0 in the client SYN)
//!   DATA          : +1  fragment_id
//!   --- then ---
//!        N   payload              (encrypted for CONNECT/DATA; empty for SYN & pure ACK)
//!        4   checksum
//!
//! CONNECT key exchange (flag 0x80): if ACK, [u16 len][ECDSA signature], then [u16 len][ECK1 ECDH public blob].
//! Then, for any type, if HAS_SIZE (0x40): [u16 payload size]; otherwise the payload runs to the checksum.

use anyhow::{bail, Result};

pub const SYN: u8 = 0;
pub const CONNECT: u8 = 1;
pub const DATA: u8 = 2;
pub const DISCONNECT: u8 = 3;
pub const PING: u8 = 4;

// flag bits as they sit in the type_flags byte (already shifted << 3)
pub const FLAG_ACK: u8 = 0x08;
pub const FLAG_RELIABLE: u8 = 0x10;
pub const FLAG_NEED_ACK: u8 = 0x20;
pub const FLAG_HAS_SIZE: u8 = 0x40;
pub const FLAG_KEY_EXCHANGE: u8 = 0x80;

/// Checksum base derived from Steep's Quazal access key; identical in both directions, so valid checksums
/// can be made without knowing the access-key string.
pub const CHECKSUM_BASE: u32 = 0xB7E6_89A7;

pub const CLIENT_STREAM: u8 = 0x3F;
pub const SERVER_STREAM: u8 = 0x31;

/// checksum32 = CHECKSUM_BASE + sum of the u32 LE words of the packet (last word zero-padded).
pub fn checksum(data: &[u8]) -> [u8; 4] {
    let mut total = CHECKSUM_BASE;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        total = total.wrapping_add(u32::from_le_bytes(word));
    }
    total.to_le_bytes()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub source: u8,
    pub dest: u8,
    pub kind: u8,
    /// flag bits as they sit in the type_flags byte
    pub flags: u8,
    pub session_id: u8,
    pub signature: [u8; 4],
    pub sequence_id: u16,
    pub connection_signature: [u8; 4],
    pub fragment_id: u8,
    pub key_signature: Vec<u8>,
    pub key_blob: Vec<u8>,
    pub payload: Vec<u8>,
}

impl Default for Packet {
    fn default() -> Self {
        Self { source: CLIENT_STREAM, dest: SERVER_STREAM, kind: DATA, flags: 0, session_id: 0, signature: [0; 4],
               sequence_id: 0, connection_signature: [0; 4], fragment_id: 0, key_signature: vec![], key_blob: vec![],
               payload: vec![] }
    }
}

impl Packet {
    pub fn type_flags(&self) -> u8 {
        (self.kind & 0x07) | (self.flags & !0x07)
    }

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut pkt = vec![self.source, self.dest, self.type_flags(), self.session_id];
        pkt.extend_from_slice(&self.signature);
        pkt.extend_from_slice(&self.sequence_id.to_le_bytes());
        if self.kind == SYN || self.kind == CONNECT {
            pkt.extend_from_slice(&self.connection_signature);
        }
        if self.kind == CONNECT && self.has(FLAG_KEY_EXCHANGE) {
            if self.has(FLAG_ACK) {
                pkt.extend_from_slice(&(self.key_signature.len() as u16).to_le_bytes());
                pkt.extend_from_slice(&self.key_signature);
            }
            pkt.extend_from_slice(&(self.key_blob.len() as u16).to_le_bytes());
            pkt.extend_from_slice(&self.key_blob);
        }
        if self.kind == DATA {
            pkt.push(self.fragment_id);
        }
        if self.has(FLAG_HAS_SIZE) {
            pkt.extend_from_slice(&(self.payload.len() as u16).to_le_bytes());
        }
        pkt.extend_from_slice(&self.payload);
        let sum = checksum(&pkt);
        pkt.extend_from_slice(&sum);
        pkt
    }

    pub fn decode(raw: &[u8]) -> Result<Packet> {
        if raw.len() < 14 {
            bail!("PRUDP packet too short: {} bytes", raw.len());
        }
        let (body, got) = raw.split_at(raw.len() - 4);
        if got != checksum(body) {
            bail!("bad checksum");
        }
        let mut p = Packet {
            source: body[0], dest: body[1], kind: body[2] & 0x07, flags: body[2] & !0x07, session_id: body[3],
            signature: body[4..8].try_into()?, sequence_id: u16::from_le_bytes([body[8], body[9]]),
            ..Default::default()
        };
        let mut off = 10;
        let need = |off: usize, n: usize| -> Result<()> {
            if off + n > body.len() { bail!("PRUDP packet truncated") } else { Ok(()) }
        };
        if p.kind == SYN || p.kind == CONNECT {
            need(off, 4)?;
            p.connection_signature = body[off..off + 4].try_into()?;
            off += 4;
        }
        let field = |off: &mut usize| -> Result<Vec<u8>> {
            need(*off, 2)?;
            let n = u16::from_le_bytes([body[*off], body[*off + 1]]) as usize;
            need(*off + 2, n)?;
            let v = body[*off + 2..*off + 2 + n].to_vec();
            *off += 2 + n;
            Ok(v)
        };
        if p.kind == CONNECT && p.has(FLAG_KEY_EXCHANGE) {
            if p.has(FLAG_ACK) {
                p.key_signature = field(&mut off)?;
            }
            p.key_blob = field(&mut off)?;
        }
        if p.kind == DATA {
            need(off, 1)?;
            p.fragment_id = body[off];
            off += 1;
        }
        p.payload = if p.has(FLAG_HAS_SIZE) { field(&mut off)? } else { body[off..].to_vec() };
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_shape() {
        let shapes = [
            Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() },
            Packet { kind: CONNECT, flags: FLAG_ACK | FLAG_KEY_EXCHANGE, key_signature: vec![1; 64],
                     key_blob: vec![2; 72], payload: vec![3; 32], connection_signature: [9, 8, 7, 6], ..Default::default() },
            Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: 513, fragment_id: 2,
                     payload: b"hello".to_vec(), ..Default::default() },
            Packet { kind: PING, flags: FLAG_HAS_SIZE, payload: vec![5, 6], ..Default::default() },
        ];
        for p in shapes {
            assert_eq!(Packet::decode(&p.encode()).unwrap(), p);
        }
        let mut raw = Packet::default().encode();
        raw[5] ^= 1;
        assert!(Packet::decode(&raw).is_err(), "checksum is checked");
    }
}

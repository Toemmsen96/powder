//! Steep NEX secure-PRUDP crypto (server side), reconstructed from the game binary.
//!
//! Handshake (PRUDP CONNECT with flag 0x80):
//!   game  -> CONNECT      key_blob = ECK1 (BCrypt ECDH P-256 public blob) of the game's ephemeral key
//!   server-> CONNECT-ACK  key_signature = ECDSA-P256(SHA-256(server ECK1 blob)) by the key whose ECS1 public
//!                         blob is served as `public_key` in /rendezvous/configuration
//!                         key_blob = server ephemeral ECK1; payload = encrypt_payload(key, seq, b"")
//!   session key = SHA-1(ECDH shared X coordinate, big-endian)[:16]
//!
//! Payload (both directions, same key):
//!   wire  = IV(16 random) || AES-128-CBC-PKCS7(key, IV, plain)
//!   plain = [u8 zlib ratio, 0 = raw] + (inner | zlib(inner)),  inner = payload + u16 LE sequence_id
//! The game compresses its payloads; the sequence trailer sits inside the compressed data.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use anyhow::{anyhow, bail, Context, Result};
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use p256::{EncodedPoint, PublicKey, SecretKey};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::path::Path;

pub const ECK1: &[u8; 4] = b"ECK1"; // BCRYPT_ECDH_PUBLIC_P256_MAGIC
pub const ECS1: &[u8; 4] = b"ECS1"; // BCRYPT_ECDSA_PUBLIC_P256_MAGIC

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

pub fn to_blob(public: &PublicKey, magic: &[u8; 4]) -> Vec<u8> {
    let point = public.to_encoded_point(false);
    let mut blob = magic.to_vec();
    blob.extend_from_slice(&32u32.to_le_bytes());
    blob.extend_from_slice(point.x().expect("uncompressed"));
    blob.extend_from_slice(point.y().expect("uncompressed"));
    blob
}

pub fn from_blob(blob: &[u8]) -> Result<PublicKey> {
    if blob.len() != 72 || !(&blob[..4] == ECK1 || &blob[..4] == ECS1) || blob[4..8] != [0x20, 0, 0, 0] {
        bail!("not a BCrypt P-256 public blob: {} len={}", hex::encode(&blob[..blob.len().min(8)]), blob.len());
    }
    let point = EncodedPoint::from_affine_coordinates(blob[8..40].into(), blob[40..72].into(), false);
    Option::from(PublicKey::from_encoded_point(&point)).ok_or_else(|| anyhow!("public key not on the curve"))
}

/// The server's ECDSA key (PRIVATE: never publish it), created on first run. Same PKCS#8 PEM file as before.
pub fn load_or_create_signing_key(path: &Path) -> Result<SigningKey> {
    if path.exists() {
        let pem = std::fs::read_to_string(path)?;
        let secret = SecretKey::from_pkcs8_pem(&pem).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        return Ok(SigningKey::from(secret));
    }
    let secret = SecretKey::random(&mut rand::rngs::OsRng);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pem = secret.to_pkcs8_pem(LineEnding::LF).map_err(|e| anyhow!("{e}"))?;
    std::fs::write(path, pem.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(SigningKey::from(secret))
}

/// Value for `public_key` in /rendezvous/configuration.
pub fn config_public_key(key: &SigningKey) -> String {
    let public = PublicKey::from(key.verifying_key());
    crate::util::b64(&to_blob(&public, ECS1))
}

pub fn sign_blob(key: &SigningKey, blob: &[u8]) -> Vec<u8> {
    let sig: Signature = key.sign(blob);
    sig.to_bytes().to_vec()
}

pub fn verify_blob(public: &PublicKey, blob: &[u8], sig: &[u8]) -> bool {
    let Ok(sig) = Signature::from_slice(sig) else { return false };
    VerifyingKey::from(public).verify(blob, &sig).is_ok()
}

fn session_key(shared_x: &[u8]) -> [u8; 16] {
    let digest = Sha1::digest(shared_x);
    digest[..16].try_into().expect("20-byte digest")
}

/// -> (server blob, signature, session key) for the CONNECT-ACK.
pub fn server_handshake(signing: &SigningKey, client_blob: &[u8]) -> Result<(Vec<u8>, Vec<u8>, [u8; 16])> {
    let peer = from_blob(client_blob)?;
    let eph = p256::ecdh::EphemeralSecret::random(&mut rand::rngs::OsRng);
    let blob = to_blob(&eph.public_key(), ECK1);
    let shared = eph.diffie_hellman(&peer);
    Ok((blob.clone(), sign_blob(signing, &blob), session_key(shared.raw_secret_bytes())))
}

/// The game's side (for tests and tools): -> (client blob, ephemeral secret).
pub fn client_key() -> (Vec<u8>, p256::ecdh::EphemeralSecret) {
    let eph = p256::ecdh::EphemeralSecret::random(&mut rand::rngs::OsRng);
    (to_blob(&eph.public_key(), ECK1), eph)
}

pub fn client_session_key(eph: &p256::ecdh::EphemeralSecret, server_blob: &[u8]) -> Result<[u8; 16]> {
    Ok(session_key(eph.diffie_hellman(&from_blob(server_blob)?).raw_secret_bytes()))
}

pub fn encrypt_payload(key: &[u8; 16], seq: u16, payload: &[u8], compress: bool) -> Vec<u8> {
    let mut inner = payload.to_vec();
    inner.extend_from_slice(&seq.to_le_bytes());
    let plain = if compress {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&inner).expect("in memory");
        let z = z.finish().expect("in memory");
        let ratio = inner.len().div_ceil(z.len()).clamp(1, 255) as u8;
        [&[ratio][..], &z].concat()
    } else {
        [&[0u8][..], &inner].concat()
    };
    let iv: [u8; 16] = crate::util::random_bytes(16).try_into().expect("16 bytes");
    let ct = Aes128CbcEnc::new(key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain);
    [&iv[..], &ct].concat()
}

pub fn decrypt_payload(key: &[u8; 16], data: &[u8], seq: Option<u16>) -> Result<Vec<u8>> {
    if data.len() < 32 || data.len() % 16 != 0 {
        bail!("encrypted payload of {} bytes", data.len());
    }
    let (iv, ct) = data.split_at(16);
    let plain = Aes128CbcDec::new(key.into(), iv.into()).decrypt_padded_vec_mut::<Pkcs7>(ct)
        .map_err(|_| anyhow!("bad padding"))?;
    let (ratio, body) = plain.split_first().ok_or_else(|| anyhow!("empty payload"))?;
    let inner = if *ratio == 0 {
        body.to_vec()
    } else {
        let mut out = vec![];
        flate2::read::ZlibDecoder::new(body).read_to_end(&mut out)?;
        out
    };
    if inner.len() < 2 {
        bail!("payload without sequence trailer");
    }
    let (data, trailer) = inner.split_at(inner.len() - 2);
    let trailer = u16::from_le_bytes([trailer[0], trailer[1]]);
    if let Some(seq) = seq {
        if trailer != seq {
            bail!("seq trailer {trailer} != packet seq {seq}");
        }
    }
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_and_payloads() {
        let signing = SigningKey::random(&mut rand::rngs::OsRng);
        let (client_blob, eph) = client_key();
        let (server_blob, sig, key) = server_handshake(&signing, &client_blob).unwrap();
        let config = crate::util::unb64(&config_public_key(&signing)).unwrap();
        assert!(verify_blob(&from_blob(&config).unwrap(), &server_blob, &sig));
        assert_eq!(client_session_key(&eph, &server_blob).unwrap(), key);
        for compress in [false, true] {
            let wire = encrypt_payload(&key, 77, b"hello hello hello hello", compress);
            assert_eq!(decrypt_payload(&key, &wire, Some(77)).unwrap(), b"hello hello hello hello");
            assert!(decrypt_payload(&key, &wire, Some(78)).is_err());
        }
    }

    #[test]
    fn signing_key_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.pem");
        let a = load_or_create_signing_key(&path).unwrap();
        let b = load_or_create_signing_key(&path).unwrap();
        assert_eq!(config_public_key(&a), config_public_key(&b));
    }
}

"""
Steep NEX secure-PRUDP crypto (server side). Reconstructed from the binary — see notes/nex-recon.md.

Handshake (PRUDP CONNECT with flag 0x80):
  game  -> CONNECT      key_blob = ECK1 (BCrypt ECDH P-256 public blob) of the game's ephemeral key
  server-> CONNECT-ACK  key_signature = ECDSA-P256(SHA-256(server ECK1 blob)) by the key whose ECS1
                        public blob is served as `public_key` in /rendezvous/configuration
                        key_blob = server ephemeral ECK1; payload = encrypt_payload(key, seq, b"")
  session key = SHA-1(ECDH shared X coordinate, big-endian)[:16]   (BCryptDeriveKey KDF "HASH")

Payload (stream types 2/3, both directions, same key; FUN_7ff661aff650 / FUN_7ff661b08c00):
  wire  = IV(16 random) || AES-128-CBC-PKCS7(key, IV, plain)
  plain = [u8 zlib ratio, 0 = raw] + (inner | zlib(inner)),  inner = payload + u16 LE sequence_id
  (receiver: decrypt -> decompress -> require trailing u16 == packet sequence_id, strip it)

LIVE-CONFIRMED (2026-10-04 run): the game accepts our signature + CONNECT-ACK, and its DATA decrypts
with SHA1(Z big-endian)[:16]. The game compresses its payloads; the seq trailer sits INSIDE the
compressed data (decrypt -> inflate -> check trailer).

Tests: server/tests/test_secure.py
"""

from __future__ import annotations

import base64
import hashlib
import logging
import os
import struct
import zlib
from pathlib import Path

from cryptography.hazmat.primitives import hashes, padding, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature, encode_dss_signature
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

log = logging.getLogger("powder.nex")

ECK1 = b"ECK1"  # BCRYPT_ECDH_PUBLIC_P256_MAGIC
ECS1 = b"ECS1"  # BCRYPT_ECDSA_PUBLIC_P256_MAGIC
MAX_INFLATED = 64 * 1024   # most a compressed payload may inflate to (the game's fragments are under 1 KB)


def to_blob(pub: ec.EllipticCurvePublicKey, magic: bytes) -> bytes:
    n = pub.public_numbers()
    return magic + struct.pack("<I", 32) + n.x.to_bytes(32, "big") + n.y.to_bytes(32, "big")


def from_blob(blob: bytes) -> ec.EllipticCurvePublicKey:
    if len(blob) != 72 or blob[:4] not in (ECK1, ECS1) or blob[4:8] != b"\x20\x00\x00\x00":
        raise ValueError(f"not a BCrypt P-256 public blob: {blob[:8].hex()} len={len(blob)}")
    x, y = int.from_bytes(blob[8:40], "big"), int.from_bytes(blob[40:72], "big")
    return ec.EllipticCurvePublicNumbers(x, y, ec.SECP256R1()).public_key()


def load_or_create_signing_key(path: Path) -> ec.EllipticCurvePrivateKey:
    """Load the server's ECDSA key (PRIVATE: never publish it), creating it on first run. On POSIX the file
    is the owner's only (0600; an existing file's mode is tightened when loaded)."""
    if path.exists():
        _private_mode(path)
        return serialization.load_pem_private_key(path.read_bytes(), password=None)
    key = ec.generate_private_key(ec.SECP256R1())
    path.parent.mkdir(parents=True, exist_ok=True)
    pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                            serialization.NoEncryption())
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0), 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(pem)
    return key


def _private_mode(path: Path) -> None:
    """Make a private file readable by its owner only (POSIX; on Windows the folder's ACL decides)."""
    if os.name != "posix":
        return
    mode = path.stat().st_mode
    if mode & 0o077:
        path.chmod(0o600)
        log.info("%s: was readable by others (mode %o), now 0600", path, mode & 0o777)


def config_public_key(signing_key: ec.EllipticCurvePrivateKey) -> str:
    """Value for `public_key` in /rendezvous/configuration."""
    return base64.b64encode(to_blob(signing_key.public_key(), ECS1)).decode()


def sign_blob(signing_key: ec.EllipticCurvePrivateKey, blob: bytes) -> bytes:
    r, s = decode_dss_signature(signing_key.sign(blob, ec.ECDSA(hashes.SHA256())))
    return r.to_bytes(32, "big") + s.to_bytes(32, "big")


def verify_blob(pub: ec.EllipticCurvePublicKey, blob: bytes, sig: bytes) -> bool:
    der = encode_dss_signature(int.from_bytes(sig[:32], "big"), int.from_bytes(sig[32:], "big"))
    try:
        pub.verify(der, blob, ec.ECDSA(hashes.SHA256()))
        return True
    except Exception:  # noqa: BLE001
        return False


def session_key(own: ec.EllipticCurvePrivateKey, peer_blob: bytes) -> bytes:
    z = own.exchange(ec.ECDH(), from_blob(peer_blob))  # big-endian X coordinate
    return hashlib.sha1(z).digest()[:16]


def server_handshake(signing_key: ec.EllipticCurvePrivateKey, client_blob: bytes):
    """-> (server_blob, signature, key) for the CONNECT-ACK."""
    eph = ec.generate_private_key(ec.SECP256R1())
    blob = to_blob(eph.public_key(), ECK1)
    return blob, sign_blob(signing_key, blob), session_key(eph, client_blob)


def encrypt_payload(key: bytes, seq: int, payload: bytes, compress: bool = False) -> bytes:
    inner = payload + struct.pack("<H", seq & 0xFFFF)   # seq trailer sits inside the compressed data
    if compress:
        z = zlib.compress(inner)
        ratio = max(1, -(-len(inner) // len(z)))
        plain = bytes([min(ratio, 255)]) + z
    else:
        plain = b"\x00" + inner
    p = padding.PKCS7(128).padder()
    padded = p.update(plain) + p.finalize()
    iv = os.urandom(16)
    e = Cipher(algorithms.AES(key), modes.CBC(iv)).encryptor()
    return iv + e.update(padded) + e.finalize()


def decrypt_payload(key: bytes, data: bytes, seq: int | None = None) -> bytes:
    iv, ct = data[:16], data[16:]
    d = Cipher(algorithms.AES(key), modes.CBC(iv)).decryptor()
    u = padding.PKCS7(128).unpadder()
    plain = u.update(d.update(ct) + d.finalize()) + u.finalize()
    # game order (FUN_7ff661b08c00): decrypt -> decompress -> check/strip trailing u16 seq
    ratio, body = plain[0], plain[1:]
    if ratio == 0:
        inner = body
    else:   # bounded: a few KB of zlib could otherwise inflate to many MB
        z = zlib.decompressobj()
        inner = z.decompress(body, MAX_INFLATED + 1)
        if len(inner) > MAX_INFLATED:
            raise ValueError(f"compressed payload inflates past {MAX_INFLATED} bytes")
        if not z.eof:
            raise ValueError("truncated compressed payload")
    if len(inner) < 2:
        raise ValueError(f"payload of {len(inner)} bytes has no seq trailer")
    data, trailer = inner[:-2], struct.unpack("<H", inner[-2:])[0]
    if seq is not None and trailer != seq & 0xFFFF:
        raise ValueError(f"seq trailer {trailer} != packet seq {seq}")
    return data

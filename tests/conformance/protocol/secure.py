"""Steep NEX secure-PRUDP crypto, client side (vendored from the Python server's transport/nex/secure.py).

Handshake (PRUDP CONNECT with flag 0x80):
  game  -> CONNECT      key_blob = ECK1 (BCrypt ECDH P-256 public blob) of the game's ephemeral key
  server-> CONNECT-ACK  key_signature = ECDSA-P256(SHA-256(server ECK1 blob)) by the key whose ECS1
                        public blob is served as `public_key` in /rendezvous/configuration
                        key_blob = server ephemeral ECK1; payload = encrypt_payload(key, seq, b"")
  session key = SHA-1(ECDH shared X coordinate, big-endian)[:16]

Payload (both directions, same key):
  wire  = IV(16 random) || AES-128-CBC-PKCS7(key, IV, plain)
  plain = [u8 zlib ratio, 0 = raw] + (inner | zlib(inner)),  inner = payload + u16 LE sequence_id
"""
from __future__ import annotations

import hashlib
import os
import struct
import zlib

from cryptography.hazmat.primitives import hashes, padding
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import encode_dss_signature
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

ECK1 = b"ECK1"  # BCRYPT_ECDH_PUBLIC_P256_MAGIC
ECS1 = b"ECS1"  # BCRYPT_ECDSA_PUBLIC_P256_MAGIC


def to_blob(pub: ec.EllipticCurvePublicKey, magic: bytes) -> bytes:
    n = pub.public_numbers()
    return magic + struct.pack("<I", 32) + n.x.to_bytes(32, "big") + n.y.to_bytes(32, "big")


def from_blob(blob: bytes) -> ec.EllipticCurvePublicKey:
    if len(blob) != 72 or blob[:4] not in (ECK1, ECS1) or blob[4:8] != b"\x20\x00\x00\x00":
        raise ValueError(f"not a BCrypt P-256 public blob: {blob[:8].hex()} len={len(blob)}")
    x, y = int.from_bytes(blob[8:40], "big"), int.from_bytes(blob[40:72], "big")
    return ec.EllipticCurvePublicNumbers(x, y, ec.SECP256R1()).public_key()


def verify_blob(pub: ec.EllipticCurvePublicKey, blob: bytes, sig: bytes) -> bool:
    """`sig` is r||s (32 bytes each), as the CONNECT-ACK carries it."""
    if len(sig) != 64:
        return False
    der = encode_dss_signature(int.from_bytes(sig[:32], "big"), int.from_bytes(sig[32:], "big"))
    try:
        pub.verify(der, blob, ec.ECDSA(hashes.SHA256()))
        return True
    except Exception:  # noqa: BLE001
        return False


def session_key(own: ec.EllipticCurvePrivateKey, peer_blob: bytes) -> bytes:
    z = own.exchange(ec.ECDH(), from_blob(peer_blob))  # big-endian X coordinate
    return hashlib.sha1(z).digest()[:16]


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
    ratio, body = plain[0], plain[1:]
    inner = body if ratio == 0 else zlib.decompress(body)
    data, trailer = inner[:-2], struct.unpack("<H", inner[-2:])[0]
    if seq is not None and trailer != seq & 0xFFFF:
        raise ValueError(f"seq trailer {trailer} != packet seq {seq}")
    return data

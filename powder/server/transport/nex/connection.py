"""One PRUDP connection (one game client): reliable, ordered, fragmented, encrypted message stream.

Reliability rules (from the prototype, which the game accepted live):
  * every client packet with NEED_ACK is acked (DATA acks echo the fragment id)
  * client DATA is decrypted, buffered by sequence id and delivered in order; fragments are joined
    until fragment_id == 0 (the last fragment)
  * our DATA is sent RELIABLE|NEED_ACK in FRAGMENT_SIZE chunks and retransmitted until acked
Fixed vs. the prototype: 16-bit sequence ids compare wrap-safe; a connection that stops answering
(retransmits exhausted) or goes silent (IDLE_TIMEOUT_S) is closed instead of lingering forever.

Bounded: DATA REORDER_WINDOW or more ids ahead of the next one to deliver, or that does not decrypt, is
dropped without an ack (the game sends it again); a connection whose buffered message passes
MAX_MESSAGE_BYTES is closed. Client packets after the SYN carry the connection signature our SYN-ACK gave
(prudp.py's layout); `signed` checks it as `[server] nex_check_signatures` says (see CHECK_MODES).
"""
from __future__ import annotations

import logging
import os
import secrets
import time
from dataclasses import dataclass, field
from typing import Callable

from . import rmc_codec, secure
from .prudp import CONNECT, DATA, FLAG_ACK, FLAG_NEED_ACK, FLAG_RELIABLE, Packet

log = logging.getLogger("powder.nex")

FRAGMENT_SIZE = 700          # plaintext bytes per DATA fragment (the game sends ~730-780 B)
RETRANSMIT_S = 0.5
MAX_TRIES = 12
IDLE_TIMEOUT_S = 60.0        # the game PINGs every few seconds while connected
REORDER_WINDOW = 256         # out-of-order DATA buffered ahead (256 of the game's fragments are ~180 KB)
MAX_MESSAGE_BYTES = 1024 * 1024   # one client message (+ what is buffered out of order); requests are a few KB

# [server] nex_check_signatures. A spoofed source address never sees our SYN-ACK, so a packet carrying its
# signature comes from the game. Not verified against the real game yet (only self-made test vectors):
#   "log"      never refuse; the first mismatch per connection is a warning ("wrong signature"), a match an info
#   "enforce"  a CONNECT, DATA, ACK, PING or DISCONNECT with another signature is ignored (as the Rust server)
#   "off"      no check
CHECK_MODES = ("log", "enforce", "off")


def seq_before(a: int, b: int) -> bool:
    """True if 16-bit sequence id `a` comes before `b` (wrap-safe)."""
    return ((a - b) & 0xFFFF) > 0x8000


@dataclass(eq=False)
class PrudpConnection:
    addr: tuple[str, int]
    sendto: Callable[[bytes, tuple], None]
    pid: int = 0                                  # our id for this connection (Register's connection id)
    did: int = field(default_factory=lambda: secrets.randbelow(0x7FFFFFFF) + 1)   # StationURL DID, never 0
    client_stream: int = 0x3F
    server_stream: int = 0x31
    server_sig: bytes = field(default_factory=lambda: os.urandom(4))
    session_id: int = field(default_factory=lambda: os.urandom(1)[0] or 1)
    client_sig: bytes = b"\x00" * 4
    key: bytes | None = None
    connect_ack: bytes | None = None              # cached, resent if the client repeats CONNECT
    expected_seq: int = 0                         # next client DATA sequence id to deliver
    pending: dict = field(default_factory=dict)   # client seq -> (fragment_id, plaintext)
    fragments: list = field(default_factory=list)
    buffered: int = 0                             # bytes in pending + fragments
    sig_logged: set = field(default_factory=set)  # "wrong" / "match": what `signed` has logged already
    next_seq: int = 1                             # our next reliable DATA sequence id
    unacked: dict = field(default_factory=dict)   # seq -> [raw, last_sent, tries]
    last_seen: float = field(default_factory=time.monotonic)
    closed: bool = False
    session: object = None                        # core.sessions.PlayerSession after LoginWithToken
    _next_call_id: int = 0x10000                  # ids for server-initiated requests

    # ---- packets --------------------------------------------------------------------------------
    def reply(self, **kw) -> Packet:
        return Packet(source=self.server_stream, dest=self.client_stream,
                      session_id=kw.pop("session_id", self.session_id),
                      signature=kw.pop("signature", self.client_sig), **kw)

    def signed(self, p: Packet, mode: str) -> bool:
        """Whether to take a client packet, by its signature (ours from the SYN-ACK) and the check mode."""
        if mode == "off":
            return True
        if p.signature == self.server_sig:
            # once per connection, after the handshake (a CONNECT is only logged when its signature is wrong)
            if mode == "log" and p.type != CONNECT and "match" not in self.sig_logged:
                self.sig_logged.add("match")
                log.info("%s: the game's packets carry our signature (type %d; nex_check_signatures)",
                         self.addr, p.type)
            return True
        refused = mode == "enforce"
        if "wrong" in self.sig_logged:
            log.debug("%s: type %d with a wrong signature %s", self.addr, p.type, "ignored" if refused else "taken")
        else:
            self.sig_logged.add("wrong")
            log.warning("%s: type %d with a wrong signature (%s, ours %s) %s (spoofed, or the game signs "
                        "differently: see nex_check_signatures)", self.addr, p.type, p.signature.hex(),
                        self.server_sig.hex(), "ignored" if refused else "taken anyway")
        return not refused

    def send(self, p: Packet, reliable: bool = False) -> None:
        raw = p.encode()
        self.sendto(raw, self.addr)
        if reliable:
            self.unacked[p.sequence_id] = [raw, time.monotonic(), 1]

    def on_ack(self, p: Packet) -> None:
        self.unacked.pop(p.sequence_id, None)

    def _ack_data(self, p: Packet) -> None:
        if p.flags & FLAG_NEED_ACK:
            self.send(self.reply(type=DATA, flags=FLAG_ACK, sequence_id=p.sequence_id,
                                 fragment_id=p.fragment_id))

    def receive_data(self, p: Packet) -> list[bytes]:
        """Handle a client DATA packet; returns the complete messages it finishes (in order). Sets `closed`
        when the client's message grows past MAX_MESSAGE_BYTES."""
        if seq_before(p.sequence_id, self.expected_seq) or p.sequence_id in self.pending:
            self._ack_data(p)                                             # a duplicate: our ack was lost
            return []
        if (p.sequence_id - self.expected_seq) & 0xFFFF >= REORDER_WINDOW:
            log.debug("%s: DATA seq %d too far ahead of %d, dropped", self.addr, p.sequence_id, self.expected_seq)
            return []
        try:
            plain = secure.decrypt_payload(self.key, p.payload, p.sequence_id) if self.key and p.payload \
                else p.payload
        except Exception as e:  # noqa: BLE001 - not acked: the game sends it again
            log.warning("%s: undecryptable DATA seq %d (%s)", self.addr, p.sequence_id, e)
            return []
        self._ack_data(p)
        self.buffered += len(plain)
        if self.buffered > MAX_MESSAGE_BYTES:
            log.warning("%s: message over %d bytes, dropping connection", self.addr, MAX_MESSAGE_BYTES)
            self.closed = True
            self.pending, self.fragments, self.buffered = {}, [], 0
            return []
        self.pending[p.sequence_id] = (p.fragment_id, plain)
        messages = []
        while self.expected_seq in self.pending:
            frag_id, chunk = self.pending.pop(self.expected_seq)
            self.expected_seq = (self.expected_seq + 1) & 0xFFFF
            self.fragments.append(chunk)
            if frag_id == 0:
                message = b"".join(self.fragments)
                self.buffered -= len(message)
                messages.append(message)
                self.fragments = []
        return messages

    # ---- messages -------------------------------------------------------------------------------
    def send_message(self, data: bytes) -> None:
        if self.closed:
            return
        chunks = [data[i:i + FRAGMENT_SIZE] for i in range(0, len(data), FRAGMENT_SIZE)] or [b""]
        for i, chunk in enumerate(chunks):
            seq = self.next_seq
            self.next_seq = (self.next_seq + 1) & 0xFFFF
            frag_id = 0 if i == len(chunks) - 1 else i + 1
            payload = secure.encrypt_payload(self.key, seq, chunk) if self.key else chunk
            self.send(self.reply(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id=seq,
                                 fragment_id=frag_id, payload=payload), reliable=True)

    def send_request(self, protocol: str, method: str, params: bytes, call_id: int | None = None) -> int:
        """Server-initiated RMC request (e.g. NotificationProtocol::ProcessNotificationEvent_V1,
        which the real server sends with call id 0). Returns the call id used."""
        if call_id is None:
            call_id = self._next_call_id
            self._next_call_id += 1
        self.send_message(rmc_codec.build_request(protocol, call_id, method, params))
        return call_id

    # ---- timers ---------------------------------------------------------------------------------
    def tick(self, now: float) -> bool:
        """Retransmit overdue packets. Returns False when the connection should be dropped."""
        if self.closed:
            return False
        if now - self.last_seen > IDLE_TIMEOUT_S:
            log.info("%s: idle for %.0fs, dropping connection", self.addr, now - self.last_seen)
            return False
        for seq, entry in list(self.unacked.items()):
            raw, last, tries = entry
            if now - last < RETRANSMIT_S:
                continue
            if tries >= MAX_TRIES:
                log.warning("%s: seq %d never acked after %d tries, dropping connection", self.addr, seq, tries)
                return False
            self.sendto(raw, self.addr)
            entry[1], entry[2] = now, tries + 1
        return True

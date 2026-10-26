"""Steep's owned-games entries for Ubisoft Connect (builtin/products.py): the bytes the Rust server writes too, and
which products are owned (field 20)."""
import hashlib

from powder.server.builtin import products
from powder.server.builtin.product_table import KINDS, PRODUCTS
from powder.server.transport.demux import pb


def _framed_sha(entries: list[bytes]) -> str:
    return hashlib.sha256(b"".join(pb.frame(e) for e in entries)).hexdigest()


def test_entries_match_the_rust_server():
    assert len(PRODUCTS) == 62
    assert _framed_sha(products.entries()) == "6daad70d0aab4157b044c3812fcea8ab859179d85e74bb145016e3236dcdf61a"
    assert (_framed_sha(products.entries(True, "achievements.xml"))
            == "6f20ee604adb131d50564d7050e6b19a019ae444479539b24493be251a90d7ee")


def test_dlc_entries_are_owned_and_the_others_as_captured():
    for entry, table in zip(products.entries(), PRODUCTS):
        fields = pb.decode(entry)
        pid = pb.first(fields, 1)
        captured = 1 if KINDS.get(pid) == "dlc" else pb.first(table, 20)
        assert pb.first(fields, 20) == captured, pid

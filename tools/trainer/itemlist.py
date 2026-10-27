#!/usr/bin/env python3
"""Custom items in the space's item list: the list the server serves at items/all (<data>/item_list.json), and a free
offer for each in the store (<data>/store_items.json). See server/src/assets.rs item_list and tools/forge/ITEMS.md.

The game ties a list entry's GUID to its own object by the entry's nameId (the object's name) and type ("GEAR" for
gear), and the store and menus go by the entry's tags. A custom item (e.g. one made by `items.py add-costume`) is a
copy of the entry of the item it is like, under its own nameId and a GUID derived from its name (stable across runs).

  itemlist.py <game list.json> <data dir> --add NAME --like ITEM [--add NAME --like ITEM ...]
  itemlist.py <game list.json> <data dir> --remove        (stop serving a list: the game falls back to its own,
                                                          which it must still have; see ITEMS.md)

<game list.json> is the game's whole list from `storemap.py <pid> <Steep> --item-list`, read while the game runs
with its own list (Ubisoft's) - never from a session that already took a custom list.
"""

import argparse
import json
import os
import sys
import uuid

NAMESPACE = uuid.UUID("5f0a43a7-0000-4000-8000-706f77646572")  # fixed: the same name gives the same GUID


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("game_list")
    ap.add_argument("data")
    ap.add_argument("--add", action="append", default=[], metavar="NAME")
    ap.add_argument("--like", action="append", default=[], metavar="ITEM")
    ap.add_argument("--remove", action="store_true")
    a = ap.parse_args()
    out = os.path.join(a.data, "item_list.json")
    if a.remove:
        if os.path.exists(out):
            os.remove(out)
        print(f"removed {out}")
        return
    if len(a.add) != len(a.like) or not a.add:
        sys.exit("give --add NAME --like ITEM pairs")
    items = json.load(open(a.game_list))["items"]
    by_name = {it["nameId"]: it for it in items}
    store_path = os.path.join(a.data, "store_items.json")
    store = json.load(open(store_path)) if os.path.exists(store_path) else {}
    for name, like in zip(a.add, a.like):
        src = by_name.get(like)
        if not src:
            sys.exit(f"{like}: no list entry has this nameId")
        guid = str(uuid.uuid5(NAMESPACE, name))
        items = [it for it in items if it["nameId"] != name] + [{**src, "itemId": guid, "nameId": name}]
        store[name] = {"guid": guid, "type": "0x3856b74b", "tags": src["tags"], "custom": True}
        print(f"{name}: {guid} ({src['type']}), tags {', '.join(src['tags'])}")
    with open(out, "w") as f:
        json.dump({"items": items}, f, indent=1)
    with open(store_path, "w") as f:
        json.dump(store, f, indent=1)
    print(f"{out}: {len(items)} items; {store_path}: {len(store)} store items (restart the server)")


if __name__ == "__main__":
    main()

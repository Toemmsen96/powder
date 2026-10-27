#!/usr/bin/env python3
"""Custom items in the space's item list: the list the server serves at items/all (<data>/item_list.json), and a free
offer for each in the store (<data>/store_items.json). See server/src/assets.rs item_list and tools/forge/ITEMS.md.

The game ties a list entry's GUID to its own object by a tag holding the object's name, and the store and menus go
by the entry's tags. A custom item (e.g. one made by `items.py add-costume`) takes the tags of the item it is like,
with that item's name tag swapped for its own; its GUID is derived from its name (stable across runs).

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
    by_tag = {t: it for it in items for t in it["tags"]}
    store_path = os.path.join(a.data, "store_items.json")
    store = json.load(open(store_path)) if os.path.exists(store_path) else {}
    for name, like in zip(a.add, a.like):
        src = by_tag.get(like)
        if not src:
            sys.exit(f"{like}: no list entry carries this name")
        guid = str(uuid.uuid5(NAMESPACE, name))
        tags = [name if t == like else t for t in src["tags"]]
        items = [it for it in items if name not in it["tags"]] + [{"itemId": guid, "spaceId": src["spaceId"],
                                                                   "tags": tags}]
        store[name] = {"guid": guid, "type": "0x3856b74b", "tags": tags, "custom": True}
        print(f"{name}: {guid}, tags {', '.join(tags)}")
    with open(out, "w") as f:
        json.dump({"items": items}, f, indent=1)
    with open(store_path, "w") as f:
        json.dump(store, f, indent=1)
    print(f"{out}: {len(items)} items; {store_path}: {len(store)} store items (restart the server)")


if __name__ == "__main__":
    main()

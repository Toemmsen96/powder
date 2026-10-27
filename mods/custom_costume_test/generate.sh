#!/usr/bin/env bash
# Generate this mod's .forge entries from your Steep install (they are Steep's data, so they are not in git).
#   mods/custom_costume_test/generate.sh [--chief] [<Steep folder>]
# Adds ONE new costume, COSTUME_MF_POWDER_TEST_01: a clone of the "urban outfit Japan" costume (costume 016) under
# new ids. Nothing of the game's is replaced. With --chief the new item, COSTUME_MF_POWDER_MASTER_CHIEF, is a clone of
# the costume mods/master_chief converts, made from that mod's generated entries (run mods/master_chief/generate.sh
# first): Master Chief as an item of its own, while the game's costume keeps its look (as long as the master_chief
# mod itself is off). The costume model comes from CHIEF_SRC (default: the master_chief mod's entries; it must hold a
# full costume, e.g. `make_chief.py --target urban_japan -o build/chief_016`). CHIEF_NAME names the item resource
# (default COSTUME_MF_POWDER_MASTER_CHIEF; its ids follow from it), CHIEF_TITLE is its name in the game (default
# "Master Chief": a new string in every UI language package, tools/forge/LOCALIZATION.md). The game lists the item
# once the server's item list has it (tools/trainer/itemlist.py --add NAME --id <item id>, tools/forge/ITEMS.md).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
I="$here/../../tools/forge/items.py"
args=(--like COSTUME_MF_URBAN_OUTFIT_JAPAN_139 --name COSTUME_MF_POWDER_TEST_01)
if [ "${1:-}" = --chief ]; then
    shift
    chief=${CHIEF_SRC:-$here/../master_chief/client/forge}/DataPC_extra_chr
    entity=$(cd "$chief" 2>/dev/null && ls -d *_STP_M_Costume_* 2>/dev/null | head -n 1 || true)
    [ -n "$entity" ] || { echo "no costume entity in $chief: run mods/master_chief/generate.sh first" >&2; exit 1; }
    args=(--costume "${entity#*_}" --name "${CHIEF_NAME:-COSTUME_MF_POWDER_MASTER_CHIEF}"
          --source-dir "$chief" --label PowderChief --title "${CHIEF_TITLE:-Master Chief}")
fi
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
M="$here/client/forge"
[ -f "$G/DataPC.forge" ] || { echo "no DataPC.forge in $G" >&2; exit 1; }
rm -rf "$M"
python3 "$I" add-costume "$G" "${args[@]}" -o "$M" | grep -v '^  /'
check=("$M"/*.json)
src=()
[ "${args[4]:-}" = --source-dir ] && src=(--source-dir "${args[5]}")
python3 "$I" check "$G" "${check[0]}" ${src[@]+"${src[@]}"}
# --chief: the clone shows the urban outfit's menu picture (its icon is shared, not cloned); that outfit is not in the
# store, so its picture becomes Chief's (rendered with Blender, as mods/master_chief does)
B=${BLENDER:-$(ls -d "$HOME"/Executables/Blender/blender-*/blender 2>/dev/null | sort -V | tail -n 1 || true)}
[ -n "$B" ] || B=$(command -v blender || true)
if [ "${args[4]:-}" = --source-dir ] && [ -n "$B" ]; then
    chief="$here/../master_chief"
    png=$(mktemp --suffix=.png) && pic=$(mktemp --suffix=.png)
    trap 'rm -f "$png" "$pic"' EXIT
    "$B" --factory-startup -b --python "$chief/render_icon.py" -- "$here/../../models/master_chief_high_poly/scene.gltf" \
        "$png" 256 >/dev/null 2>&1 && python3 - "$G" "$png" "$pic" "$M" <<'PY'
import os, sys
game, png, pic, out = sys.argv[1:]
here = os.path.dirname(os.path.abspath(out))
sys.path.insert(0, os.path.join(here, "..", "..", "master_chief"))
import make_chief
name = "STP_ICON_COSTUME_MF_URBAN_OUTFIT_JAPAN_139_00006_Map"
make_chief.picture(game, name, png, pic)
make_chief.run(os.path.join(make_chief.TOOLS, "texture.py"), "replace", os.path.join(game, "DataPC_extra_2.forge"), name,
               "--image", pic, "-o", os.path.join(out, "DataPC_extra_2"))
print(f"  picture: {name}")
PY
fi
echo "generated $M"

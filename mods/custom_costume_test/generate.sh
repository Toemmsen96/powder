#!/usr/bin/env bash
# Generate this mod's .forge entries from your Steep install (they are Steep's data, so they are not in git).
#   mods/custom_costume_test/generate.sh [--chief] [<Steep folder>]
# Adds ONE new costume, COSTUME_MF_POWDER_TEST_01: a clone of the "urban outfit Japan" costume (costume 016) under
# new ids. Nothing of the game's is replaced. With --chief the new item, POWDER_MASTER_CHIEF, is a clone of the
# costume mods/master_chief converts, made from that mod's generated entries (run mods/master_chief/generate.sh
# first): Master Chief as an item of its own, while the game's costume keeps its look (as long as the master_chief
# mod itself is off). The costume model comes from CHIEF_SRC (default: the master_chief mod's entries; it must hold a
# full costume, e.g. `make_chief.py --target penguin -o build/chief_penguin`). The item's name is CHIEF_NAME,
# default the name tag of an entry in Ubisoft's item list that no game object uses
# (PROGRESSION_TOKEN_BEAVER_COSTUME_ULC_CONDITION_ULC_4026690459, GUID 077f575a-e943-4d28-8cc4-7d637a54dce0): the game
# ties a list entry's GUID to the object of that name (G), so the new item gets a GUID that can be granted.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
I="$here/../../tools/forge/items.py"
args=(--like COSTUME_MF_URBAN_OUTFIT_JAPAN_139 --name COSTUME_MF_POWDER_TEST_01)
if [ "${1:-}" = --chief ]; then
    shift
    chief=${CHIEF_SRC:-$here/../master_chief/client/forge}/DataPC_extra_chr
    entity=$(cd "$chief" 2>/dev/null && ls -d *_STP_M_Costume_* 2>/dev/null | head -n 1 || true)
    [ -n "$entity" ] || { echo "no costume entity in $chief: run mods/master_chief/generate.sh first" >&2; exit 1; }
    args=(--costume "${entity#*_}" --name "${CHIEF_NAME:-PROGRESSION_TOKEN_BEAVER_COSTUME_ULC_CONDITION_ULC_4026690459}"
          --source-dir "$chief" --label PowderChief)
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
echo "generated $M"

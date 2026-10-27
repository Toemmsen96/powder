#!/usr/bin/env bash
# Generate this mod's .forge entries from your Steep install (they are Steep's data, so they are not in git).
#   mods/custom_costume_test/generate.sh [--chief] [<Steep folder>]
# Adds ONE new costume, COSTUME_MF_POWDER_TEST_01: a clone of the "urban outfit Japan" costume (costume 016) under
# new ids. Nothing of the game's is replaced. With --chief the new item, POWDER_MASTER_CHIEF, is a clone of the
# costume mods/master_chief converts, made from that mod's generated entries (run mods/master_chief/generate.sh
# first): Master Chief as an item of its own, while the game's costume keeps its look (as long as the master_chief
# mod itself is off).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
I="$here/../../tools/forge/items.py"
args=(--like COSTUME_MF_URBAN_OUTFIT_JAPAN_139 --name COSTUME_MF_POWDER_TEST_01)
if [ "${1:-}" = --chief ]; then
    shift
    chief="$here/../master_chief/client/forge/DataPC_extra_chr"
    entity=$(cd "$chief" 2>/dev/null && ls -d *_STP_M_Costume_* 2>/dev/null | head -n 1 || true)
    [ -n "$entity" ] || { echo "no costume entity in $chief: run mods/master_chief/generate.sh first" >&2; exit 1; }
    args=(--costume "${entity#*_}" --name POWDER_MASTER_CHIEF --source-dir "$chief")
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

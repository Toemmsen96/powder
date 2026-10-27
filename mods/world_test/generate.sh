#!/usr/bin/env bash
# Generate this mod's .forge entries from your own Steep install (they are Steep's data, so they are not in git).
#   mods/world_test/generate.sh [<Steep folder>]   (default: POWDER_STEEP_DIR or the usual Lutris prefix path)
#   WORLD_TEST=move mods/world_test/generate.sh    (only the moved boulder; WORLD_TEST=copy: only the two new firs)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
WORLD="$here/../../tools/forge/world.py"
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
MODE=${WORLD_TEST:-both}
W="$G/DataPC_STP_WhiteRoom.forge"
M="$here/client/forge/DataPC_STP_WhiteRoom"
[ -f "$W" ] || { echo "no DataPC_STP_WhiteRoom.forge in $G" >&2; exit 1; }
case $MODE in both|move|copy) ;; *) echo "WORLD_TEST must be both, move or copy (not $MODE)" >&2; exit 1 ;; esac
rm -rf "$here/client/forge"
if [ "$MODE" != move ]; then
    # Cell00036 (x 0..128, y 0..128): a copy of STP_WOR_GEN_VEG_Sapin_Medium_S4_01_C_006 (20 m fir) 12 m north of
    # the rider's spawn; Cell00027 (x -128..0, y -128..0): a copy of ..._Sapin_Small_S4_01_C_016 (9.5 m fir) 9.5 m
    # west of it. z = the ground there (firs stand with their origin on the ground)
    python3 "$WORLD" edit "$W" 36 -o "$M" --copy 0xd8d3d96665 2.3,12,-0.55
    python3 "$WORLD" edit "$W" 27 -o "$M" --copy 0xd8d3d9730d -9,-3,-2.0
fi
if [ "$MODE" != copy ]; then
    # Cell00028 (x 0..128, y -128..0): STP_GEN_ROCK_Granite_Small_N_01_A_008 from (12.6, -32.8) to 9.5 m east-south-
    # east of the spawn, sunk into the ground as deep as before (0.77 m)
    python3 "$WORLD" edit "$W" 28 -o "$M" --move 0xd8d3d94147 9.5,-7.5,-1.26
fi
echo "generated $M ($MODE)"

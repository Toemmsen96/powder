#!/usr/bin/env bash
# Generate this mod's .forge entries from your own Steep install (they are Steep's data, so they are not in git).
#   mods/world_test/generate.sh [<Steep folder>]   (default: POWDER_STEEP_DIR or the usual Lutris prefix path)
#   WORLD_TEST=move mods/world_test/generate.sh    (only the moved boulder; WORLD_TEST=copy: only the two new firs;
#   WORLD_TEST=big (default): a ring of 9 firs within 4-12 m of the rider, the boulder doubled beside them)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
WORLD="$here/../../tools/forge/world.py"
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
MODE=${WORLD_TEST:-big}
W="$G/DataPC_STP_WhiteRoom.forge"
M="$here/client/forge/DataPC_STP_WhiteRoom"
[ -f "$W" ] || { echo "no DataPC_STP_WhiteRoom.forge in $G" >&2; exit 1; }
case $MODE in big|both|move|copy) ;; *) echo "WORLD_TEST must be big, both, move or copy (not $MODE)" >&2; exit 1 ;; esac
rm -rf "$here/client/forge"
if [ "$MODE" = big ]; then
    # A ring of firs round the spawn (0, 0, -0.93), clear of the customisation cameras south of it (x 1..2.3,
    # y -3.8..-4.9); z near the spawn's ground (G: a fir may stand a little sunk or above it). The 20 m fir the
    # copies come from (52 m north) grows 2.5x.
    F=0xd8d3d96665  # STP_WOR_GEN_VEG_Sapin_Medium_S4_01_C_006, Cell00036 (x 0..128, y 0..128)
    python3 "$WORLD" edit "$W" 36 -o "$M" --copy $F 2.3,12,-0.55 --copy $F 5,5,-0.8,40 --copy $F 9,1.5,-0.9,95 \
        --copy $F 1,7,-0.7,160 --copy $F 7,9,-0.6,220 --scale $F 2.5
    S=0xd8d3d9730d  # ..._Sapin_Small_S4_01_C_016 (9.5 m), Cell00027 (x -128..0, y -128..0)
    python3 "$WORLD" edit "$W" 27 -o "$M" --copy $S -9,-3,-2.0 --copy $S -4.5,-1.5,-1.3,60 \
        --copy $S -6,-6.5,-1.6,130 --copy $S -2.5,-9,-1.5,250
    # the boulder (~3 x 3 x 5 m) 6 m east of the rider, twice its size
    python3 "$WORLD" edit "$W" 28 -o "$M" --move 0xd8d3d94147 6,-1.5,-1.4 --scale 0xd8d3d94147 2
    echo "generated $M ($MODE)"
    exit 0
fi
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

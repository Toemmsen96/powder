#!/usr/bin/env bash
# Generate this mod's .forge entries from your own Steep install (they are Steep's data, so they are not in git).
#   mods/shoulder_pads/generate.sh [<Steep folder>]     (default: POWDER_STEEP_DIR or the usual Lutris prefix path)
# model/pads.glb is the mod's own geometry (model/make_pads.py writes it; or model it in Blender, see README).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
MESH="$here/../../tools/forge/mesh.py"
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
M="$here/client/forge"
[ -f "$G/DataPC_extra_chr_onboarding.forge" ] || { echo "no DataPC_extra_chr_onboarding.forge in $G" >&2; exit 1; }
rm -rf "$M"
# STP_M_Jacket_001 (entity: LOD0..3) and the copy of its LOD0 in the Alps map archive; the pads join every LOD
python3 "$MESH" import "$G/DataPC_extra_chr_onboarding.forge" 0x8b66b1fe21 --glb "$here/model/pads.glb" --to LOD --add \
    -o "$M/DataPC_extra_chr_onboarding"
python3 "$MESH" import "$G/DataPC_STP_Alpes.forge" 0x7a5374f895 --glb "$here/model/pads.glb" --to LOD0 --add \
    -o "$M/DataPC_STP_Alpes"
echo "generated $M"

#!/usr/bin/env bash
# Generate this mod's .forge entries from your own Steep install (they are Steep's data, so they are not in git).
#   mods/mesh_edit/generate.sh [<Steep folder>]     (default: POWDER_STEEP_DIR or the usual Lutris prefix path)
#   MESH_EDIT_INFLATE=0.08 mods/mesh_edit/generate.sh    (metres along the normals; default 0.05)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
MESH="$here/../../tools/forge/mesh.py"
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
INFLATE=${MESH_EDIT_INFLATE:-0.05}
M="$here/client/forge"
[ -f "$G/DataPC_extra_chr_onboarding.forge" ] || { echo "no DataPC_extra_chr_onboarding.forge in $G" >&2; exit 1; }
rm -rf "$M"
# STP_M_Jacket_001 (entity: LOD0..3 + material + texture set) and the copy of its LOD0 in the Alps map archive
python3 "$MESH" edit "$G/DataPC_extra_chr_onboarding.forge" 0x8b66b1fe21 --inflate "$INFLATE" -o "$M/DataPC_extra_chr_onboarding"
python3 "$MESH" edit "$G/DataPC_STP_Alpes.forge" 0x7a5374f895 --inflate "$INFLATE" -o "$M/DataPC_STP_Alpes"
echo "generated $M"

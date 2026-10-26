#!/usr/bin/env bash
# Generate this mod's .forge entries from your own Steep install (they are Steep's data, so they are not in git).
#   mods/model_swap/generate.sh [<Steep folder>]     (default: POWDER_STEEP_DIR or the usual Lutris prefix path)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
F="$here/../../tools/forge/forge.py"
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
M="$here/client/forge"
[ -f "$G/DataPC_extra_chr.forge" ] || { echo "no DataPC_extra_chr.forge in $G" >&2; exit 1; }
rm -rf "$M"
python3 "$F" swap "$G/DataPC_extra_chr_onboarding.forge" 0xeac3434b92 --with "$G/DataPC_extra_chr.forge" 0x9c6dd17896 -o "$M/DataPC_extra_chr_onboarding"
python3 "$F" swap "$G/DataPC_extra_chr.forge" 0x8af9205b3f --with "$G/DataPC_extra_chr.forge" 0x9c6dd17896 -o "$M/DataPC_extra_chr"
python3 "$F" swap "$G/DataPC_extra_chr.forge" 0xeac3434be5 --with "$G/DataPC_extra_chr.forge" 0x9c6dd7015f -o "$M/DataPC_extra_chr"
echo "generated $M"

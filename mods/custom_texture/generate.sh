#!/usr/bin/env bash
# Generate this mod's .forge entries from your own Steep install: the jacket's diffuse texture, re-encoded from a
# generated pattern (or your image) in the texture's own format, size and mip chain. The entries keep Steep's ids
# and headers, so they are built from the install and not kept in git.
#   mods/custom_texture/generate.sh [<Steep folder>] [<image.png>]
#   (Steep folder default: POWDER_STEEP_DIR or the usual Lutris prefix path; no image: the POWDER checkerboard)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
T="$here/../../tools/forge/texture.py"
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
M="$here/client/forge"
if [ -n "${2:-}" ]; then src=(--image "$2"); else src=(--pattern checker --text POWDER); fi
[ -f "$G/DataPC_extra_chr_onboarding.forge" ] || { echo "no DataPC_extra_chr_onboarding.forge in $G" >&2; exit 1; }
DIFFUSE=0x7a5374f6aa    # STP_M_Jacket_001_T01_DiffuseMap (BC1 2048x2048, 12 levels; levels 0/1 in its Mip0/Mip1)
rm -rf "$M"
# the TextureMap + its Mip0/Mip1 (only this archive has the Mips) ...
python3 "$T" replace "$G/DataPC_extra_chr_onboarding.forge" "$DIFFUSE" "${src[@]}" -o "$M/DataPC_extra_chr_onboarding"
# ... and the two other copies of the TextureMap, so whichever the game loads is ours (they say that levels 0/1
# live in the onboarding archive: that is expected, those are replaced above)
python3 "$T" replace "$G/DataPC_extra_chr.forge" "$DIFFUSE" "${src[@]}" -o "$M/DataPC_extra_chr"
python3 "$T" replace "$G/DataPC_STP_Alpes.forge" "$DIFFUSE" "${src[@]}" -o "$M/DataPC_STP_Alpes"
echo "generated $M"

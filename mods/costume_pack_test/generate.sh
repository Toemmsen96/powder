#!/usr/bin/env bash
# Makes this content mod's assets/ from your own Steep install and the Master Chief model (both not in git):
#   assets/chief_model/  Chief fitted onto costume 016's entries (mods/master_chief/make_chief.py --target urban_japan)
#   assets/chief.png     his picture (mods/master_chief/render_icon.py, Blender)
#   assets/overalls.png  a test picture for the Bib clone (a POWDER checkerboard)
#   mods/costume_pack_test/generate.sh [<Steep folder>]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
MODEL=$repo/models/master_chief_high_poly
mkdir -p "$here/assets"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
python3 "$repo/mods/master_chief/make_chief.py" --game "$G" --model "$MODEL" --target urban_japan -o "$tmp" >/dev/null
rm -rf "$here/assets/chief_model"
mv "$tmp/DataPC_extra_chr" "$here/assets/chief_model"
B=${BLENDER:-$(ls -d "$HOME"/Executables/Blender/blender-*/blender 2>/dev/null | sort -V | tail -n 1 || command -v blender)}
"$B" --factory-startup -b --python "$repo/mods/master_chief/render_icon.py" -- "$MODEL/scene.gltf" \
    "$here/assets/chief.png" 256 >/dev/null 2>&1
python3 -c "
import sys, numpy as np
sys.path.insert(0, '$repo/tools/forge')
import texture
img = texture.pattern('checker', 256, 256, 'POWDER').astype(np.uint8)
if img.shape[-1] == 3:
    img = np.concatenate([img, np.full(img.shape[:2] + (1,), 255, np.uint8)], -1)
texture.write_png('$here/assets/overalls.png', img)"
echo "generated $here/assets"

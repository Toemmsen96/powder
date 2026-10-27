#!/usr/bin/env bash
# The mod author's step: writes assets/ (Chief's own files: the fitted models per gender, his maps per Steep material,
# his picture) from the Sketchfab model (not in git, models/master_chief_high_poly) and the Steep skeleton of your
# install. The game's entries are made from these at each game start (tools/forge/compose.py, costume_model.py).
#   mods/master_chief_costume/generate.sh [<Steep folder>]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
MODEL=$repo/models/master_chief_high_poly
rm -rf "$here/assets"
python3 "$repo/mods/master_chief/make_chief.py" --game "$G" --model "$MODEL" --target urban_japan \
    --export-assets "$here/assets"
B=${BLENDER:-$(ls -d "$HOME"/Executables/Blender/blender-*/blender 2>/dev/null | sort -V | tail -n 1 || command -v blender)}
"$B" --factory-startup -b --python "$repo/mods/master_chief/render_icon.py" -- "$MODEL/scene.gltf" \
    "$here/assets/icon.png" 256 >/dev/null 2>&1
echo "generated $here/assets"

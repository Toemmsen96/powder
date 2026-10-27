#!/usr/bin/env bash
# Generate this mod's .forge entries from your Steep install and the Master Chief model (neither is in git).
#   mods/master_chief/generate.sh [<Steep folder>] [<model folder>]
#   CHIEF_TARGET=military mods/master_chief/generate.sh   (the costume Chief replaces; default penguin)
# The model: "Master Chief High Poly" by HarrisonHag1 (CC-BY-4.0, Sketchfab), unpacked as glTF into
# models/master_chief_high_poly/ (scene.gltf, scene.bin, textures/). Needs numpy and Pillow. With Blender
# (BLENDER, default ~/Executables/Blender/blender-*/blender or `blender` on PATH) the costume's menu picture is
# rendered from the model too.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
MODEL=${2:-$here/../../models/master_chief_high_poly}
[ -f "$G/DataPC_extra_chr_onboarding.forge" ] || { echo "no DataPC_extra_chr_onboarding.forge in $G" >&2; exit 1; }
[ -f "$MODEL/scene.gltf" ] || { echo "no scene.gltf in $MODEL (unpack the model there)" >&2; exit 1; }
B=${BLENDER:-$(ls -d "$HOME"/Executables/Blender/blender-*/blender 2>/dev/null | sort -V | tail -n 1 || true)}
[ -n "$B" ] || B=$(command -v blender || true)
icon=()
if [ -n "$B" ]; then
    png=$(mktemp --suffix=.png)
    trap 'rm -f "$png"' EXIT
    "$B" --factory-startup -b --python "$here/render_icon.py" -- "$MODEL/scene.gltf" "$png" 256 >/dev/null 2>&1 \
        && icon=(--icon "$png") || echo "the picture could not be rendered ($B); the costume keeps its own" >&2
else
    echo "no Blender found: the costume keeps its own menu picture" >&2
fi
python3 "$here/make_chief.py" --game "$G" --model "$MODEL" --target "${CHIEF_TARGET:-penguin}" ${icon[@]+"${icon[@]}"} \
    -o "$here/client/forge"
echo "generated $here/client/forge"

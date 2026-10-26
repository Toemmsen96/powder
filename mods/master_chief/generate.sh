#!/usr/bin/env bash
# Generate this mod's .forge entries from your Steep install and the Master Chief model (neither is in git).
#   mods/master_chief/generate.sh [<Steep folder>] [<model folder>]
# The model: "Master Chief High Poly" by HarrisonHag1 (CC-BY-4.0, Sketchfab), unpacked as glTF into
# models/master_chief_high_poly/ (scene.gltf, scene.bin, textures/). Needs numpy and Pillow.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
G=${1:-${POWDER_STEEP_DIR:-$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep}}
MODEL=${2:-$here/../../models/master_chief_high_poly}
[ -f "$G/DataPC_extra_chr_onboarding.forge" ] || { echo "no DataPC_extra_chr_onboarding.forge in $G" >&2; exit 1; }
[ -f "$MODEL/scene.gltf" ] || { echo "no scene.gltf in $MODEL (unpack the model there)" >&2; exit 1; }
python3 "$here/make_chief.py" --game "$G" --model "$MODEL" -o "$here/client/forge"
echo "generated $here/client/forge"

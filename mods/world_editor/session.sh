#!/usr/bin/env bash
# The world editor's session step: linux/powder-wrap runs a named mod's session.sh at each game start, before it
# builds the asset mods (see the "mod session steps" part there). This one turns the server's stored world edits
# into edited cells (client/forge/) and puts the panel's object index beside the game (apply.py, README.md).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
exec python3 -I "$here/apply.py" session

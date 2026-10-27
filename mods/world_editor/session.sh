#!/usr/bin/env bash
# The world editor's session step: linux/powder-wrap runs a named mod's session.sh at each game start, before it
# builds the asset mods (see the "mod session steps" part there). This one turns the server's stored world edits
# into edited cells (client/forge/) and puts the panel's object index beside the game (apply.py, README.md).
# With the overlay (a runtime folder), it then leaves the live helper running for the session (live.py: edits made
# in-game show when their cell streams in again; it stops by itself when powder-wrap removes the runtime folder).
# WORLD_EDITOR_LIVE=0 leaves it out.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
python3 -I "$here/apply.py" session
rc=$?
if [ -n "${POWDER_MOD_RUNTIME:-}" ] && [ -d "$POWDER_MOD_RUNTIME" ] && [ "${WORLD_EDITOR_LIVE:-1}" != 0 ]; then
    # its own session (setsid): not in the step's process group, which powder-wrap's timeout would signal
    setsid python3 -I "$here/live.py" run < /dev/null &
    echo "world_editor: live helper started (pid $!)" >&2
fi
exit $rc

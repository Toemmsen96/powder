#!/usr/bin/env bash
# Build the native TLS helper for Linux/Proton (the version.dll proxy) into native/bin/.
# Needs a MinGW-w64 cross compiler (x86_64-w64-mingw32-gcc) or zig. See native/version_proxy.c.
#   native/build.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin="$here/bin"
mkdir -p "$bin"
if command -v x86_64-w64-mingw32-gcc >/dev/null; then
    cc() { x86_64-w64-mingw32-gcc "$@"; }
elif command -v zig >/dev/null; then
    cc() { zig cc -target x86_64-windows-gnu "$@"; }
else
    echo "need x86_64-w64-mingw32-gcc (mingw-w64) or zig" >&2; exit 1
fi
cc -shared -O2 -o "$bin/version.dll" "$here/version_proxy.c" "$here/version.def" -lbcrypt
echo "built: $bin/version.dll  (the Linux/Proton TLS helper; powder-wrap deploys it)"

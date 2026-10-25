#!/usr/bin/env bash
# Build the native helpers for Linux/Proton into native/bin/:
#   - version.dll + armer.exe : the TLS helper (version.dll proxy), see native/version_proxy.c
#   - powder_overlay.dll       : the in-game ImGui admin/debug overlay, see native/overlay/overlay.cpp
#   - nettest.exe              : a standalone check of the overlay's networking (run under Wine)
# Needs a MinGW-w64 cross compiler (x86_64-w64-mingw32-gcc/g++). Dear ImGui, MinHook and nlohmann/json are
# fetched into native/third_party/ (gitignored, pinned) on first build.
#   native/build.sh            # everything
#   native/build.sh tls        # just the TLS helper
#   native/build.sh overlay    # just the overlay + nettest
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin="$here/bin"
tp="$here/third_party"
mkdir -p "$bin"

if command -v x86_64-w64-mingw32-gcc >/dev/null && command -v x86_64-w64-mingw32-g++ >/dev/null; then
    cc()  { x86_64-w64-mingw32-gcc  "$@"; }
    cxx() { x86_64-w64-mingw32-g++  "$@"; }
else
    echo "need the MinGW-w64 cross compiler (x86_64-w64-mingw32-gcc / -g++)" >&2; exit 1
fi

fetch_deps() {
    command -v git  >/dev/null || { echo "need git to fetch third-party sources" >&2; exit 1; }
    [ -d "$tp/imgui" ]   || git clone --quiet --depth 1 --branch v1.91.5 https://github.com/ocornut/imgui "$tp/imgui"
    [ -d "$tp/minhook" ] || git clone --quiet --depth 1 --branch v1.3.3  https://github.com/TsudaKageyu/minhook "$tp/minhook"
    if [ ! -f "$tp/json/json.hpp" ]; then
        mkdir -p "$tp/json"
        command -v curl >/dev/null || { echo "need curl to fetch json.hpp" >&2; exit 1; }
        curl -fsSL -o "$tp/json/json.hpp" \
            https://github.com/nlohmann/json/releases/download/v3.11.3/json.hpp
    fi
}

build_tls() {
    cc -shared -O2 -o "$bin/version.dll" "$here/version_proxy.c" "$here/version.def" -lbcrypt
    cc -O2 -o "$bin/armer.exe" "$here/armer.c"
    echo "built: version.dll + armer.exe  (the Linux/Proton TLS helper)"
}

build_overlay() {
    fetch_deps
    local img="$tp/imgui"
    local obj="$bin/obj"; rm -rf "$obj"; mkdir -p "$obj"
    local inc=(-I"$img" -I"$img/backends" -I"$tp/minhook/include" -I"$tp/json" -I"$here/overlay")

    # MinHook (C)
    for s in buffer hook trampoline; do
        cc -O2 -c "$tp/minhook/src/$s.c" -o "$obj/mh_$s.o" -I"$tp/minhook/include" -I"$tp/minhook/src"
    done
    cc -O2 -c "$tp/minhook/src/hde/hde64.c" -o "$obj/hde64.o" -I"$tp/minhook/src"

    # ImGui + backends + our C++
    local cxxflags=(-O2 -std=c++17 -Wall "${inc[@]}")
    local srcs=("$img/imgui" "$img/imgui_draw" "$img/imgui_tables" "$img/imgui_widgets"
                "$img/backends/imgui_impl_dx11" "$img/backends/imgui_impl_win32"
                "$here/overlay/overlay" "$here/overlay/http" "$here/overlay/memscan")
    for s in "${srcs[@]}"; do
        cxx "${cxxflags[@]}" -c "$s.cpp" -o "$obj/$(basename "$s").o"
    done

    cxx -shared -o "$bin/powder_overlay.dll" "$obj"/*.o \
        -static -static-libgcc -static-libstdc++ -s \
        -ld3d11 -ldxgi -ld3dcompiler_47 -lwinhttp -lgdi32 -luser32 -ldwmapi -limm32 -lole32
    echo "built: powder_overlay.dll  (the in-game overlay; powder-wrap deploys it with POWDER_OVERLAY=1)"

    # standalone networking check (reuses http.o)
    cxx "${cxxflags[@]}" -c "$here/overlay/nettest.cpp" -o "$obj/nettest.o"
    cxx -o "$bin/nettest.exe" "$obj/nettest.o" "$obj/http.o" -static -static-libgcc -static-libstdc++ -s -lwinhttp
    echo "built: nettest.exe  (wine native/bin/nettest.exe <url> <email> <password> [id])"
    rm -rf "$obj"
}

case "${1:-all}" in
    tls) build_tls ;;
    overlay) build_overlay ;;
    all) build_tls; build_overlay ;;
    *) echo "usage: build.sh [all|tls|overlay]" >&2; exit 1 ;;
esac

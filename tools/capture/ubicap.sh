#!/usr/bin/env bash
# Capture the REAL Ubisoft Connect account-service responses (ownership, entitlements, offers, parameters) so
# they can be baked into Powder. Runs mitmproxy as an HTTPS proxy, trusts its CA in the Wine prefix (so Ubisoft
# Connect - which uses the prefix cert store - talks through it), points the prefix at the proxy, and saves every
# ubi.com flow. Use WITHOUT powder-wrap (Lutris launches the game normally, against the live account services).
#
#   WINEPREFIX=~/Games/ubisoft-connect tools/capture/ubicap.sh
#   # then launch Steep in Lutris (no powder-wrap), sign in, open the map / travel to a DLC world, open the shop
#   # Ctrl-C here when done -> flows saved; cleanup removes the proxy + CA from the prefix
#
# Note: Ubisoft Connect's traffic (schannel, trusts the prefix store) is captured. steep.exe's own calls use
# pinned OpenSSL and will NOT go through the proxy - but DLC/ownership is Connect's, which is what we need.
set -euo pipefail

: "${WINEPREFIX:?set WINEPREFIX to the game prefix, e.g. ~/Games/ubisoft-connect}"
WINE=${POWDER_WINE:-wine}
OUT=${1:-$HOME/ubi-capture}
VENV="$HOME/.cache/ubicap-venv"
MITM_CA="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
PROXY_KEY='HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
say() { printf 'ubicap: %s\n' "$*" >&2; }

# 1. mitmdump: system install if present, else a private venv
if command -v mitmdump >/dev/null; then
    MITMDUMP=$(command -v mitmdump)
elif [ -x "$VENV/bin/mitmdump" ]; then
    MITMDUMP="$VENV/bin/mitmdump"
else
    say "installing mitmproxy into $VENV (one-time)"
    python3 -m venv "$VENV"
    "$VENV/bin/pip" -q install --upgrade pip >/dev/null
    "$VENV/bin/pip" -q install mitmproxy >/dev/null
    MITMDUMP="$VENV/bin/mitmdump"
fi
say "using $MITMDUMP"

# 2. make sure mitmproxy's CA exists
if [ ! -f "$MITM_CA" ]; then
    say "generating mitmproxy CA"
    timeout 5 "$MITMDUMP" -q >/dev/null 2>&1 || true
    [ -f "$MITM_CA" ] || { say "mitmproxy CA not created at $MITM_CA"; exit 1; }
fi

# 3. trust the CA in the Wine prefix (same registry layout powder-wrap/trust.rs use)
HERE=$(cd "$(dirname "$0")" && pwd)
REG=$(mktemp --suffix=.reg)
THUMB=$(python3 "$HERE/mkcareg.py" "$MITM_CA" "$REG")
ROOT='Software\Microsoft\SystemCertificates\Root\Certificates'
say "trusting mitmproxy CA (HKLM + HKCU) in $WINEPREFIX"
WINEPREFIX="$WINEPREFIX" "$WINE" regedit "$REG" >/dev/null 2>&1 || true

# 4. point the prefix at the proxy (WinINet / Ubisoft Connect reads this)
say "setting the prefix proxy -> 127.0.0.1:8080"
WINEPREFIX="$WINEPREFIX" "$WINE" reg add "$PROXY_KEY" /v ProxyEnable /t REG_DWORD /d 1 /f >/dev/null 2>&1 || true
WINEPREFIX="$WINEPREFIX" "$WINE" reg add "$PROXY_KEY" /v ProxyServer /t REG_SZ /d "127.0.0.1:8080" /f >/dev/null 2>&1 || true

cleanup() {
    say "cleanup: removing proxy + CA from the prefix"
    WINEPREFIX="$WINEPREFIX" "$WINE" reg add "$PROXY_KEY" /v ProxyEnable /t REG_DWORD /d 0 /f >/dev/null 2>&1 || true
    WINEPREFIX="$WINEPREFIX" "$WINE" reg delete "$PROXY_KEY" /v ProxyServer /f >/dev/null 2>&1 || true
    WINEPREFIX="$WINEPREFIX" "$WINE" reg delete "HKEY_LOCAL_MACHINE\\$ROOT\\$THUMB" /f >/dev/null 2>&1 || true
    WINEPREFIX="$WINEPREFIX" "$WINE" reg delete "HKEY_CURRENT_USER\\$ROOT\\$THUMB" /f >/dev/null 2>&1 || true
    rm -f "$REG"
    say "flows saved to $OUT.flows  -> parse with: python3 tools/capture/ubiparse.py $OUT.flows"
}
trap cleanup EXIT

# 5. capture every ubi.com flow
say "proxy on 127.0.0.1:8080. In Lutris (Steep > System options > Environment variables) set:"
say "    HTTP_PROXY=http://127.0.0.1:8080   HTTPS_PROXY=http://127.0.0.1:8080"
say "    http_proxy=http://127.0.0.1:8080   https_proxy=http://127.0.0.1:8080"
say "then launch Steep (no powder-wrap), sign in, open the map / a DLC world / the shop."
say "watch below: each captured request prints a line. press Ctrl-C when done."
# capture everything (so we can confirm routing works); ubiparse.py filters to the DLC-relevant endpoints
"$MITMDUMP" --listen-host 127.0.0.1 -p 8080 -w "$OUT.flows" --set flow_detail=1

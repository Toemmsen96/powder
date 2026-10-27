#!/usr/bin/env python3
# Parse a mitmproxy flow dump and print the Ubisoft account-service requests + responses that matter for DLC:
# ownership, entitlements, offers (store), parameters, rewards. Run with the same Python that has mitmproxy.
#   python3 ubiparse.py <capture.flows> [--all] [--full]
import sys
from mitmproxy.io import FlowReader

INTERESTING = ("entitlement", "ownership", "/offers", "parameterGroups", "rewards", "/actions",
               "addon", "ulc", "dlc", "product", "fullgame", "catalog", "configs/primarystore")

def main():
    path = sys.argv[1]
    show_all = "--all" in sys.argv
    full = "--full" in sys.argv
    with open(path, "rb") as f:
        for flow in FlowReader(f).stream():
            if not hasattr(flow, "request") or flow.request is None:
                continue
            url = flow.request.pretty_url
            low = url.lower()
            if not show_all and not any(k in low for k in INTERESTING):
                continue
            resp = flow.response
            body = ""
            if resp is not None:
                try:
                    body = resp.get_text(strict=False) or ""
                except Exception:
                    body = f"<{len(resp.content)} bytes>"
            if not full and len(body) > 4000:
                body = body[:4000] + f"... (+{len(body) - 4000} bytes)"
            print("=" * 100)
            print(f"{flow.request.method} {url}")
            if resp is not None:
                print(f"-> {resp.status_code}  ({len(resp.content)} bytes)")
            print(body)

if __name__ == "__main__":
    main()

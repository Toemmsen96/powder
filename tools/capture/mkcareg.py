#!/usr/bin/env python3
# Write a Wine .reg that trusts a CA (PEM) in BOTH the machine and user root stores (the layout Wine/crypt32
# use; Chromium/CEF under Wine often reads the per-user store). Prints the SHA-1 thumbprint so the caller can
# build the key paths for cleanup.
#   python3 mkcareg.py <ca.pem> <out.reg>   -> prints THUMBPRINT
import hashlib, ssl, struct, sys

der = ssl.PEM_cert_to_DER_cert(open(sys.argv[1]).read())
thumb = hashlib.sha1(der).hexdigest().upper()
prop = lambda i, d: struct.pack("<III", i, 1, len(d)) + d
blob = prop(3, hashlib.sha1(der).digest()) + prop(0x20, der)
h = [f"{b:02x}" for b in blob]
lines = [",".join(h[i:i + 25]) for i in range(0, len(h), 25)]
hexblob = '"Blob"=hex:' + ",\\\r\n  ".join(lines)

out = ["Windows Registry Editor Version 5.00", ""]
for root in (r"HKEY_LOCAL_MACHINE\Software\Microsoft\SystemCertificates\Root\Certificates",
             r"HKEY_CURRENT_USER\Software\Microsoft\SystemCertificates\Root\Certificates"):
    out += ["[" + root + "\\" + thumb + "]", hexblob, ""]
open(sys.argv[2], "w", newline="\r\n").write("\r\n".join(out) + "\r\n")
print(thumb)

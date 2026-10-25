#!/usr/bin/env python3
# Write a Wine .reg that trusts a CA (PEM) in the prefix's root store (the layout Wine/crypt32 use, same as
# powder-wrap and server/src/trust.rs). Prints the HKLM key so the caller can delete it later.
#   python3 mkcareg.py <ca.pem> <out.reg>
import hashlib, ssl, struct, sys

der = ssl.PEM_cert_to_DER_cert(open(sys.argv[1]).read())
thumb = hashlib.sha1(der).hexdigest().upper()
key = r"HKEY_LOCAL_MACHINE\Software\Microsoft\SystemCertificates\Root\Certificates" + "\\" + thumb

prop = lambda i, d: struct.pack("<III", i, 1, len(d)) + d
blob = prop(3, hashlib.sha1(der).digest()) + prop(0x20, der)
h = [f"{b:02x}" for b in blob]
lines = [",".join(h[i:i + 25]) for i in range(0, len(h), 25)]
reg = 'Windows Registry Editor Version 5.00\r\n\r\n[' + key + ']\r\n"Blob"=hex:' + ",\\\r\n  ".join(lines) + "\r\n"
open(sys.argv[2], "w", newline="").write(reg)
print(key)

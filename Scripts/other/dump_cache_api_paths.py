"""Dumps all /api/v0/... paths found in the Firefox cache (JS bundles)."""

import os
import re
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
cache = (
    r"C:\Users\danal\AppData\Local\Mozilla\Firefox\Profiles"
    r"\ebsfvtv4.default-release\cache2\entries"
)
pattern = re.compile(r"/api/v0/[A-Za-z0-9_/\-{}?=&.]*")
paths = set()
for name in os.listdir(cache):
    p = os.path.join(cache, name)
    try:
        if os.path.getsize(p) < 20_000_000:
            data = open(p, "rb").read()
            if b"api/v0" in data:
                paths |= set(pattern.findall(data.decode("utf-8", "replace")))
    except OSError:
        pass
for p in sorted(paths):
    print(p)

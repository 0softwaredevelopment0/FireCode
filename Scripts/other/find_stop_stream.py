"""Finds stop_stream payload shape in the Firefox cache JS bundles."""

import os
import re
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
cache = (
    r"C:\Users\danal\AppData\Local\Mozilla\Firefox\Profiles"
    r"\ebsfvtv4.default-release\cache2\entries"
)
for name in os.listdir(cache):
    p = os.path.join(cache, name)
    try:
        if os.path.getsize(p) > 20_000_000:
            continue
        data = open(p, "rb").read()
    except OSError:
        continue
    if b"stop_stream" not in data:
        continue
    text = data.decode("utf-8", "replace")
    for m in re.finditer(r"stop_stream", text):
        s = max(0, m.start() - 400)
        print("=" * 60)
        print(text[s:m.start() + 120].replace("\n", " ")[:520])

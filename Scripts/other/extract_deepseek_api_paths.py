"""Extracts api/v0 paths from the chat.deepseek.com JS bundles."""

import re
import sys
import urllib.request

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
UA = {"User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko/20100101 Firefox/145.0"}


def fetch(url: str) -> str:
    return urllib.request.urlopen(
        urllib.request.Request(url, headers=UA), timeout=30
    ).read().decode("utf-8", "replace")


html = fetch("https://chat.deepseek.com/")
srcs = re.findall(r'src="([^"]+\.js[^"]*)"', html)
print("scripts:", srcs[:12])

paths = set()
for src in srcs:
    url = src if src.startswith("http") else "https://chat.deepseek.com" + src
    try:
        js = fetch(url)
    except Exception as e:  # noqa: BLE001
        print(f"  fetch fail {url}: {e}")
        continue
    found = set(re.findall(r'["\`](/api/v0/[A-Za-z0-9_\-/${}?=&.]*)["`]', js))
    if found:
        print(f"  {url.rsplit('/', 1)[-1][:60]}: {len(found)} paths")
    paths |= found

for p in sorted(paths):
    print(p)

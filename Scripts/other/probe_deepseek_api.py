"""DeepSeek web API probe (phase 1b research).

Reads userToken from the Firefox profile (path taken from FireCode config.toml),
then sends a few minimal requests to chat.deepseek.com to determine:
  - whether plain HTTP (no browser) passes AWS WAF
  - which auth headers the chat completion endpoint expects
  - the shape of a working request (documented in the Vault afterwards)

Secrets policy: the token value is never printed; only its length and status codes.

Usage: py -3 probe_deepseek_api.py
"""

import base64
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HOME = Path(os.environ["USERPROFILE"])
CONFIG = HOME / "AppData/Roaming/0softwaredevelopment0/FireCode/config/config.toml"
BASE = "https://chat.deepseek.com"
UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:145.0) "
    "Gecko/20100101 Firefox/145.0"
)


def read_config_profile() -> str:
    raw = CONFIG.read_text(encoding="utf-8")
    for line in raw.splitlines():
        line = line.strip()
        if line.startswith("path") and "=" in line:
            value = line.split("=", 1)[1].strip().strip("'\"")
            if value:
                return value
    raise SystemExit(f"no profile path found in {CONFIG}")


def read_user_token(profile: str) -> str:
    ls_db = Path(profile) / "storage/default/https+++chat.deepseek.com/ls/data.sqlite"
    if not ls_db.is_file():
        raise SystemExit(f"localStorage db not found: {ls_db}")
    tmp = Path(tempfile.mkdtemp(prefix="firecode_probe_"))
    try:
        shutil.copy(ls_db, tmp / "data.sqlite")
        for sidecar in ("-wal", "-shm"):
            src = Path(str(ls_db) + sidecar)
            if src.is_file():
                shutil.copy(src, tmp / f"data.sqlite{sidecar}")
        conn = sqlite3.connect(tmp / "data.sqlite")
        row = conn.execute(
            "SELECT value, conversion_type, compression_type FROM data WHERE key='userToken'"
        ).fetchone()
        conn.close()
        if row is None:
            raise SystemExit("userToken not found in localStorage")
        raw, conversion, compression = row
        if compression != 0:
            raise SystemExit(f"userToken is LZ4-compressed (type={compression})")
        if conversion == 1:
            value = bytes(raw).decode("latin-1")
        elif conversion == 0:
            value = bytes(raw).decode("utf-16-le")
        else:
            raise SystemExit(f"unknown conversion_type={conversion}")
        # Firefox stores localStorage strings as {"value": "..."} wrappers here.
        value = value.strip()
        if value.startswith("{"):
            try:
                parsed = json.loads(value)
                inner = parsed.get("value")
                if isinstance(inner, str):
                    value = inner
                elif isinstance(inner, dict) and isinstance(inner.get("value"), str):
                    value = inner["value"]
            except json.JSONDecodeError:
                pass
        return value
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def request(method: str, url: str, token: str | None = None, body: dict | None = None,
            extra_headers: dict | None = None) -> tuple[int, str, dict]:
    headers = {"User-Agent": UA, "Accept": "application/json, text/plain, */*",
               "Accept-Language": "en-US,en;q=0.9", "Origin": BASE,
               "Referer": BASE + "/"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if body is not None:
        headers["Content-Type"] = "application/json"
    if extra_headers:
        headers.update(extra_headers)
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return resp.status, resp.read().decode("utf-8", "replace"), dict(resp.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace"), dict(e.headers)
    except Exception as e:  # noqa: BLE001 - probe must not die on network errors
        return -1, f"{type(e).__name__}: {e}", {}


M64 = (1 << 64) - 1
KECCAK_RC = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
    0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
]
KECCAK_ROT = [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44]
KECCAK_PIL = [10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1]


def _rotl64(x: int, n: int) -> int:
    return ((x << n) | (x >> (64 - n))) & M64


def keccak_f23(st: list) -> None:
    """Reduced Keccak-f[1600]: 23 rounds (RC[0] skipped), per DeepSeekHashV1."""
    for rnd in range(1, 24):
        bc = [st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20] for i in range(5)]
        for i in range(5):
            t = bc[(i + 4) % 5] ^ _rotl64(bc[(i + 1) % 5], 1)
            for j in range(0, 25, 5):
                st[j + i] ^= t
        t = st[1]
        for i in range(24):
            j = KECCAK_PIL[i]
            tmp = st[j]
            st[j] = _rotl64(t, KECCAK_ROT[i])
            t = tmp
        for j in range(0, 25, 5):
            bc = st[j:j + 5]
            for i in range(5):
                st[j + i] = bc[i] ^ ((~bc[(i + 1) % 5] & M64) & bc[(i + 2) % 5])
        st[0] ^= KECCAK_RC[rnd]


def deepseek_hash_v1(msg: bytes) -> bytes:
    rate = 136
    st = [0] * 25
    blk = bytearray(msg)
    blk.append(0x06)
    while len(blk) < rate:
        blk.append(0x00)
    blk[rate - 1] |= 0x80
    for i in range(rate // 8):
        st[i] ^= int.from_bytes(blk[i * 8:(i + 1) * 8], "little")
    keccak_f23(st)
    return b"".join(st[i].to_bytes(8, "little") for i in range(4))


def solve_pow(challenge: dict) -> int:
    target = bytes.fromhex(challenge["challenge"])
    prefix = f"{challenge['salt']}_{challenge['expire_at']}_".encode()
    started = time.time()
    nonce = 0
    while True:
        if deepseek_hash_v1(prefix + str(nonce).encode()) == target:
            print(f"    pow solved: nonce={nonce} "
                  f"({time.time() - started:.1f}s, pure python)")
            return nonce
        nonce += 1
        if nonce > 50_000_000:
            raise SystemExit("pow: nonce not found, giving up")


def pow_header(challenge: dict, nonce: int) -> str:
    answer = {
        "algorithm": challenge["algorithm"],
        "challenge": challenge["challenge"],
        "salt": challenge["salt"],
        "answer": nonce,
        "signature": challenge["signature"],
        "target_path": challenge["target_path"],
    }
    return base64.b64encode(json.dumps(answer).encode()).decode()


def request_json(method: str, url: str, token: str, body: dict,
                 extra_headers: dict | None = None) -> tuple[int, str, dict]:
    return request(method, url, token=token, body=body,
                   extra_headers=extra_headers)


def summarize(status: int, text: str, headers: dict, label: str, limit: int = 400) -> None:
    ct = headers.get("Content-Type", "?")
    server = headers.get("Server", "?")
    set_cookie = headers.get("Set-Cookie", "")
    print(f"--- {label}: HTTP {status} (ct={ct}, server={server})")
    if set_cookie:
        print(f"    set-cookie: {set_cookie[:120]}")
    snippet = " ".join(text.split())[:limit]
    print(f"    body: {snippet if snippet else '<empty>'}")


def main() -> None:
    profile = read_config_profile()
    print(f"profile: {profile}")
    token = read_user_token(profile)
    print(f"userToken: <hidden, {len(token)} chars>")

    extra = {"x-client-platform": "web", "x-app-version": "20240101.0"}

    # 1. Create a chat session (also proves the token is accepted).
    status, text, headers = request("POST", BASE + "/api/v0/chat_session/create",
                                    token=token, body={"agent": "chat"},
                                    extra_headers=extra)
    summarize(status, text, headers, "POST /api/v0/chat_session/create")
    session_id = None
    try:
        session_id = json.loads(text)["data"]["biz_data"]["id"]
    except (json.JSONDecodeError, KeyError, TypeError):
        pass
    if not session_id:
        raise SystemExit("no session id, cannot continue")

    # 2. Fetch the proof-of-work challenge and solve it.
    status, text, headers = request("POST", BASE + "/api/v0/chat/create_pow_challenge",
                                    token=token,
                                    body={"target_path": "/api/v0/chat/completion"},
                                    extra_headers=extra)
    summarize(status, text, headers, "POST /api/v0/chat/create_pow_challenge", limit=300)
    challenge = json.loads(text)["data"]["biz_data"]["challenge"]
    nonce = solve_pow(challenge)

    # 3. Completion with the real session id + pow header.
    body = {"chat_session_id": session_id,
            "prompt": "Reply with exactly: pong",
            "ref_file_ids": [],
            "parent_message_id": None,
            "thinking_enabled": False,
            "model_class": "deepseek_chat"}
    extra_pow = dict(extra)
    extra_pow["x-ds-pow-response"] = pow_header(challenge, nonce)
    status, text, headers = request("POST", BASE + "/api/v0/chat/completion",
                                    token=token, body=body, extra_headers=extra_pow)
    ct = headers.get("Content-Type", "?")
    summarize(status, text, headers,
              f"POST completion (session {session_id}, pow)", limit=800)
    print(f"    stream: {'text/event-stream' in ct}")

    # 3. Auth sanity check: an intentionally broken token must differ.
    status, text, headers = request("POST", BASE + "/api/v0/chat/completion",
                                    token="x" * len(token), body=body,
                                    extra_headers=extra)
    summarize(status, text, headers, "POST completion (broken token, control)")


if __name__ == "__main__":
    sys.exit(main())

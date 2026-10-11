"""DeepSeek SSE stream + session endpoints investigation (phase 2 research).

Dumps the RAW completion stream to see how it terminates (fixes truncation),
then probes candidate endpoints for listing existing chats and reading their
history (the /a/chat/s/<uuid> links).

Secrets policy: the token value is never printed.

Usage: py -3 probe_deepseek_sessions.py
"""

import base64
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

HOME = Path(os.environ["USERPROFILE"])
CONFIG = HOME / "AppData/Roaming/0softwaredevelopment0/FireCode/config/config.toml"
BASE = "https://chat.deepseek.com"
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:145.0) Gecko/20100101 Firefox/145.0"
TARGET = "/api/v0/chat/completion"


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
    tmp = Path(tempfile.mkdtemp(prefix="firecode_probe2_"))
    try:
        shutil.copy(ls_db, tmp / "d.sqlite")
        conn = sqlite3.connect(tmp / "d.sqlite")
        raw, conversion = conn.execute(
            "SELECT value, conversion_type FROM data WHERE key='userToken'"
        ).fetchone()
        conn.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    value = bytes(raw).decode("latin-1" if conversion == 1 else "utf-16-le").strip()
    if value.startswith("{"):
        value = json.loads(value)["value"]
    return value


# --- DeepSeekHashV1 (reduced keccak, ported in phase 1b) ---

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
M64 = (1 << 64) - 1


def _rol(x, n):
    return ((x << n) | (x >> (64 - n))) & M64


def _permute(st):
    for rnd in range(1, 24):
        bc = [st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20] for i in range(5)]
        for i in range(5):
            t = bc[(i + 4) % 5] ^ _rol(bc[(i + 1) % 5], 1)
            for j in range(0, 25, 5):
                st[j + i] ^= t
        t = st[1]
        for i in range(24):
            j = KECCAK_PIL[i]
            st[j], t = _rol(t, KECCAK_ROT[i]), st[j]
        for j in range(0, 25, 5):
            bc = st[j:j + 5]
            for i in range(5):
                st[j + i] = bc[i] ^ ((~bc[(i + 1) % 5] & M64) & bc[(i + 2) % 5])
        st[0] ^= KECCAK_RC[rnd]


def ds_hash(msg: bytes) -> bytes:
    rate = 136
    st = [0] * 25
    blk = bytearray(msg)
    blk.append(0x06)
    blk += bytes(rate - len(blk))
    blk[rate - 1] |= 0x80
    for i in range(rate // 8):
        st[i] ^= int.from_bytes(blk[i * 8:(i + 1) * 8], "little")
    _permute(st)
    return b"".join(st[i].to_bytes(8, "little") for i in range(4))


def solve_pow(ch: dict) -> int:
    target = bytes.fromhex(ch["challenge"])
    prefix = f"{ch['salt']}_{ch['expire_at']}_".encode()
    nonce = 0
    while True:
        if ds_hash(prefix + str(nonce).encode()) == target:
            return nonce
        nonce += 1


def pow_header(ch: dict, nonce: int) -> str:
    answer = {"algorithm": ch["algorithm"], "challenge": ch["challenge"],
              "salt": ch["salt"], "answer": nonce, "signature": ch["signature"],
              "target_path": ch["target_path"]}
    return base64.b64encode(json.dumps(answer).encode()).decode()


class Api:
    def __init__(self, token: str):
        self.token = token

    def req(self, method: str, path: str, body=None, extra=None, timeout=30):
        headers = {"User-Agent": UA, "Content-Type": "application/json",
                   "Authorization": f"Bearer {self.token}",
                   "x-client-platform": "web", "x-app-version": "20240101.0",
                   "Accept": "text/event-stream, application/json"}
        if extra:
            headers.update(extra)
        data = json.dumps(body).encode() if body is not None else None
        r = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
        try:
            resp = urllib.request.urlopen(r, timeout=timeout)
            return resp.status, resp.read(), dict(resp.headers)
        except urllib.error.HTTPError as e:
            return e.code, e.read(), dict(e.headers)

    def json_body(self, status: int, raw: bytes, _headers=None):
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError:
            print(f"    HTTP {status}, non-json body: {raw[:200]!r}")
            return None
        return parsed


def completion_stream(api: Api, session_id: str, prompt: str, parent=None,
                      raw_dump: bool = False) -> tuple[str, int | None]:
    ch = api.json_body(*api.req("POST", "/api/v0/chat/create_pow_challenge",
                                {"target_path": TARGET}))["data"]["biz_data"]["challenge"]
    header = pow_header(ch, solve_pow(ch))
    body = {"chat_session_id": session_id, "prompt": prompt, "ref_file_ids": [],
            "parent_message_id": parent, "thinking_enabled": False,
            "model_class": "deepseek_chat"}
    status, raw, _ = api.req("POST", TARGET, body, {"x-ds-pow-response": header}, timeout=180)
    if status != 200:
        print(f"    completion HTTP {status}: {raw[:300]!r}")
        return "", None
    text_acc = ""
    last_id = None
    last_append_is_content = False
    event = None
    for raw_line in raw.decode("utf-8", "replace").splitlines():
        line = raw_line.strip()
        if line.startswith("event:"):
            event = line[6:].strip()
            if raw_dump:
                print(f"  [event] {event}")
            continue
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload:
            continue
        if raw_dump:
            print(f"  [{event or 'data'}] {payload[:160]}")
        try:
            obj = json.loads(payload)
        except json.JSONDecodeError:
            continue
        if "response_message_id" in obj:
            last_id = obj["response_message_id"]
        v = obj.get("v")
        if isinstance(v, dict) and "response" in v:
            frags = v["response"].get("fragments", [])
            for frag in reversed(frags):
                if frag.get("type") == "RESPONSE":
                    text_acc = frag.get("content", "")
                    break
            continue
        p, o = obj.get("p"), obj.get("o")
        if o == "APPEND" and p:
            last_append_is_content = p.endswith("content")
            if last_append_is_content:
                text_acc += obj.get("v", "")
        elif isinstance(v, str) and "p" not in obj:
            # bare {"v": "..."} continues the last APPEND target
            if last_append_is_content:
                text_acc += v
        elif o == "BATCH":
            for item in obj.get("v", []):
                ip, io = item.get("p"), item.get("o")
                if io == "APPEND" and ip and ip.endswith("content"):
                    text_acc += item.get("v", "")
    return text_acc, last_id


def main() -> None:
    profile = read_config_profile()
    api = Api(read_user_token(profile))
    print("token loaded (hidden)")

    # --- 1. Raw stream dump: how does a longer answer terminate? ---
    status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
    session_id = api.json_body(status, raw)["data"]["biz_data"]["id"]
    print(f"session: {session_id}")
    print("--- raw stream (truncated to 160 chars/line) ---")
    answer, msg_id = completion_stream(
        api, session_id,
        "In one paragraph: what model are you? Answer in Russian.",
        raw_dump=True)
    print(f"--- accumulated answer ({len(answer)} chars): {answer[:300]!r}")
    print(f"--- last response_message_id: {msg_id}")

    # --- 2. Second message with parent id: does context continue? ---
    answer2, msg_id2 = completion_stream(
        api, session_id, "Repeat your first word from the previous answer.", parent=msg_id)
    print(f"--- second answer: {answer2[:200]!r} (id {msg_id2})")

    # --- 3. Candidate session-list endpoints (the /a/chat/s/<uuid> chats) ---
    candidates = [
        ("GET", "/api/v0/chat_sessions?offset=0&limit=10&agent=chat", None),
        ("GET", "/api/v0/chat_session/list?offset=0&limit=10&agent=chat", None),
        ("POST", "/api/v0/chat_session/list", {"agent": "chat", "offset": 0, "limit": 10}),
        ("POST", "/api/v0/chat_sessions/list", {"agent": "chat", "offset": 0, "limit": 10}),
        ("POST", "/api/v0/chat_session/fetch", {"agent": "chat", "offset": 0, "limit": 10}),
    ]
    print("--- session list probes ---")
    for method, path, body in candidates:
        status, raw, _ = api.req(method, path, body)
        snippet = raw[:220].decode("utf-8", "replace").replace("\n", " ")
        print(f"  {method} {path} -> {status}: {snippet}")

    # --- 4. History of the session we just used ---
    hist_candidates = [
        ("GET", f"/api/v0/chat/messages?chat_session_id={session_id}", None),
        ("POST", "/api/v0/chat/messages", {"chat_session_id": session_id}),
        ("GET", f"/api/v0/chat_session/{session_id}/messages", None),
        ("GET", f"/api/v0/chat/history?chat_session_id={session_id}", None),
    ]
    print("--- history probes ---")
    for method, path, body in hist_candidates:
        status, raw, _ = api.req(method, path, body)
        snippet = raw[:220].decode("utf-8", "replace").replace("\n", " ")
        print(f"  {method} {path} -> {status}: {snippet}")


if __name__ == "__main__":
    main()

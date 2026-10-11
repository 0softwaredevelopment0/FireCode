"""Deterministic stop test: chunked stream reading, stop only when flowing."""

import base64
import json
import sys
import threading
import time
import urllib.request
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, str(Path(__file__).parent))
from probe_deepseek_sessions import Api, read_config_profile, read_user_token  # noqa: E402
from probe_deepseek_sessions import solve_pow, pow_header, TARGET  # noqa: E402

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:145.0) Gecko/20100101 Firefox/145.0"


def main() -> None:
    api = Api(read_user_token(read_config_profile()))
    status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
    sid = api.json_body(status, raw)["data"]["biz_data"]["id"]
    print(f"session: {sid}", flush=True)

    print("solving pow...", flush=True)
    ch = api.json_body(*api.req("POST", "/api/v0/chat/create_pow_challenge",
                                {"target_path": TARGET}))["data"]["biz_data"]["challenge"]
    header = pow_header(ch, solve_pow(ch))
    print("pow solved", flush=True)

    body = {"chat_session_id": sid,
            "prompt": "Write a very long essay about oceans, at least 2000 words. Start now.",
            "ref_file_ids": [], "parent_message_id": None,
            "thinking_enabled": False, "model_class": "deepseek_chat"}
    headers = {"User-Agent": UA, "Content-Type": "application/json",
               "Authorization": f"Bearer {api.token}",
               "x-client-platform": "web", "x-app-version": "20240101.0",
               "x-ds-pow-response": header}
    req = urllib.request.Request(BASE_URL + TARGET, data=json.dumps(body).encode(),
                                 headers=headers, method="POST")

    state = {"started": 0.0, "bytes": 0, "done": False}

    def stream():
        resp = urllib.request.urlopen(req, timeout=180)
        while True:
            chunk = resp.read(4096)
            if not chunk:
                break
            if state["started"] == 0.0:
                state["started"] = time.time()
            state["bytes"] += len(chunk)
        state["done"] = True

    t0 = time.time()
    t = threading.Thread(target=stream, daemon=True)
    t.start()
    while state["started"] == 0.0 and time.time() - t0 < 60:
        time.sleep(0.5)
    if state["started"] == 0.0:
        print("stream never started, aborting")
        return
    print(f"stream flowing at +{state['started'] - t0:.1f}s, waiting 5s then stopping...",
          flush=True)
    time.sleep(5)
    stop_at = time.time()
    status, raw, _ = api.req("POST", "/api/v0/chat/stop_stream",
                             {"chat_session_id": sid, "message_id": 2})
    print(f"stop reply: HTTP {status}: {raw[:120].decode('utf-8', 'replace')}", flush=True)
    t.join(timeout=120)
    print(f"stream ended +{time.time() - t0:.1f}s, stop sent +{stop_at - t0:.1f}s, "
          f"total bytes: {state['bytes']}, done={state['done']}", flush=True)


BASE_URL = "https://chat.deepseek.com"

if __name__ == "__main__":
    main()

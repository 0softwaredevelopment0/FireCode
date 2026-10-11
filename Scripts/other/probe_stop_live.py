"""Finds the working stop_stream variant against a live in-flight stream."""

import json
import sys
import threading
import time
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, str(Path(__file__).parent))
from probe_deepseek_sessions import Api, read_config_profile, read_user_token, completion_stream  # noqa: E402


def main() -> None:
    api = Api(read_user_token(read_config_profile()))
    status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
    sid = api.json_body(status, raw)["data"]["biz_data"]["id"]
    print(f"session: {sid}", flush=True)

    result = {}

    def stream():
        answer, last = completion_stream(
            api, sid,
            "Write a very long essay about oceans, at least 2000 words. Start now.")
        result["answer_len"] = len(answer)
        result["last_id"] = last

    t = threading.Thread(target=stream, daemon=True)
    t.start()
    # NOTE: completion_stream solves the Python POW first (~36s), the SSE
    # itself starts only after that - wait it out before stopping.
    time.sleep(50)  # let POW solve + stream get going

    for label, body in [
        ("parent_message_id=2", {"chat_session_id": sid, "parent_message_id": 2}),
        ("message_id=2", {"chat_session_id": sid, "message_id": 2}),
        ("request_message_id=1", {"chat_session_id": sid, "request_message_id": 1}),
        ("response_message_id=2", {"chat_session_id": sid, "response_message_id": 2}),
    ]:
        status, raw, _ = api.req("POST", "/api/v0/chat/stop_stream", body)
        print(f"--- {label}: HTTP {status}: {raw[:160].decode('utf-8', 'replace')}", flush=True)
        time.sleep(1)

    t.join(timeout=120)
    print("stream thread done:", {k: (v if not isinstance(v, str) or len(v) < 60 else f"{len(v)} chars") for k, v in result.items()})


if __name__ == "__main__":
    main()

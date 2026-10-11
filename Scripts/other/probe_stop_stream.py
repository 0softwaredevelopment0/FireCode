"""Probes chat/stop_stream request shapes."""

import json
import sys
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, str(Path(__file__).parent))
from probe_deepseek_sessions import Api, read_config_profile, read_user_token  # noqa: E402


def main() -> None:
    api = Api(read_user_token(read_config_profile()))
    status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
    sid = api.json_body(status, raw)["data"]["biz_data"]["id"]
    print(f"session: {sid}")
    for label, body in [
        ("chat_session_id", {"chat_session_id": sid}),
        ("empty", {}),
        ("session_id", {"session_id": sid}),
    ]:
        status, raw, _ = api.req("POST", "/api/v0/chat/stop_stream", body)
        print(f"--- {label}: HTTP {status}: {raw[:200].decode('utf-8', 'replace')}")


if __name__ == "__main__":
    main()

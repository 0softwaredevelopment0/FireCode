"""Typo check: does the server itself send 'Пял' or does our accumulator drop chars?"""

import sys
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, str(Path(__file__).parent))
from probe_deepseek_sessions import Api, read_config_profile, read_user_token, completion_stream  # noqa: E402


def main() -> None:
    api = Api(read_user_token(read_config_profile()))
    for i in range(3):
        status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
        session_id = api.json_body(status, raw)["data"]["biz_data"]["id"]
        answer, _ = completion_stream(
            api, session_id, "Во 1 говори на русском")
        print(f"run {i}: accumulated={answer!r}")


if __name__ == "__main__":
    main()

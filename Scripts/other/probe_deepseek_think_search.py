"""Dumps raw SSE for DeepThink (reasoning) and Search modes.

Answers two questions:
  1. which body fields enable reasoning (thinking_enabled / model_class) and
     how THINKING vs RESPONSE fragments stream (fragment creation, -1 paths);
  2. which field enables web search and what its stream looks like.

Usage: py -3 probe_deepseek_think_search.py
"""

import json
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, str(__import__("pathlib").Path(__file__).parent))
from probe_deepseek_sessions import Api, read_config_profile, read_user_token  # noqa: E402

TARGET = "/api/v0/chat/completion"


def dump_stream(api: Api, session_id: str, label: str, body: dict) -> None:
    ch = api.json_body(*api.req("POST", "/api/v0/chat/create_pow_challenge",
                                {"target_path": TARGET}))["data"]["biz_data"]["challenge"]
    from probe_deepseek_sessions import solve_pow, pow_header
    header = pow_header(ch, solve_pow(ch))
    status, raw, _ = api.req("POST", TARGET, body, {"x-ds-pow-response": header}, timeout=180)
    print(f"=== {label}: HTTP {status}")
    if status != 200:
        print(f"    body: {raw[:400].decode('utf-8', 'replace')}")
        return
    print(f"    ct: dict has stream? first 200 bytes: {raw[:200]!r}")
    for raw_line in raw.decode("utf-8", "replace").splitlines():
        line = raw_line.strip()
        if line.startswith("event:"):
            print(f"  [event] {line[6:].strip()}")
            continue
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload:
            continue
        print(f"  {payload[:220]}")


def main() -> None:
    api = Api(read_user_token(read_config_profile()))

    # mode probes share one session each (fresh context)
    status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
    sid_think = api.json_body(status, raw)["data"]["biz_data"]["id"]

    print("##### 1. thinking_enabled=true, model_class=deepseek_chat")
    dump_stream(api, sid_think, "think/chat", {
        "chat_session_id": sid_think,
        "prompt": "What is 15*17? Think briefly, then answer.",
        "ref_file_ids": [],
        "parent_message_id": None,
        "thinking_enabled": True,
        "model_class": "deepseek_chat",
    })

    status, raw, _ = api.req("POST", "/api/v0/chat_session/create", {"agent": "chat"})
    sid_search = api.json_body(status, raw)["data"]["biz_data"]["id"]

    print("##### 2. search_enabled=true")
    dump_stream(api, sid_search, "search", {
        "chat_session_id": sid_search,
        "prompt": "What is the current stable version of Rust?",
        "ref_file_ids": [],
        "parent_message_id": None,
        "thinking_enabled": False,
        "search_enabled": True,
        "model_class": "deepseek_chat",
    })


if __name__ == "__main__":
    main()

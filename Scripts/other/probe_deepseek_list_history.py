"""Probes the real session-list and history endpoints found in the JS bundle."""

import json
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, str(__import__("pathlib").Path(__file__).parent))
from probe_deepseek_sessions import Api, read_config_profile, read_user_token  # noqa: E402


def main() -> None:
    api = Api(read_user_token(read_config_profile()))

    print("--- users/current ---")
    status, raw, _ = api.req("GET", "/api/v0/users/current")
    body = api.json_body(status, raw)
    if body:
        print(f"  code={body.get('code')} user fields: {sorted((body.get('data') or {}).keys())}")

    print("--- chat_session/fetch_page variants ---")
    variants = [
        ("GET", "/api/v0/chat_session/fetch_page?lte_cursor.pinned=false", None),
        ("GET", "/api/v0/chat_session/fetch_page?lte_cursor.pinned=false&limit=10", None),
        ("GET", "/api/v0/chat_session/fetch_page?limit=10", None),
    ]
    for method, path, body in variants:
        status, raw, _ = api.req(method, path, body)
        parsed = api.json_body(status, raw)
        if parsed is None:
            print(f"  {path} -> HTTP {status}: non-json")
            continue
        print(f"  {path} -> code={parsed.get('code')}")
        data = parsed.get("data") or {}
        biz = data.get("biz_data") or {}
        print(f"    biz_data keys: {list(biz.keys())}")
        items = biz.get("items") or biz.get("chat_sessions") or []
        if items:
            print(f"    first item keys: {list(items[0].keys())}")
            for it in items[:5]:
                print(
                    f"    - {it.get('id')} | title={it.get('title')!r} "
                    f"| pinned={it.get('pinned')} | updated={it.get('updated_at')}"
                )
            break

    print("--- history_messages ---")
    status, raw, _ = api.req(
        "GET", "/api/v0/chat_session/fetch_page?lte_cursor.pinned=false&limit=5"
    )
    parsed = api.json_body(status, raw)
    items = ((parsed or {}).get("data") or {}).get("biz_data", {}).get("chat_sessions") or []
    if not items:
        print("  no sessions found for history test")
        return
    sid = items[0]["id"]
    status, raw, _ = api.req("GET", f"/api/v0/chat/history_messages?chat_session_id={sid}")
    parsed = api.json_body(status, raw)
    print(f"  session {sid} -> code={parsed.get('code')}")
    data = (parsed.get("data") or {}).get("biz_data") or {}
    print(f"  biz_data keys: {list(data.keys())}")
    msgs = data.get("chat_messages") or data.get("messages") or []
    print(f"  messages: {len(msgs)}")
    if msgs:
        print(f"  first msg keys: {list(msgs[0].keys())}")
        for m in msgs[:4]:
            content = (m.get("content") or "")[:60].replace("\n", " ")
            print(f"    - role={m.get('role')} content={content!r}")


if __name__ == "__main__":
    main()

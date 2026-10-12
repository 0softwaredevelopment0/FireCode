# FireCode

**FireCode** is a desktop client that turns AI chat websites (e.g. DeepSeek) into a full-blown
coding agent. The app reads the authentication token straight from the local browser
profile (Firefox / Chrome / Yandex), talks to the DeepSeek API and executes commands
in the PC terminal, feeding their output back into the conversation.

Current version: 0.2.1-beta.1.

## Features

- 💬 **DeepSeek chat** — streaming responses (SSE), thinking mode, search, model preset selection.
- 🤖 **Agent mode** — the model proposes commands in the `<command:"...">` format, FireCode runs them and feeds the output back into the chat as feedback.
- 🔁 **Auto-continue loop** — after every reply FireCode automatically prompts the model with *"Continue, to stop this response, use \<stop\> in your message on new line."* The loop only stops when the model writes `<stop>` on a new line anywhere in its message.
- 🔐 **Token reading from the browser** — Firefox (localStorage/SQLite), Chrome and Yandex (DPAPI + AES-256-GCM for cookies, LevelDB for localStorage).
- 🖥️ **egui/eframe GUI** — lightweight native interface, targeting Windows.
- 📝 **Markdown rendering** — code, lists, tables, emphasis.
- 💾 **Local chat cache** — every chat (including command/feedback windows) is cached per session, so reopening a chat — even after an app restart — looks exactly like before.
- 📎 **AGENTS.md / PROMPT.md** — custom agent instructions next to the exe; AGENTS.md can be prepended to every message.
- 📦 **Portability** — settings and config live in `%APPDATA%\0softwaredevelopment0\FireCode`.

## Requirements

- **Windows 10/11** (primary platform).
- **Rust** (edition 2024) and Cargo — to build from source.
- A browser (Firefox, Chrome or Yandex) with an active session on chat.deepseek.com.

## Building

```bash
git clone https://github.com/0softwaredevelopment0/FireCode.git
cd FireCode
cargo build --release
```

The resulting binary is `target/release/firecode.exe`.

## Usage

1. Run `firecode.exe`.
2. In Settings, pick the browser to take the token from and, if needed, the profile path.
3. The app reads the DeepSeek token from the browser profile.
4. Type a request. In agent mode the model may reply with lines like `<command:"git status">` — FireCode executes them and feeds the output back into the chat.
5. Type a request. In agent mode the model may reply with lines like `<command:"git status">` — FireCode executes them and feeds the output back into the chat.
6. The agent loop keeps going: after every reply FireCode sends the *"Continue, to stop this response..."* prompt automatically. The model ends its final message with a `<stop>` line to close the turn — no continue marker needs to be remembered.

### Working files next to the exe

| File | Purpose |
|------|---------|
| PROMPT.md | Agent instructions (created automatically, editable). The 📄 Prompt button sends its content as a request. |
| AGENTS.md | Custom project instructions. When the Include AGENTS.md option is on, they are prepended to every message. |

### Configuration

- `%APPDATA%\0softwaredevelopment0\FireCode\config.toml` — sites, presets, agent parameters.
- `%APPDATA%\0softwaredevelopment0\FireCode\settings.toml` — selected browser and agent options.
- `%APPDATA%\0softwaredevelopment0\FireCode\chat-cache\` — per-session chat cache (JSON).

Agent parameters (config.toml):

| Parameter | Description | Default |
|-----------|-------------|---------|
| agent.shell | Shell: powershell or cmd | powershell |
| agent.timeout_secs | Per-command timeout, seconds | 300 |
| agent.max_steps | Maximum agent steps | 25 |

Settings (settings.toml):

| Parameter | Description | Default |
|-----------|-------------|---------|
| browser.preset | Token source: firefox, chrome, yandex | firefox |
| agent.allow_sudo | Allow commands starting with sudo | false |
| agent.include_agents_md | Prepend AGENTS.md to every message | false |
| agent.working_dir | Working directory for commands (`local` = exe folder) | local |

## Architecture

| File | Role |
|------|------|
| src/main.rs | Entry point, GUI (egui), chat worker, event handling. |
| src/agent.rs | Parsing `<command>` / `<continue>`, running commands in a fresh shell, building feedback. |
| src/chatcache.rs | Per-session local chat cache (JSON). |
| src/deepseek.rs | DeepSeek HTTP client (SSE streaming, POW challenge, sessions, completions). |
| src/pow.rs | Solving the DeepSeek proof-of-work challenge. |
| src/auth.rs | Token extraction: Firefox localStorage, unwrapping value wrappers. |
| src/chromium.rs | Chrome/Yandex: LevelDB localStorage, DPAPI, AES-256-GCM cookies. |
| src/config.rs | config.toml: sites, presets, agent parameters, Firefox profile discovery. |
| src/settings.rs | settings.toml: browser preset and agent options. |
| src/markdown.rs | Minimal Markdown parser for rendering. |
| src/fonts.rs | Font loading for the GUI. |

## Command execution details

- Every command runs in a fresh shell (`cd` does not persist between commands — combine steps on one line).
- Commands run fully in the background (CREATE_NO_WINDOW), no terminal window pops up.
- Output is truncated to 32,000 characters and fed back to the model as `Command "<cmd>":` blocks with the exit code.
- Commands with the `sudo` prefix are blocked unless the agent.allow_sudo option is on.

## License

GNU Affero General Public License v3.0 (AGPL-3.0). See LICENSE.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// Maximum characters of a command output fed back into the chat.
const MAX_OUTPUT_CHARS: usize = 32_000;

#[derive(Debug, Clone)]
pub struct ParsedResponse {
    /// Visible message text with command/continue lines removed.
    pub text: String,
    pub commands: Vec<String>,
    pub continue_next: bool,
}

/// Unescapes a double-quoted command string: `\"` → `"`, `\\` → `\`,
/// `\n` → newline, `\t` → tab, `\r` → carriage return. Unknown escapes and
/// a trailing lone backslash stay literal.
fn unescape_command_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Extracts single-line `<command:"...">` commands, multiline
/// `<command-header>` / `<string:"...">` (or `<string:'...'>`) /
/// `<command-footer>` blocks and a trailing `<continue>` marker from a
/// response. Anything else stays in `text`. A multiline block runs as one
/// command (lines joined with `\n`). Lines inside ``` fenced code blocks
/// are never treated as commands or markers - fences shield documentation.
pub fn parse_response(raw: &str) -> ParsedResponse {
    let mut text = String::new();
    let mut commands = Vec::new();
    let mut continue_next = false;
    let mut multi: Option<Vec<String>> = None;
    let mut in_fence = false;

    let flush_multi = |multi: &mut Option<Vec<String>>, commands: &mut Vec<String>| {
        if let Some(mut block) = multi.take() {
            while block.first().is_some_and(|line| line.is_empty()) {
                block.remove(0);
            }
            while block.last().is_some_and(|line| line.is_empty()) {
                block.pop();
            }
            let joined = block.join("\n");
            if !joined.trim().is_empty() {
                commands.push(joined);
            }
        }
    };

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            text.push_str(line);
            text.push('\n');
            continue;
        }
        if in_fence {
            text.push_str(line);
            text.push('\n');
            continue;
        }
        if multi.is_some() {
            // a new header always starts a new command, even if the previous
            // block missed its footer - blocks can never merge this way
            if trimmed == "<command-header>" {
                flush_multi(&mut multi, &mut commands);
                multi = Some(Vec::new());
            } else if trimmed == "<command-footer>" {
                flush_multi(&mut multi, &mut commands);
            } else if let Some(cmd) = extract_string_line(trimmed) {
                multi
                    .as_mut()
                    .expect("multiline block open")
                    .push(cmd);
            } else if let Some(cmd) = extract_single_command(trimmed) {
                flush_multi(&mut multi, &mut commands);
                commands.push(cmd);
            }
            continue;
        }
        if trimmed == "<command-header>" {
            multi = Some(Vec::new());
            continue;
        }
        if let Some(cmd) = extract_single_command(trimmed) {
            commands.push(cmd);
            continue;
        }
        if trimmed == "<continue>" {
            continue_next = true;
            continue;
        }
        text.push_str(line);
        text.push('\n');
    }
    // an unterminated block still runs what was collected
    flush_multi(&mut multi, &mut commands);

    ParsedResponse {
        text: text.trim_end().to_owned(),
        commands,
        continue_next,
    }
}

/// Extracts one `<command:"...">` line (with unescaping), if it matches.
fn extract_single_command(trimmed: &str) -> Option<String> {
    trimmed
        .strip_prefix("<command:\"")
        .and_then(|rest| rest.strip_suffix("\">"))
        .map(|inner| unescape_command_string(inner.trim()))
        .filter(|cmd| !cmd.is_empty())
}

/// Extracts one `<string:...>` line: the double-quoted form unescapes,
/// the single-quoted form is raw (free `"` embedding). Content is kept
/// exactly (indentation matters for files); empty lines are kept too so
/// blank lines survive - only a fully empty block is dropped at flush.
/// Returns None when the line is not a string line at all.
fn extract_string_line(trimmed: &str) -> Option<String> {
    if let Some(inner) = trimmed
        .strip_prefix("<string:\"")
        .and_then(|rest| rest.strip_suffix("\">"))
    {
        return Some(unescape_command_string(inner));
    }
    if let Some(inner) = trimmed
        .strip_prefix("<string:'")
        .and_then(|rest| rest.strip_suffix("'>"))
    {
        return Some(inner.to_owned());
    }
    None
}

/// Splits a feedback message (built by [`build_feedback`]) back into
/// `(header, output)` blocks: a line like `Command "cmd":` opens a block,
/// everything until the next such line (or EOF) is its output. Lines before
/// the first header are dropped (build_feedback emits none).
pub fn parse_feedback_blocks(text: &str) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(header) = trimmed
            .strip_prefix("Command \"")
            .and_then(|rest| rest.strip_suffix("\":"))
        {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some((format!("Command \"{header}\":"), String::new()));
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }
    blocks
}
/// Builds the feedback message sent back to the chat after command
/// execution: one `Command "...":` block per command.
pub fn build_feedback(results: &[(String, CommandOutput)]) -> String {
    let mut feedback = String::new();
    for (cmd, out) in results {
        feedback.push_str(&format!("Command \"{}\":\n", cmd));
        let output = out.output.trim_end();
        if output.is_empty() {
            feedback.push_str("(no output)\n");
        } else {
            feedback.push_str(output);
            feedback.push('\n');
        }
        match out.exit_code {
            Some(0) => {}
            Some(code) => feedback.push_str(&format!("(exit code {code})\n")),
            None => feedback.push_str("(did not exit)\n"),
        }
        if out.timed_out {
            feedback.push_str("(timed out)\n");
        }
    }
    feedback.trim_end().to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    PowerShell,
    Cmd,
}

/// True when the command asks for elevation via sudo.
pub fn looks_like_sudo(cmd: &str) -> bool {
    cmd.split_whitespace().next() == Some("sudo")
}

impl Shell {
    pub fn parse(s: &str) -> Shell {
        match s.trim().to_ascii_lowercase().as_str() {
            "cmd" => Shell::Cmd,
            _ => Shell::PowerShell,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Resolves the agent working directory: `"local"` (or empty) means the
/// directory the app was launched from (the exe directory); anything else
/// is used as a custom path as-is.
pub fn resolve_working_dir(configured: &str) -> PathBuf {
    let trimmed = configured.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("local") {
        return std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
    }
    PathBuf::from(trimmed)
}

/// Runs one command in a fresh shell and captures combined output.
pub async fn execute_command(
    cmd: &str,
    workdir: &Path,
    shell: Shell,
    timeout: Duration,
) -> Result<CommandOutput> {
    use tokio::process::Command;

    let mut command = match shell {
        Shell::PowerShell => {
            // force UTF-8 output so cyrillic text is not mojibake
            let full = format!(
                "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {}",
                cmd
            );
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &full]);
            c
        }
        Shell::Cmd => {
            let mut c = Command::new("cmd");
            c.args(["/C", cmd]);
            c
        }
    };
    command
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // a console child of a GUI app gets a visible window by default;
        // no flag inside the command text can prevent that, only the spawner
        // can - so every agent command runs fully headless.
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn shell for: {cmd}"))?;
    let timed_out;
    let output;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            timed_out = false;
            output = out;
        }
        Ok(Err(e)) => return Err(e).context(format!("command failed: {cmd}")),
        Err(_) => {
            timed_out = true;
            output = std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
        }
    }

    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    let truncated = combined.chars().count() > MAX_OUTPUT_CHARS;
    if truncated {
        combined = combined.chars().take(MAX_OUTPUT_CHARS).collect();
        combined.push_str("\n(...output truncated)");
    }

    Ok(CommandOutput {
        output: combined,
        exit_code: output.status.code(),
        timed_out,
    })
}

pub fn agents_md_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate current exe")?;
    Ok(exe
        .parent()
        .context("exe has no parent directory")?
        .join("AGENTS.md"))
}

/// Loads the user-owned AGENTS.md next to the exe, if present.
/// Missing file is not an error - the include option simply adds nothing.
pub fn load_agents_md_opt() -> Option<String> {
    let path = agents_md_path().ok()?;
    std::fs::read_to_string(path).ok()
}

pub fn prompt_md_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate current exe")?;
    Ok(exe
        .parent()
        .context("exe has no parent directory")?
        .join("PROMPT.md"))
}

pub const DEFAULT_PROMPT_MD: &str = r#"# FireCode agent instructions

You are a coding agent working inside FireCode, a desktop client that talks to
you through this chat. FireCode can execute terminal commands on the user's PC
on your behalf and send their output back to you as feedback.

## Tool 1: terminal commands

When you need to run something on the user's PC, append command lines to your
response. Each command must be on its own line, in exactly this syntax:

<command:"git status">

Rules:
- You may include several commands; they run sequentially, top to bottom.
- Every command runs in a FRESH shell in the working directory - `cd` does not
  persist between commands. Chain it inside one command if needed.
- Commands must be non-interactive: never ask for input, use flags like
  `-y` / `--yes`. Prefer PowerShell syntax (`;` instead of `&&`).
- Every command runs fully headless: no terminal window ever opens, you only
  receive captured text output. Never try to open windows, GUI apps, or
  anything interactive - there is no screen to show them on.
- After all commands finish you receive their results as a feedback message:
  `Command "cmd":` followed by the captured output (long output is
  truncated). That feedback triggers you again automatically, so you do NOT
  need `<continue>` alongside commands - if a message has both, the commands
  run and the `<continue>` is simply ignored.
- Keep the number of commands small and purposeful. Do not run destructive
  commands (deleting files, force pushes) unless the user explicitly asked.

## Tool 1b: multiline commands

When one command needs several lines, wrap them in a header/footer block.
Every line inside must use the `<string:"...">` syntax:

<command-header>
<string:"cd project">
<string:"cargo build --release">
<string:"cargo test">
<command-footer>

The lines run together as one multiline command, top to bottom. The same
rules as single-line commands apply (fresh shell, non-interactive, headless).
A new `<command-header>` always starts a new command, even if the previous
block missed its footer - blocks never merge.

### Quoting inside `<string:"...">` (and `<command:"...">`)

Backslash starts an escape sequence: `\"` → `"`, `\\` → `\`, `\n` →
newline, `\t` → tab, `\r` → carriage return. Unknown escapes stay literal.
Double every backslash in Windows paths (`C:\\notes`), or use the
single-quoted form below.

For quote-heavy content use the single-quoted form - no processing inside,
`"` embeds freely:

<string:'Write-Host "hi"'>

### Fences and blank lines

Empty `<string:"">` lines are kept, so blank lines survive in generated
files; leading spaces are preserved exactly. Lines inside ``` fenced code
blocks are never treated as commands or markers - use fences when showing
syntax examples.

## Tool 2: continue

Every user request is just an information output task, and one response may
not fit everything: if you have NOT finished what the user asked for - you
stopped mid-sentence, still owe part of the answer, or the task needs more
steps - end your message with a single line:

<continue>

FireCode will automatically send you "Continue" so you can pick up where you
stopped. Use `<continue>` whenever your answer is incomplete, not only when
you visibly "stopped on something".

## General

- Plain text without commands or `<continue>` ends the agent turn.
- Write commands exactly in the syntax shown - they are parsed line-by-line.
"#;

pub fn ensure_prompt_md() -> Result<PathBuf> {
    let path = prompt_md_path()?;
    if !path.exists() {
        std::fs::write(&path, DEFAULT_PROMPT_MD)
            .with_context(|| format!("failed to write {}", path.display()))?;
        return Ok(path);
    }
    // existing file from an older version: append the multiline tool docs
    // if the user has not added them (their own edits are kept as-is).
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut updated = raw;
    let mut changed = false;
    if !updated.contains("<command-header>") {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&multiline_tool_section());
        changed = true;
    }
    // quoting docs arrived one version later than the block syntax itself
    if !updated.contains("Quoting inside") {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&quoting_docs_section());
        changed = true;
    }
    // fence shielding and blank-line preservation came later still
    if !updated.contains("Fences and blank lines") {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&fences_docs_section());
        changed = true;
    }
    if changed {
        std::fs::write(&path, updated)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(path)
}

fn multiline_tool_section() -> &'static str {
    r#"
## Tool 1b: multiline commands

When one command needs several lines, wrap them in a header/footer block.
Every line inside must use the `<string:"...">` syntax:

<command-header>
<string:"cd project">
<string:"cargo build --release">
<string:"cargo test">
<command-footer>

The lines run together as one multiline command, top to bottom. The same
rules as single-line commands apply (fresh shell, non-interactive, headless).
"#
}

fn quoting_docs_section() -> &'static str {
    r#"
### Quoting inside `<string:"...">` (and `<command:"...">`)

Backslash starts an escape sequence: `\"` → `"`, `\\` → `\`, `\n` →
newline, `\t` → tab, `\r` → carriage return. Unknown escapes stay literal.
Double every backslash in Windows paths (`C:\\notes`), or use the
single-quoted form below.

For quote-heavy content use the single-quoted form - no processing inside,
`"` embeds freely:

<string:'Write-Host "hi"'>
"#
}

fn fences_docs_section() -> &'static str {
    r#"
### Fences and blank lines

Empty `<string:"">` lines are kept, so blank lines survive in generated
files; leading spaces are preserved exactly. Lines inside ``` fenced code
blocks are never treated as commands or markers - use fences when showing
syntax examples.
"#
}

pub fn load_prompt_md() -> Result<String> {
    let path = ensure_prompt_md()?;
    std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_with_commands() {
        let raw = "Привет\n<command:\"git status\">\nзапускаю\n<command:\"cargo build --release\">\nготово";
        let parsed = parse_response(raw);
        assert_eq!(
            parsed.commands,
            vec!["git status".to_owned(), "cargo build --release".to_owned()]
        );
        assert!(!parsed.continue_next);
        assert!(parsed.text.contains("Привет"));
        assert!(parsed.text.contains("готово"));
        assert!(!parsed.text.contains("<command"));
    }

    #[test]
    fn parse_continue_marker() {
        let parsed = parse_response("часть ответа\n<continue>");
        assert!(parsed.continue_next);
        assert!(parsed.commands.is_empty());
        assert_eq!(parsed.text, "часть ответа");
    }

    #[test]
    fn parse_plain_text() {
        let parsed = parse_response("just an answer\nsecond line");
        assert!(parsed.commands.is_empty());
        assert!(!parsed.continue_next);
        assert_eq!(parsed.text, "just an answer\nsecond line");
    }

    #[test]
    fn malformed_command_lines_stay_text() {
        let parsed = parse_response("<command:no-quotes>\n<command:\"\">");
        assert!(parsed.commands.is_empty());
        assert!(parsed.text.contains("<command:no-quotes>"));
    }

    #[test]
    fn feedback_blocks_round_trip() {
        let results = vec![
            (
                "git status".to_owned(),
                CommandOutput {
                    output: "On main".to_owned(),
                    exit_code: Some(0),
                    timed_out: false,
                },
            ),
            (
                "cargo test".to_owned(),
                CommandOutput {
                    output: "line1\nline2".to_owned(),
                    exit_code: Some(1),
                    timed_out: false,
                },
            ),
        ];
        let feedback = build_feedback(&results);
        let blocks = parse_feedback_blocks(&feedback);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "Command \"git status\":");
        assert_eq!(blocks[0].1, "On main");
        assert_eq!(blocks[1].0, "Command \"cargo test\":");
        assert!(blocks[1].1.contains("line1\nline2"));
    }

    #[test]
    fn feedback_blocks_empty_without_headers() {
        assert!(parse_feedback_blocks("just text\nno headers").is_empty());
    }

    #[test]
    fn feedback_format_matches_spec() {
        let results = vec![
            (
                "git status".to_owned(),
                CommandOutput {
                    output: "On main\nnothing to commit".to_owned(),
                    exit_code: Some(0),
                    timed_out: false,
                },
            ),
            (
                "cargo test".to_owned(),
                CommandOutput {
                    output: "test result: ok".to_owned(),
                    exit_code: Some(101),
                    timed_out: false,
                },
            ),
        ];
        let feedback = build_feedback(&results);
        assert!(feedback.contains("Command \"git status\":\nOn main\nnothing to commit"));
        assert!(feedback.contains("Command \"cargo test\":\ntest result: ok\n(exit code 101)"));
    }

    #[test]
    fn feedback_empty_output_and_timeout() {
        let results = vec![
            (
                "echo".to_owned(),
                CommandOutput {
                    output: "  ".to_owned(),
                    exit_code: Some(0),
                    timed_out: false,
                },
            ),
            (
                "slow".to_owned(),
                CommandOutput {
                    output: String::new(),
                    exit_code: None,
                    timed_out: true,
                },
            ),
        ];
        let feedback = build_feedback(&results);
        assert!(feedback.contains("Command \"echo\":\n(no output)"));
        assert!(feedback.contains("Command \"slow\":\n(no output)\n(did not exit)\n(timed out)"));
    }

    #[test]
    fn parse_string_escapes() {
        let raw = "<command-header>\n<string:\"Write-Host \\\"hi\\\", path C:\\\\notes\">\n<string:\"line1\\nline2\\ttab\\rend\">\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(
            parsed.commands,
            vec!["Write-Host \"hi\", path C:\\notes\nline1\nline2\ttab\rend".to_owned()]
        );
    }

    #[test]
    fn parse_single_quoted_string_is_raw() {
        let raw = "<command-header>\n<string:'Write-Host \"hi\" and C:\\notes\\n'>\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(
            parsed.commands,
            vec!["Write-Host \"hi\" and C:\\notes\\n".to_owned()]
        );
    }

    #[test]
    fn parse_unknown_escapes_stay_literal() {
        let parsed = parse_response("<command:\"echo a\\xb\\\">");
        assert_eq!(parsed.commands, vec!["echo a\\xb\\".to_owned()]);
    }

    #[test]
    fn parse_escaped_single_line_command() {
        let parsed = parse_response("<command:\"echo \\\"hi\\\"\">");
        assert_eq!(parsed.commands, vec!["echo \"hi\"".to_owned()]);
    }

    #[test]
    fn parse_header_without_footer_splits_blocks() {
        let raw = "<command-header>\n<string:\"echo a\">\n<command-header>\n<string:\"echo b\">\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(
            parsed.commands,
            vec!["echo a".to_owned(), "echo b".to_owned()]
        );
    }

    #[test]
    fn parse_single_command_inside_block_flushes() {
        let raw = "<command-header>\n<string:\"echo a\">\n<command:\"git status\">\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(
            parsed.commands,
            vec!["echo a".to_owned(), "git status".to_owned()]
        );
    }

    #[test]
    fn parse_block_keeps_blank_lines_and_indentation() {
        let raw = "<command-header>\n<string:\"line1\">\n<string:\"\">\n<string:\"    indented\">\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(parsed.commands, vec!["line1\n\n    indented".to_owned()]);
    }

    #[test]
    fn parse_fenced_markers_stay_text() {
        let raw = "text\n```\n<command:\"x\">\n<continue>\n```\nafter";
        let parsed = parse_response(raw);
        assert!(parsed.commands.is_empty());
        assert!(!parsed.continue_next);
        assert!(parsed.text.contains("<command:\"x\">"));
        assert!(parsed.text.contains("<continue>"));
    }

    #[test]
    fn parse_two_adjacent_blocks_are_two_commands() {
        let raw = "<command-header>\n<string:\"test\">\n<string:\"test\">\n<command-footer>\n<command-header>\n<string:\"test\">\n<string:\"test\">\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(
            parsed.commands,
            vec!["test\ntest".to_owned(), "test\ntest".to_owned()]
        );
    }

    #[test]
    fn parse_multiline_command_block() {
        let raw = "делаю\n<command-header>\n<string:\"cd project\">\n<string:\"cargo build\">\n<string:\"cargo test\">\n<command-footer>\nготово";
        let parsed = parse_response(raw);
        assert_eq!(parsed.commands, vec!["cd project\ncargo build\ncargo test".to_owned()]);
        assert!(!parsed.continue_next);
        assert_eq!(parsed.text, "делаю\nготово");
    }

    #[test]
    fn parse_multiline_mixed_with_single_and_continue() {
        let raw = "<command:\"git status\">\n<command-header>\n<string:\"echo a\">\n<command-footer>\n<continue>";
        let parsed = parse_response(raw);
        assert_eq!(parsed.commands, vec!["git status".to_owned(), "echo a".to_owned()]);
        assert!(parsed.continue_next);
        assert!(parsed.text.is_empty());
    }

    #[test]
    fn parse_unterminated_block_flushes() {
        let parsed = parse_response("<command-header>\n<string:\"echo hi\">");
        assert_eq!(parsed.commands, vec!["echo hi".to_owned()]);
    }

    #[test]
    fn parse_block_skips_empty_and_foreign_lines() {
        let raw = "<command-header>\n<string:\"\">\nмусор\n<string:\"echo ok\">\n<command-footer>";
        let parsed = parse_response(raw);
        assert_eq!(parsed.commands, vec!["echo ok".to_owned()]);
        assert!(parsed.text.is_empty());
    }

    #[test]
    fn parse_commands_and_continue_together() {
        let parsed = parse_response("<command:\"git status\">\ntext\n<continue>");
        assert_eq!(parsed.commands, vec!["git status".to_owned()]);
        assert!(parsed.continue_next);
        assert_eq!(parsed.text, "text");
    }

    #[test]
    fn sudo_detection() {
        assert!(looks_like_sudo("sudo apt update"));
        assert!(looks_like_sudo("  sudo rm -rf /"));
        assert!(!looks_like_sudo("echo sudo"));
        assert!(!looks_like_sudo("sudoers"));
        assert!(!looks_like_sudo(""));
    }

    #[test]
    fn resolve_working_dir_local_means_exe_dir() {
        let exe_dir = std::env::current_exe()
            .expect("exe")
            .parent()
            .expect("parent")
            .to_path_buf();
        assert_eq!(resolve_working_dir("local"), exe_dir);
        assert_eq!(resolve_working_dir("LOCAL"), exe_dir);
        assert_eq!(resolve_working_dir("  local  "), exe_dir);
        assert_eq!(resolve_working_dir(""), exe_dir);
        assert_eq!(
            resolve_working_dir("C:\\work"),
            std::path::PathBuf::from("C:\\work")
        );
    }

    /// Runs a real shell on this machine.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "spawns a real shell"]
    async fn execute_command_runs_and_captures() {
        let out = execute_command(
            "echo hello-agent",
            &resolve_working_dir(""),
            Shell::PowerShell,
            Duration::from_secs(30),
        )
        .await
        .expect("execute");
        assert!(out.output.contains("hello-agent"), "output: {}", out.output);
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
    }

    /// A command that outlives the timeout must be killed and reported.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "spawns a real shell and waits for a timeout"]
    async fn execute_command_times_out() {
        let started = std::time::Instant::now();
        let out = execute_command(
            "Start-Sleep -Seconds 30",
            &resolve_working_dir(""),
            Shell::PowerShell,
            Duration::from_secs(2),
        )
        .await
        .expect("execute");
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(15));
    }
}

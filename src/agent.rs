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

/// Extracts `<command:"...">` lines and a trailing `<continue>` marker from
/// a response. Anything else stays in `text`.
pub fn parse_response(raw: &str) -> ParsedResponse {
    let mut text = String::new();
    let mut commands = Vec::new();
    let mut continue_next = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(cmd) = trimmed
            .strip_prefix("<command:\"")
            .and_then(|rest| rest.strip_suffix("\">"))
        {
            if !cmd.trim().is_empty() {
                commands.push(cmd.trim().to_owned());
            }
            continue;
        }
        if trimmed == "<continue>" {
            continue_next = true;
            continue;
        }
        text.push_str(line);
        text.push('\n');
    }

    ParsedResponse {
        text: text.trim_end().to_owned(),
        commands,
        continue_next,
    }
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

pub fn resolve_working_dir(configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        return PathBuf::from(configured);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
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

pub const DEFAULT_AGENTS_MD: &str = r#"# FireCode agent instructions

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
- After all commands finish you receive their results as a feedback message:
  `Command "<command>":` followed by the captured output (long output is
  truncated). Analyze it, then either send more commands, continue your answer,
  or finish.
- Keep the number of commands small and purposeful. Do not run destructive
  commands (deleting files, force pushes) unless the user explicitly asked.

## Tool 2: continue

If your message got cut off or you have more to say but no commands to run,
end the message with a single line:

<continue>

FireCode will automatically send you "Continue" so you can pick up where you
stopped. `<continue>` and `<command:...>` must not be used in the same message.

## General

- Plain text without commands or `<continue>` ends the agent turn.
- Write commands exactly in the syntax shown - they are parsed line-by-line.
"#;

pub fn ensure_agents_md() -> Result<PathBuf> {
    let path = agents_md_path()?;
    if !path.exists() {
        std::fs::write(&path, DEFAULT_AGENTS_MD)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(path)
}

pub fn load_agents_md() -> Result<String> {
    let path = ensure_agents_md()?;
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

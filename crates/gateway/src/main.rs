use std::{io::Write, path::PathBuf, process::ExitCode};

use agentsflow_adapters::{
    Agent,
    claude::{ClaudeOptions, ClaudeSession},
    codex::{CodexOptions, CodexSession, SandboxMode},
};
use agentsflow_events::Event;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};

const USAGE: &str = "Usage: agentsflow <claude|codex> \"<prompt>\" [--model <m>] [--cwd <dir>] [--yes] [--sandbox <read-only|workspace-write>] [--codex-bin <path>]\n       agentsflow --version";

fn truncate(text: &str) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn format_event(agent: &str, event: &Event) -> String {
    match event {
        Event::Message { text, .. } => format!("{agent}> {text}"),
        Event::ToolCall { name, input, .. } => {
            format!("[tool] {name} {}", truncate(&input.to_string()))
        }
        Event::ToolResult {
            output, is_error, ..
        } => format!(
            "[result{}] {}",
            if *is_error { ":error" } else { "" },
            truncate(output)
        ),
        Event::PermissionRequest { tool, input, .. } => {
            format!("[permission] {tool} {}", truncate(&input.to_string()))
        }
        Event::Done { reason } => format!("[done] {reason}"),
        Event::Error { message } => format!("[error] {message}"),
        Event::Diff { path, patch } => format!("[diff] {path}\n{patch}"),
    }
}

struct Command {
    agent: String,
    prompt: String,
    model: Option<String>,
    cwd: Option<PathBuf>,
    yes: bool,
    sandbox: Option<SandboxMode>,
    bin: Option<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Option<Command> {
    let mut args = args.into_iter();
    let agent = args.next()?;
    if agent != "claude" && agent != "codex" {
        return None;
    }
    let prompt = args.next()?;
    if prompt.starts_with("--") {
        return None;
    }
    let mut command = Command {
        agent,
        prompt,
        model: None,
        cwd: None,
        yes: false,
        sandbox: None,
        bin: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--yes" => command.yes = true,
            "--model" | "--cwd" | "--sandbox" | "--codex-bin" => {
                let value = args.next()?;
                if value.starts_with("--") || value.is_empty() {
                    return None;
                }
                match arg.as_str() {
                    "--model" => command.model = Some(value),
                    "--cwd" => command.cwd = Some(value.into()),
                    "--sandbox" if command.agent == "codex" => {
                        command.sandbox = Some(match value.as_str() {
                            "read-only" => SandboxMode::Read_only,
                            "workspace-write" => SandboxMode::Workspace_write,
                            _ => return None,
                        })
                    }
                    "--codex-bin" if command.agent == "codex" => command.bin = Some(value.into()),
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(command)
}

async fn run(command: Command) -> Result<()> {
    if command.agent == "claude" {
        let session = ClaudeSession::start(ClaudeOptions {
            model: command.model,
            cwd: command.cwd,
        })
        .await?;
        run_turn(session, "claude", &command.prompt, command.yes).await
    } else {
        let session = CodexSession::start(CodexOptions {
            model: command.model,
            cwd: command.cwd,
            sandbox: command.sandbox,
            bin: command.bin,
        })
        .await?;
        run_turn(session, "codex", &command.prompt, command.yes).await
    }
}

async fn run_turn<A: Agent>(mut session: A, agent: &str, prompt: &str, yes: bool) -> Result<()> {
    let result = drive_turn(&mut session, agent, prompt, yes).await;
    let shutdown = session.shutdown().await;
    result?;
    shutdown
}

async fn drive_turn<A: Agent>(session: &mut A, agent: &str, prompt: &str, yes: bool) -> Result<()> {
    session.send(prompt).await?;
    let mut input = BufReader::new(tokio::io::stdin());
    loop {
        let events = session.next_events().await?;
        for event in &events {
            if let Event::PermissionRequest { id, .. } = event {
                let allow = if yes {
                    println!("{} [auto-allowed]", format_event(agent, event));
                    true
                } else {
                    print!("{} Allow? [y/N] ", format_event(agent, event));
                    std::io::stdout().flush()?;
                    let mut answer = String::new();
                    input.read_line(&mut answer).await?;
                    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                };
                session.respond(id, allow).await?;
            } else {
                println!("{}", format_event(agent, event));
            }
        }
        if events
            .iter()
            .any(|event| matches!(event, Event::Done { .. }))
        {
            break;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("agentsflow {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let Some(command) = parse_args(args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match run(command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[error] {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_events::Role;

    #[test]
    fn formats_events() {
        let event = Event::Message {
            role: Role::Assistant,
            text: "hello".into(),
            delta: false,
        };
        assert_eq!(format_event("claude", &event), "claude> hello");
        assert_eq!(format_event("codex", &event), "codex> hello");
        assert_eq!(
            format_event(
                "codex",
                &Event::ToolResult {
                    id: "1".into(),
                    output: "bad".into(),
                    is_error: true
                }
            ),
            "[result:error] bad"
        );
        assert_eq!(
            format_event(
                "codex",
                &Event::Done {
                    reason: "completed".into()
                }
            ),
            "[done] completed"
        );
    }
    #[test]
    fn truncates_unicode() {
        let input = "я".repeat(201).into();
        let text = format_event(
            "codex",
            &Event::ToolCall {
                id: "1".into(),
                name: "Write".into(),
                input,
            },
        );
        assert!(text.starts_with("[tool] Write \""));
        assert!(text.ends_with("..."));
        assert_eq!(truncate(&"я".repeat(201)).chars().count(), 203);
    }
    #[test]
    fn parses_options_and_rejects_invalid_commands() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cmd = parse_args(args(&[
            "codex",
            "hello",
            "--model",
            "m",
            "--cwd",
            ".",
            "--yes",
            "--sandbox",
            "workspace-write",
            "--codex-bin",
            "bin",
        ]))
        .unwrap();
        assert_eq!(cmd.agent, "codex");
        assert_eq!(cmd.model.as_deref(), Some("m"));
        assert_eq!(cmd.sandbox, Some(SandboxMode::Workspace_write));
        assert_eq!(cmd.bin, Some(PathBuf::from("bin")));
        assert!(cmd.yes);
        assert!(parse_args(args(&["claude", "hello", "--model", "haiku"])).is_some());
        for input in [
            &[][..],
            &["unknown"][..],
            &["claude", "hi", "--sandbox", "read-only"][..],
            &["claude", "hi", "--codex-bin", "x"][..],
            &["codex", "hi", "--sandbox", "danger-full-access"][..],
            &["codex", "hi", "--model", "--yes"][..],
        ] {
            assert!(parse_args(args(input)).is_none());
        }
    }
}

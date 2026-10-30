use std::{io::Write, path::PathBuf, process::ExitCode};

use agentsflow_adapters::claude::{ClaudeOptions, ClaudeSession};
use agentsflow_events::Event;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};

const USAGE: &str = "Usage: agentsflow claude \"<prompt>\" [--model <m>] [--cwd <dir>] [--yes]\n       agentsflow --version";

fn truncate(text: &str) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn format_event(event: &Event) -> String {
    match event {
        Event::Message { text, .. } => format!("claude> {text}"),
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
    prompt: String,
    opts: ClaudeOptions,
    yes: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Option<Command> {
    let mut args = args.into_iter();
    if args.next()?.as_str() != "claude" {
        return None;
    }
    let prompt = args.next()?;
    if prompt.starts_with("--") {
        return None;
    }
    let mut command = Command {
        prompt,
        opts: ClaudeOptions::default(),
        yes: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--model" => {
                let value = args.next()?;
                if value.starts_with("--") {
                    return None;
                }
                command.opts.model = Some(value);
            }
            "--cwd" => {
                let value = args.next()?;
                if value.starts_with("--") {
                    return None;
                }
                command.opts.cwd = Some(PathBuf::from(value));
            }
            "--yes" => command.yes = true,
            _ => return None,
        }
    }
    Some(command)
}

async fn run(command: Command) -> Result<()> {
    let mut session = ClaudeSession::start(command.opts).await?;
    let result = run_turn(&mut session, &command.prompt, command.yes).await;
    let shutdown = session.shutdown().await;
    result?;
    shutdown
}

async fn run_turn(session: &mut ClaudeSession, prompt: &str, yes: bool) -> Result<()> {
    session.send(prompt).await?;
    let mut input = BufReader::new(tokio::io::stdin());
    loop {
        let events = session.next_events().await?;
        for event in &events {
            if let Event::PermissionRequest { id, .. } = event {
                let allow = if yes {
                    println!("{} [auto-allowed]", format_event(event));
                    true
                } else {
                    print!("{} Allow? [y/N] ", format_event(event));
                    std::io::stdout().flush()?;
                    let mut answer = String::new();
                    input.read_line(&mut answer).await?;
                    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                };
                session.respond(id, allow).await?;
            } else {
                println!("{}", format_event(event));
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
    fn formats_messages_and_results() {
        assert_eq!(
            format_event(&Event::Message {
                role: Role::Assistant,
                text: "hello".into(),
                delta: false
            }),
            "claude> hello"
        );
        assert_eq!(
            format_event(&Event::ToolResult {
                id: "1".into(),
                output: "bad".into(),
                is_error: true
            }),
            "[result:error] bad"
        );
        assert_eq!(
            format_event(&Event::Done {
                reason: "success".into()
            }),
            "[done] success"
        );
    }

    #[test]
    fn formats_and_truncates_tool_input() {
        let input = "я".repeat(201).into();
        let text = format_event(&Event::ToolCall {
            id: "1".into(),
            name: "Write".into(),
            input,
        });
        assert!(text.starts_with("[tool] Write \""));
        assert!(text.ends_with("..."));
        assert_eq!(truncate(&"я".repeat(201)).chars().count(), 203);
    }

    #[test]
    fn parses_options_and_rejects_invalid_commands() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cmd = parse_args(args(&[
            "claude", "hello", "--model", "haiku", "--cwd", ".", "--yes",
        ]))
        .unwrap();
        assert_eq!(cmd.prompt, "hello");
        assert_eq!(cmd.opts.model.as_deref(), Some("haiku"));
        assert!(cmd.yes);
        assert!(parse_args(args(&[])).is_none());
        assert!(parse_args(args(&["unknown"])).is_none());
        assert!(parse_args(args(&["claude", "hi", "--model", "--yes"])).is_none());
    }
}

use std::{io::Write, path::PathBuf};

use agentsflow_adapters::{
    Agent,
    claude::{ClaudeOptions, ClaudeSession},
    codex::{CodexOptions, CodexSession, SandboxMode},
};
use agentsflow_events::Event;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};

pub const USAGE: &str = "Usage: agentsflow serve [--port N] [--cwd DIR] [--agents claude,codex | --team pair|trio|FILE] [--claude-model M] [--codex-model M] [--sandbox read-only|workspace-write] [--codex-bin PATH] [--workflow FILE] [--log FILE] [--mail-thread-limit N] [--yes] [--allow-bypass]\n       agentsflow flow FILE --task <text> --yes [--team pair|trio|FILE] [--allow-bypass] [options]\n       agentsflow <claude|codex> \"<prompt>\" [--model <m>] [--cwd <dir>] [--yes] [--sandbox <read-only|workspace-write>] [--codex-bin <path>]\n       agentsflow team init [FILE] | team check <pair|trio|FILE> | team show\n       agentsflow --version";

fn truncate(text: &str) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

pub fn format_event(agent: &str, event: &Event) -> String {
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
        Event::PermissionResolved { id, allow, .. } => format!(
            "[permission:{}] {id}",
            if *allow { "allowed" } else { "denied" }
        ),
        Event::Done { reason } => format!("[done] {reason}"),
        Event::Error { message } => format!("[error] {message}"),
        Event::Diff { path, patch } => format!("[diff] {path}\n{patch}"),
        Event::Mail {
            from, to, subject, ..
        } => format!(
            "[mail] {} -> {}: {subject}",
            agent_label(from),
            agent_label(to)
        ),
        Event::MailRead { agent, ids } => {
            format!("[mail:read] {} {}", agent_label(agent), ids.join(", "))
        }
        Event::Handoff {
            from,
            to,
            step,
            text,
        } => format!(
            "[handoff] {} -> {} ({step}): {text}",
            agent_label(from),
            agent_label(to)
        ),
        Event::Workflow {
            name,
            step,
            status,
            detail,
        } => format!(
            "[workflow] {name} {} {} {detail}",
            step.as_deref().unwrap_or("-"),
            serde_json::to_value(status).unwrap().as_str().unwrap()
        ),
    }
}

fn agent_label(agent: &agentsflow_events::AgentId) -> &str {
    agent.as_str()
}

fn print_flow_event(envelope: &agentsflow_events::Envelope) {
    let label = agent_label(&envelope.agent);
    let formatted = format_event(label, &envelope.event);
    if matches!(
        &envelope.event,
        Event::Message { .. } | Event::Handoff { .. } | Event::Workflow { .. }
    ) {
        println!("{formatted}");
    } else {
        println!("[{label}] {formatted}");
    }
}

pub struct Command {
    agent: String,
    prompt: String,
    model: Option<String>,
    cwd: Option<PathBuf>,
    yes: bool,
    sandbox: Option<SandboxMode>,
    bin: Option<PathBuf>,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Option<Command> {
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

pub async fn run(command: Command) -> Result<()> {
    if command.agent == "claude" {
        let session = ClaudeSession::start(ClaudeOptions {
            model: command.model,
            cwd: command.cwd,
            mail: None,
            effort: None,
            bypass: false,
        })
        .await?;
        run_turn(session, "claude", &command.prompt, command.yes).await
    } else {
        let session = CodexSession::start(CodexOptions {
            model: command.model,
            cwd: command.cwd,
            sandbox: command.sandbox,
            bin: command.bin,
            mail: None,
            effort: None,
            bypass: false,
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

pub struct FlowCommand {
    spec: crate::workflow::Spec,
    task: String,
    options: crate::runtime::RuntimeOptions,
}
impl FlowCommand {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let file = args
            .first()
            .filter(|s| !s.starts_with("--"))
            .ok_or_else(|| anyhow::anyhow!("flow requires a workflow file"))?;
        let mut rest = args[1..].iter();
        let mut task = None;
        let mut yes = false;
        let mut serve_args = Vec::new();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--yes" => {
                    yes = true;
                    serve_args.push(arg.clone());
                }
                "--allow-bypass" => serve_args.push(arg.clone()),
                "--task" => {
                    task = Some(
                        rest.next()
                            .ok_or_else(|| anyhow::anyhow!("--task requires text"))?
                            .clone(),
                    );
                }
                "--cwd" | "--agents" | "--team" | "--claude-model" | "--codex-model"
                | "--sandbox" | "--codex-bin" => {
                    serve_args.push(arg.clone());
                    serve_args.push(
                        rest.next()
                            .ok_or_else(|| anyhow::anyhow!("{arg} requires a value"))?
                            .clone(),
                    );
                }
                _ => anyhow::bail!("unknown flow option: {arg}"),
            }
        }
        if !yes {
            anyhow::bail!("--yes is required: headless mode cannot answer permission requests");
        }
        let task = task
            .filter(|t| !t.trim().is_empty() && !t.starts_with("--"))
            .ok_or_else(|| anyhow::anyhow!("--task requires nonempty text"))?;
        let options = crate::server::ServeOptions::parse_checked(&serve_args)?
            .ok_or_else(|| anyhow::anyhow!("invalid flow options"))?
            .runtime_options();
        let spec =
            crate::workflow::Spec::load(std::path::Path::new(file))?.resolve_team(&options.team)?;
        Ok(Self {
            spec,
            task,
            options,
        })
    }
    pub async fn run(self) -> Result<bool> {
        tokio::task::LocalSet::new()
            .run_until(async move {
                let runtime = crate::runtime::Runtime::new(self.options)?;
                let (_, mut events) = runtime.hub.subscribe();
                let (stop, receiver) = tokio::sync::watch::channel(false);
                let id = uuid::Uuid::new_v4().to_string();
                let success = {
                    let execution = crate::workflow::execute(
                        &runtime.hub,
                        &runtime.agents,
                        &self.spec,
                        &self.task,
                        &id,
                        receiver,
                    );
                    tokio::pin!(execution);
                    let mut interrupted = false;
                    loop {
                        tokio::select! {
                            signal = tokio::signal::ctrl_c(), if !interrupted => {
                                if let Err(error) = signal { eprintln!("signal: {error}"); }
                                interrupted = true;
                                let _ = stop.send(true);
                            }
                            event = events.recv() => {
                                if let Ok(event) = event {
                                    print_flow_event(&event);
                                }
                            }
                            result = &mut execution => {
                                while let Ok(event) = events.try_recv() {
                                    print_flow_event(&event);
                                }
                                break result;
                            }
                        }
                    }
                };
                runtime.shutdown().await;
                Ok(success)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_events::Role;

    #[test]
    fn flow_requires_bypass_acknowledgement() {
        let path =
            std::env::temp_dir().join(format!("agentsflow-bypass-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, "[[agent]]\nid='lead'\nkind='claude'\nlead=true\nmode='bypass'\nroles=['reviewer','engineer']\n").unwrap();
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../workflows/implement-and-review.toml");
        let mut args = vec![
            file.to_string_lossy().into_owned(),
            "--task".into(),
            "x".into(),
            "--yes".into(),
            "--team".into(),
            path.to_string_lossy().into_owned(),
        ];
        assert!(
            FlowCommand::parse(&args)
                .err()
                .unwrap()
                .to_string()
                .contains("--allow-bypass")
        );
        args.push("--allow-bypass".into());
        assert!(FlowCommand::parse(&args).is_ok());
        std::fs::remove_file(path).unwrap();
    }

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

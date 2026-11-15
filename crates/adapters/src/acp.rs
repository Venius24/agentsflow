use crate::Agent;
use agentsflow_events::{Event, Role};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, process::Stdio, time::Duration};

pub const ACP_START_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct ToolState {
    data: serde_json::Map<String, Value>,
    emitted: bool,
    diff: bool,
    result: bool,
}
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};

#[derive(Default)]
pub struct AcpOptions {
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub mail: Option<crate::MailConfig>,
    pub diagnostic: Option<fn(&str)>,
}

pub struct AcpSession {
    child: Child,
    stdin: mpsc::UnboundedSender<String>,
    lines: mpsc::UnboundedReceiver<std::io::Result<String>>,
    session_id: String,
    next_id: u64,
    turn_id: Option<u64>,
    pending: HashMap<String, (Value, Vec<Value>)>,
    block: Option<(bool, String, String)>,
    block_no: u64,
    tools: HashMap<String, ToolState>,
    process_id: Option<u32>,
    #[cfg(windows)]
    watchdog: Option<std::process::Child>,
}

fn enqueue(tx: &mpsc::UnboundedSender<String>, value: Value) -> Result<()> {
    tx.send(format!("{value}\n"))
        .map_err(|_| anyhow::anyhow!("ACP stdin closed"))
}

impl AcpSession {
    pub async fn start(opts: AcpOptions) -> Result<Self> {
        Self::start_with_timeout(opts, ACP_START_TIMEOUT).await
    }

    async fn start_with_timeout(opts: AcpOptions, timeout: Duration) -> Result<Self> {
        let (program, args) = opts.command.split_first().context("ACP command is empty")?;
        let cwd = std::path::absolute(opts.cwd.unwrap_or(std::env::current_dir()?))?;
        #[cfg(windows)]
        let executable = if std::path::Path::new(program).components().count() == 1
            && std::path::Path::new(program).extension().is_none()
        {
            std::env::var_os("PATH")
                .and_then(|path| {
                    std::env::split_paths(&path)
                        .flat_map(|dir| {
                            [
                                dir.join(format!("{program}.exe")),
                                dir.join(format!("{program}.cmd")),
                                dir.join(format!("{program}.bat")),
                            ]
                        })
                        .find(|path| path.is_file())
                })
                .unwrap_or_else(|| program.into())
        } else {
            program.into()
        };
        #[cfg(not(windows))]
        let executable: PathBuf = program.into();
        let mut cmd = Command::new(executable);
        cmd.args(args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.env_remove(crate::TELEGRAM_TOKEN_VAR);
        for var in crate::GITHUB_TOKEN_VARS {
            cmd.env_remove(var);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot start ACP agent {program}"))?;
        let process_id = child.id();
        // A separate watcher survives termination of the gateway itself. Drop
        // alone cannot run after the operating system forcibly kills a process.
        #[cfg(windows)]
        let watchdog = process_id.and_then(|pid| {
            use std::os::windows::process::CommandExt;
            let script = format!("Wait-Process -Id {} -ErrorAction SilentlyContinue; & taskkill.exe /PID {} /T /F *> $null; Stop-Process -Id {} -Force -ErrorAction SilentlyContinue", std::process::id(), pid, pid);
            std::process::Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script]).creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()
        });
        let mut input = child.stdin.take().context("ACP stdin unavailable")?;
        let output = child.stdout.take().context("ACP stdout unavailable")?;
        let errors = child.stderr.take().context("ACP stderr unavailable")?;
        let diagnostic = opts.diagnostic;
        tokio::spawn(async move {
            let mut lines = BufReader::new(errors).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(log) = diagnostic {
                    log(&line);
                } else {
                    eprintln!("ACP: {line}");
                }
            }
        });
        let (stdin, mut queue) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = queue.recv().await {
                if input.write_all(line.as_bytes()).await.is_err() || input.flush().await.is_err() {
                    break;
                }
            }
        });
        let (sender, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if sender.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = sender.send(Err(e));
                        break;
                    }
                }
            }
        });
        let mut this = Self {
            child,
            stdin,
            lines,
            session_id: String::new(),
            next_id: 1,
            turn_id: None,
            pending: HashMap::new(),
            block: None,
            block_no: 0,
            tools: HashMap::new(),
            process_id,
            #[cfg(windows)]
            watchdog,
        };
        let init = this.request("initialize", json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}))?;
        let command_name = opts.command.join(" ");
        let login_error = || {
            format!(
                "агент `{command_name}` не ответил: похоже, он не вошёл в аккаунт; сначала войдите в него в терминале"
            )
        };
        let init_result = tokio::time::timeout(timeout, this.wait_response(init))
            .await
            .map_err(|_| anyhow::anyhow!(login_error()))??;
        let auth_methods = init_result
            .get("authMethods")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty());
        let mut servers = Vec::new();
        if let Some(mail) = opts.mail {
            servers.push(json!({"name":"mail","command":mail.exe,"args":["mail-mcp","--as",mail.id],"env":[{"name":"AGENTSFLOW_URL","value":mail.url},{"name":"AGENTSFLOW_TOKEN","value":mail.token},{"name":"AGENTSFLOW_PEERS","value":mail.peers.join(",")}]}));
        }
        let id = this.request("session/new", json!({"cwd":cwd,"mcpServers":servers}))?;
        let result = match tokio::time::timeout(timeout, this.wait_response(id)).await {
            Err(_) => bail!(login_error()),
            Ok(Err(error))
                if auth_methods
                    && (format!("{error:#}").to_ascii_lowercase().contains("auth")
                        || format!("{error:#}").contains("-32000")) =>
            {
                bail!("{}: {error:#}", login_error())
            }
            Ok(result) => result?,
        };
        this.session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .context("ACP session/new missing sessionId")?
            .to_owned();
        Ok(this)
    }
    fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        enqueue(
            &self.stdin,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        Ok(id)
    }
    async fn frame(&mut self) -> Result<Value> {
        loop {
            let line = self
                .lines
                .recv()
                .await
                .context("ACP agent closed stdout")??;
            match serde_json::from_str::<Value>(&line) {
                Ok(v) if v.is_object() => return Ok(v),
                _ => eprintln!("ACP: malformed frame: {line}"),
            }
        }
    }
    async fn wait_response(&mut self, id: u64) -> Result<Value> {
        loop {
            let frame = self.frame().await?;
            if frame.get("id").and_then(Value::as_u64) == Some(id) && frame.get("method").is_none()
            {
                if let Some(error) = frame.get("error") {
                    bail!("ACP request failed: {error}");
                }
                return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
            }
            self.handle(frame)?;
        }
    }
    fn flush(&mut self, events: &mut Vec<Event>) {
        if let Some((thinking, _, text)) = self.block.take() {
            if !text.is_empty() {
                if thinking {
                    events.push(Event::Thinking { text });
                } else {
                    events.push(Event::Message {
                        role: Role::Assistant,
                        text,
                        delta: false,
                    });
                }
            }
        }
    }
    fn tool_update(&mut self, update: &Value, events: &mut Vec<Event>) {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let state = self.tools.entry(id.to_owned()).or_default();
        if let Some(fields) = update.as_object() {
            for (key, value) in fields {
                if !value.is_null() {
                    state.data.insert(key.clone(), value.clone());
                }
            }
        }
        let get = |name: &str| state.data.get(name);
        let status = get("status").and_then(Value::as_str).unwrap_or("");
        if !state.emitted
            && (get("rawInput").is_some()
                || matches!(status, "in_progress" | "completed" | "failed"))
        {
            let input = get("rawInput").cloned().unwrap_or_else(
                || json!({"title":get("title"),"kind":get("kind"),"locations":get("locations")}),
            );
            events.push(Event::ToolCall {
                id: id.into(),
                name: get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .into(),
                input,
            });
            state.emitted = true;
        }
        if let Some(items) = update.get("content").and_then(Value::as_array) {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("diff") {
                    if let Some(path) = item.get("path").and_then(Value::as_str) {
                        let old = item.get("oldText").and_then(Value::as_str);
                        let new = item.get("newText").and_then(Value::as_str).unwrap_or("");
                        let input = if let Some(old) = old {
                            json!({"file_path":path,"old_string":old,"new_string":new})
                        } else {
                            json!({"file_path":path,"content":new})
                        };
                        if let Some(diff) = crate::claude::tool_diff(
                            if old.is_some() { "Edit" } else { "Write" },
                            &input,
                        ) {
                            events.push(diff);
                            state.diff = true;
                        }
                    }
                }
            }
        }
        let status = state
            .data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(status, "completed" | "failed") && !state.result {
            if !state.diff {
                if let Some(input) = state.data.get("rawInput") {
                    let path = input
                        .get("filePath")
                        .or_else(|| input.get("file_path"))
                        .and_then(Value::as_str);
                    if let Some(path) = path {
                        let old = input
                            .get("oldString")
                            .or_else(|| input.get("old_string"))
                            .and_then(Value::as_str);
                        let new = input
                            .get("newString")
                            .or_else(|| input.get("new_string"))
                            .and_then(Value::as_str);
                        let patch = if let (Some(old), Some(new)) = (old, new) {
                            crate::claude::tool_diff(
                                "Edit",
                                &json!({"file_path":path,"old_string":old,"new_string":new}),
                            )
                        } else {
                            input
                                .get("content")
                                .and_then(Value::as_str)
                                .and_then(|content| {
                                    crate::claude::tool_diff(
                                        "Write",
                                        &json!({"file_path":path,"content":content}),
                                    )
                                })
                        };
                        if let Some(diff) = patch {
                            events.push(diff);
                            state.diff = true;
                        }
                    }
                }
            }
            let output = state
                .data
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| {
                            v.pointer("/content/text")
                                .or_else(|| v.get("text"))
                                .and_then(Value::as_str)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            events.push(Event::ToolResult {
                id: id.into(),
                output,
                is_error: status == "failed",
            });
            state.result = true;
        }
    }
    fn handle(&mut self, frame: Value) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        if let Some(method) = frame.get("method").and_then(Value::as_str) {
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            match method {
                "session/update" => {
                    let u = &params["update"];
                    let kind = u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
                    if matches!(kind, "agent_message_chunk" | "agent_thought_chunk") {
                        let thinking = kind == "agent_thought_chunk";
                        if self.block.as_ref().is_some_and(|b| b.0 != thinking) {
                            self.flush(&mut events);
                        }
                        if let Some(text) = u.pointer("/content/text").and_then(Value::as_str) {
                            if self.block.is_none() {
                                self.block_no += 1;
                                self.block = Some((
                                    thinking,
                                    format!("acp:{}", self.block_no),
                                    String::new(),
                                ));
                            }
                            if let Some((_, id, whole)) = &mut self.block {
                                whole.push_str(text);
                                if crate::stream_enabled() {
                                    events.push(Event::StreamDelta {
                                        block_id: id.clone(),
                                        thinking,
                                        text: text.to_owned(),
                                    });
                                }
                            }
                        }
                    } else {
                        self.flush(&mut events);
                        if matches!(kind, "tool_call" | "tool_call_update") {
                            self.tool_update(u, &mut events);
                        }
                    }
                }
                "session/request_permission" => {
                    if let Some(id) = frame.get("id") {
                        self.flush(&mut events);
                        let key = id.to_string();
                        let call = &params["toolCall"];
                        let options = params
                            .get("options")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        self.pending.insert(key.clone(), (id.clone(), options));
                        let kind = call.get("kind").and_then(Value::as_str).unwrap_or("");
                        let input = call.get("rawInput").cloned().unwrap_or_else(|| json!({}));
                        events.push(Event::PermissionRequest {
                            id: key,
                            tool: if kind == "execute" {
                                "Bash".into()
                            } else {
                                call.get("title")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_owned()
                            },
                            input,
                            guard: None,
                        });
                    }
                }
                _ => {
                    if let Some(id) = frame.get("id") {
                        enqueue(
                            &self.stdin,
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
                        )?;
                    }
                }
            }
        } else if frame.get("id").and_then(Value::as_u64) == self.turn_id && self.turn_id.is_some()
        {
            self.turn_id = None;
            self.flush(&mut events);
            if let Some(error) = frame.get("error") {
                events.push(Event::Error {
                    message: error.to_string(),
                });
            }
            events.push(Event::Done {
                reason: frame
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_owned(),
            });
        }
        Ok(events)
    }
}

impl Agent for AcpSession {
    async fn send(&mut self, prompt: &str) -> Result<()> {
        if self.turn_id.is_some() {
            bail!("ACP turn already active");
        }
        self.turn_id = Some(self.request(
            "session/prompt",
            json!({"sessionId":self.session_id,"prompt":[{"type":"text","text":prompt}]}),
        )?);
        Ok(())
    }
    async fn next_events(&mut self) -> Result<Vec<Event>> {
        let frame = self.frame().await?;
        self.handle(frame)
    }
    async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()> {
        let (id, options) = self
            .pending
            .remove(request_id)
            .context("unknown ACP permission request")?;
        let kinds: &[&str] = if allow {
            &["allow_once", "allow_always"]
        } else {
            &["reject_once", "reject_always"]
        };
        let selected = kinds.iter().find_map(|kind| {
            options
                .iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some(*kind))
                .and_then(|o| o.get("optionId"))
        });
        let outcome = selected.map_or_else(
            || json!({"outcome":"cancelled"}),
            |id| json!({"outcome":"selected","optionId":id}),
        );
        enqueue(
            &self.stdin,
            json!({"jsonrpc":"2.0","id":id,"result":{"outcome":outcome}}),
        )
    }
    fn interrupter(&self) -> crate::Interrupter {
        let tx = self.stdin.clone();
        let id = self.session_id.clone();
        crate::Interrupter::new(move || {
            enqueue(
                &tx,
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}),
            )
        })
    }
    async fn shutdown(mut self) -> Result<()> {
        self.kill_tree();
        let _ = self.child.wait().await;
        Ok(())
    }
}

impl AcpSession {
    fn kill_tree(&mut self) {
        #[cfg(windows)]
        if let Some(mut watcher) = self.watchdog.take() {
            let _ = watcher.kill();
            let _ = watcher.wait();
        }
        if let Some(pid) = self.process_id.take() {
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = self.child.start_kill();
    }
}
impl Drop for AcpSession {
    fn drop(&mut self) {
        self.kill_tree();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_command(mode: &str) -> Option<Vec<String>> {
        for (interpreter, script) in [
            ("node", "fake_acp.js"),
            ("python", "fake_acp.py"),
            ("python3", "fake_acp.py"),
        ] {
            if std::process::Command::new(interpreter)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                return Some(vec![
                    interpreter.into(),
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("tests")
                        .join(script)
                        .to_string_lossy()
                        .into_owned(),
                    mode.into(),
                ]);
            }
        }
        eprintln!("skipping ACP subprocess test: neither node nor python is available");
        None
    }

    #[tokio::test]
    async fn fake_agent_subprocess() -> Result<()> {
        let Some(command) = fake_command("normal") else {
            return Ok(());
        };
        let mut agent = AcpSession::start_with_timeout(
            AcpOptions {
                command,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await?;
        agent.send("hello").await?;
        let mut seen = Vec::new();
        let mut calls = 0;
        loop {
            let events =
                tokio::time::timeout(Duration::from_secs(5), agent.next_events()).await??;
            for event in events {
                match &event {
                    Event::PermissionRequest { id, .. } => {
                        calls += 1;
                        agent.respond(id, calls == 1).await?;
                    }
                    Event::ToolResult { .. } => agent.interrupt()?,
                    _ => {}
                }
                let done = matches!(event, Event::Done { .. });
                seen.push(event);
                if done {
                    break;
                }
            }
            if matches!(seen.last(), Some(Event::Done { .. })) {
                break;
            }
        }
        assert_eq!(calls, 2);
        assert!(
            seen.iter()
                .any(|e| matches!(e, Event::Thinking { text } if text == "think"))
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, Event::Message { text, .. } if text == "hello"))
        );
        let call = seen
            .iter()
            .position(
                |e| matches!(e, Event::ToolCall { input, .. } if input["filePath"] == "a.txt"),
            )
            .context("late input")?;
        let result = seen
            .iter()
            .position(|e| matches!(e, Event::ToolResult { .. }))
            .context("result")?;
        assert!(call < result);
        assert!(seen.iter().any(
            |e| matches!(e, Event::Diff { path, patch } if path == "a.txt" && patch.contains("+hi"))
        ));
        assert!(matches!(seen.last(), Some(Event::Done { reason }) if reason == "cancelled"));
        if crate::stream_enabled() {
            assert!(seen.iter().any(|e| matches!(e, Event::StreamDelta { .. })));
        } else {
            assert!(!seen.iter().any(|e| matches!(e, Event::StreamDelta { .. })));
        }
        if std::env::var_os("ACP_TEST_STREAM_OFF").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "acp::tests::fake_agent_subprocess",
                    "--nocapture",
                ])
                .env("AGENTSFLOW_STREAM", "off")
                .env("ACP_TEST_STREAM_OFF", "1")
                .status()?;
            assert!(status.success());
        }
        agent.shutdown().await?;
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn watcher_cleans_up_after_gateway_termination() -> Result<()> {
        if std::env::var_os("ACP_TEST_ORPHAN").is_some() {
            let Some(command) = fake_command("orphan") else {
                return Ok(());
            };
            let session = AcpSession::start_with_timeout(
                AcpOptions {
                    command,
                    ..Default::default()
                },
                Duration::from_secs(5),
            )
            .await?;
            std::fs::write(
                std::env::var_os("ACP_TEST_ORPHAN").context("pid file")?,
                session.process_id.context("child pid")?.to_string(),
            )?;
            // Simulate an OS termination: destructors do not run.
            std::process::exit(0);
        }
        if fake_command("orphan").is_none() {
            return Ok(());
        }
        let pid_file =
            std::env::temp_dir().join(format!("agentsflow-acp-orphan-{}", std::process::id()));
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "acp::tests::watcher_cleans_up_after_gateway_termination",
                "--nocapture",
            ])
            .env("ACP_TEST_ORPHAN", &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        assert!(status.success());
        let pid: u32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
        std::fs::remove_file(pid_file)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let script = format!(
                "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
            );
            let status = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if !status.success() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = std::process::Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &format!("Stop-Process -Id {pid} -Force -ErrorAction SilentlyContinue"),
                    ])
                    .status();
                panic!("orphaned ACP process {pid}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn startup_timeouts_and_auth_error() -> Result<()> {
        for mode in ["hang", "newhang", "auth"] {
            let Some(command) = fake_command(mode) else {
                return Ok(());
            };
            let error = match AcpSession::start_with_timeout(
                AcpOptions {
                    command,
                    ..Default::default()
                },
                // The auth answer needs the interpreter to start: slow on CI runners.
                Duration::from_millis(if mode == "auth" { 20_000 } else { 700 }),
            )
            .await
            {
                Ok(_) => panic!("expected startup failure"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("терминале"), "{error}");
            if mode == "auth" {
                assert!(error.contains("Authentication required"), "{error}");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn protocol_turn_and_permissions() -> Result<()> {
        let child = Command::new(std::env::current_exe()?)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let (stdin, mut outgoing) = mpsc::unbounded_channel();
        let (incoming, lines) = mpsc::unbounded_channel();
        let mut agent = AcpSession {
            child,
            stdin,
            lines,
            session_id: "s".into(),
            next_id: 1,
            turn_id: None,
            pending: HashMap::new(),
            block: None,
            block_no: 0,
            tools: HashMap::new(),
            process_id: None,
            #[cfg(windows)]
            watchdog: None,
        };
        let id = agent.request("initialize", json!({"protocolVersion":1}))?;
        assert!(
            outgoing
                .recv()
                .await
                .context("request")?
                .contains("initialize")
        );
        incoming.send(Ok("malformed".into()))?;
        incoming.send(Ok(json!({"jsonrpc":"2.0","id":id,"result":{}}).to_string()))?;
        agent.wait_response(id).await?;
        let id = agent.request("session/new", json!({"cwd":".","mcpServers":[]}))?;
        assert!(
            outgoing
                .recv()
                .await
                .context("request")?
                .contains("session/new")
        );
        incoming.send(Ok(json!({"id":id,"result":{"sessionId":"s"}}).to_string()))?;
        assert_eq!(agent.wait_response(id).await?["sessionId"], "s");
        agent.send("hello").await?;
        assert!(
            outgoing
                .recv()
                .await
                .context("prompt")?
                .contains("session/prompt")
        );
        let update =
            |u: Value| json!({"method":"session/update","params":{"sessionId":"s","update":u}});
        for text in ["hel", "lo"] {
            let events = agent.handle(update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}})))?;
            if crate::stream_enabled() {
                assert!(matches!(events.as_slice(), [Event::StreamDelta { .. }]));
            }
        }
        let events = agent.handle(update(
            json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"run","kind":"execute","status":"in_progress"}),
        ))?;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Message { text, .. } if text == "hello"))
        );
        assert!(events.iter().any(|e| matches!(e, Event::ToolCall { .. })));
        for allow in [true, false] {
            let events = agent.handle(json!({"id":42,"method":"session/request_permission","params":{"toolCall":{"kind":"execute","rawInput":{"command":"git status"}},"options":[{"optionId":"yes","kind":"allow_once"},{"optionId":"no","kind":"reject_once"}]}}))?;
            assert!(
                matches!(events.as_slice(), [Event::PermissionRequest { tool, input, .. }] if tool == "Bash" && input["command"] == "git status")
            );
            agent.respond("42", allow).await?;
            let response: Value =
                serde_json::from_str(&outgoing.recv().await.context("response")?)?;
            assert_eq!(
                response["result"]["outcome"]["optionId"],
                if allow { "yes" } else { "no" }
            );
        }
        let events = agent.handle(update(json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed","content":[{"type":"diff","path":"x","oldText":"a","newText":"b"}]})))?;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Diff { path, .. } if path == "x"))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolResult {
                is_error: false,
                ..
            }
        )));
        agent.handle(json!({"id":55,"method":"unknown"}))?;
        let response: Value = serde_json::from_str(&outgoing.recv().await.context("error")?)?;
        assert_eq!(response["error"]["code"], -32601);
        assert!(
            agent
                .handle(update(json!({"sessionUpdate":"future_kind"})))?
                .is_empty()
        );
        agent.interrupt()?;
        assert!(
            outgoing
                .recv()
                .await
                .context("cancel")?
                .contains("session/cancel")
        );
        let turn_id = agent.turn_id.context("turn")?;
        let events = agent.handle(json!({"id":turn_id,"result":{"stopReason":"cancelled"}}))?;
        assert!(matches!(events.as_slice(), [Event::Done { reason }] if reason == "cancelled"));
        Ok(())
    }
}

use agentsflow_events::{AgentId, Envelope, Event, Role, TaskSnapshot, TaskStatus, WorkflowStatus};
use std::fmt::Write;

const TOOL_INPUT_LIMIT: usize = 200;
const TOOL_OUTPUT_LIMIT: usize = 1500;

/// One `serde_json` line per envelope, in order, each terminated by `\n`.
pub fn render_jsonl(events: &[Envelope]) -> String {
    let mut out = String::new();
    for envelope in events {
        out.push_str(&serde_json::to_string(envelope).expect("event serializes"));
        out.push('\n');
    }
    out
}

/// A readable transcript. Event text never reaches the output as raw HTML or
/// as a fence that could swallow the rest of the document.
pub fn render_markdown(events: &[Envelope]) -> String {
    let mut tasks: Vec<TaskSnapshot> = Vec::new();
    for envelope in events {
        if let Event::Task { task } = &envelope.event {
            if let Some(current) = tasks.iter_mut().find(|t| t.id == task.id) {
                *current = task.clone();
            } else {
                tasks.push(task.clone());
            }
        }
    }
    render_markdown_with_tasks(events, &tasks)
}

pub fn render_markdown_with_tasks(events: &[Envelope], current_tasks: &[TaskSnapshot]) -> String {
    let mut out = String::from("# agentsflow session transcript\n\n");
    let range = events
        .iter()
        .map(|e| e.ts_ms)
        .min()
        .zip(events.iter().map(|e| e.ts_ms).max());
    match range {
        Some((first, last)) => {
            let _ = writeln!(
                out,
                "{} events, {} \u{2013} {} UTC",
                events.len(),
                format_ts(first),
                format_ts(last)
            );
        }
        None => out.push_str("0 events\n"),
    }
    for envelope in events {
        if let Some(block) = render_event(envelope) {
            out.push('\n');
            out.push_str(&block);
        }
    }
    let mut tasks = current_tasks.to_vec();
    if !tasks.is_empty() {
        tasks.sort_by_key(|task| task.created_ms);
        out.push_str("\n## Задачи\n");
        for (status, title) in [
            (TaskStatus::Backlog, "Backlog"),
            (TaskStatus::Todo, "Todo"),
            (TaskStatus::InProgress, "In progress"),
            (TaskStatus::Review, "Review"),
            (TaskStatus::Done, "Done"),
            (TaskStatus::Blocked, "Blocked"),
            (TaskStatus::Archived, "Archived"),
        ] {
            let group: Vec<_> = tasks.iter().filter(|task| task.status == status).collect();
            if group.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n### {title}");
            for task in group {
                let assignee = task.assignee.as_ref().map_or("-", AgentId::as_str);
                let _ = writeln!(
                    out,
                    "- {} - {} - {}% - {}",
                    escape_inline(&task.title),
                    escape_inline(assignee),
                    task.progress,
                    escape_inline(&task.note)
                );
            }
        }
    }
    out
}

fn render_event(envelope: &Envelope) -> Option<String> {
    let agent = agent_name(&envelope.agent);
    let mut out = String::new();
    match &envelope.event {
        Event::Task { .. } | Event::Usage { .. } => return None,
        Event::Message { role, text, .. } => {
            let role = match role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let _ = writeln!(out, "**{agent} ({role}):** {}", escape_text(text));
        }
        Event::ToolCall { name, input, .. } => {
            let (args, _) = truncate(&input.to_string(), TOOL_INPUT_LIMIT);
            let _ = writeln!(
                out,
                "**{agent}** called {}({})",
                inline_code(name),
                inline_code(&args)
            );
        }
        Event::ToolResult {
            id,
            output,
            is_error,
        } => {
            let marker = if *is_error { " [error]" } else { "" };
            let (shown, rest) = truncate(output, TOOL_OUTPUT_LIMIT);
            let _ = writeln!(out, "**{agent}** tool result{marker} {}:", inline_code(id));
            out.push('\n');
            out.push_str(&fenced("", &shown));
            if rest > 0 {
                let _ = writeln!(out, "\n[truncated, {rest} more chars]");
            }
        }
        Event::Diff { path, patch } => {
            let _ = writeln!(out, "**{agent}** diff {}:", inline_code(path));
            out.push('\n');
            out.push_str(&fenced("diff", patch));
        }
        Event::PermissionRequest { tool, guard, .. } => {
            let _ = writeln!(
                out,
                "**{agent}** asked permission for {}",
                inline_code(tool)
            );
            if let Some(note) = guard {
                let _ = writeln!(
                    out,
                    "Guard: {} - {}",
                    escape_inline(&note.rule),
                    escape_inline(&note.reason)
                );
            }
        }
        Event::PermissionResolved { id, allow, .. } => {
            let verdict = if *allow { "allowed" } else { "denied" };
            let _ = writeln!(out, "**{agent}** permission {} {verdict}", inline_code(id));
        }
        Event::Mail {
            from,
            to,
            subject,
            body,
            ..
        } => {
            let _ = writeln!(
                out,
                "> **{} -> {}** [{}]",
                agent_name(from),
                agent_name(to),
                escape_inline(subject)
            );
            out.push_str(">\n");
            for line in escape_text(body).lines() {
                let _ = writeln!(out, "> {line}");
            }
        }
        Event::MailRead { .. } => return None,
        Event::Handoff {
            from,
            to,
            step,
            text,
        } => {
            let _ = writeln!(
                out,
                "*handoff {} -> {} [{}]: {}*",
                agent_name(from),
                agent_name(to),
                escape_inline(step),
                escape_inline(text)
            );
        }
        Event::Workflow {
            name,
            step,
            status,
            detail,
        } => {
            let _ = write!(
                out,
                "*workflow {}: {}",
                escape_inline(name),
                status_name(status)
            );
            if let Some(step) = step {
                let _ = write!(out, " (step {})", escape_inline(step));
            }
            if !detail.is_empty() {
                let _ = write!(out, " - {}", escape_inline(detail));
            }
            out.push_str("*\n");
        }
        Event::Done { reason } => {
            let _ = writeln!(out, "**{agent}** done: {}", escape_inline(reason));
        }
        Event::Error { message } => {
            let _ = writeln!(out, "**{agent}** error: {}", escape_inline(message));
        }
    }
    Some(out)
}

fn agent_name(agent: &AgentId) -> &str {
    agent.as_str()
}

fn status_name(status: &WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Started => "started",
        WorkflowStatus::StepStarted => "step started",
        WorkflowStatus::StepDone => "step done",
        WorkflowStatus::Looped => "looped",
        WorkflowStatus::Finished => "finished",
        WorkflowStatus::Failed => "failed",
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time in milliseconds.
fn format_ts(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
    let rem = secs % 86_400;
    // Days since 1970-01-01 to a civil date (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The first `limit` chars of `text` and how many chars were cut off.
fn truncate(text: &str, limit: usize) -> (String, usize) {
    match text.char_indices().nth(limit) {
        Some((at, _)) => (text[..at].to_string(), text[at..].chars().count()),
        None => (text.to_string(), 0),
    }
}

fn longest_run(text: &str, ch: char) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in text.chars() {
        run = if c == ch { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// A fenced block whose fence is longer than any backtick run in `text`.
fn fenced(info: &str, text: &str) -> String {
    let fence = "`".repeat((longest_run(text, '`') + 1).max(3));
    let mut out = format!("{fence}{info}\n{text}");
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
    out
}

/// An inline code span on a single line, safe for any content.
fn inline_code(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let fence = "`".repeat(longest_run(&flat, '`') + 1);
    if flat.starts_with('`') || flat.ends_with('`') {
        format!("{fence} {flat} {fence}")
    } else {
        format!("{fence}{flat}{fence}")
    }
}

/// Multi-line prose: no raw HTML and no line that could open a code fence.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            out.push('\\');
        }
        out.push_str(&line.replace('<', "&lt;"));
    }
    out
}

/// One line of prose with markdown punctuation escaped, for use inside
/// emphasis or brackets.
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '\\' | '`' | '*' | '_' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_is_only_in_jsonl() {
        let event = env(
            1,
            0,
            AgentId::from("claude"),
            Event::Usage {
                input_tokens: 123,
                output_tokens: 45,
                cost_usd: Some(0.12),
            },
        );
        assert!(render_event(&event).is_none());
        let line = render_jsonl(&[event]);
        let envelope: Envelope = serde_json::from_str(line.trim()).unwrap();
        assert!(
            matches!(envelope.event, Event::Usage { input_tokens: 123, output_tokens: 45, cost_usd: Some(cost) } if cost == 0.12)
        );
    }

    #[test]
    fn tasks_section_uses_latest_snapshot() {
        let task = TaskSnapshot {
            id: "12345678".into(),
            title: "Build".into(),
            description: String::new(),
            status: TaskStatus::Todo,
            assignee: Some(AgentId::from("codex")),
            progress: 0,
            labels: vec![],
            note: String::new(),
            thread: None,
            created_ms: 1,
            updated_ms: 1,
        };
        let mut updated = task.clone();
        updated.status = TaskStatus::Review;
        updated.progress = 80;
        updated.note = "ready".into();
        let out = render_markdown(&[
            env(1, 1, AgentId::orchestrator(), Event::Task { task }),
            env(2, 2, AgentId::from("codex"), Event::Task { task: updated }),
        ]);
        assert!(out.contains("## Задачи"));
        assert!(out.contains("### Review"));
        assert!(
            out.contains("Build")
                && out.contains("codex")
                && out.contains("80%")
                && out.contains("ready")
        );
        assert!(!out.contains("### Todo"));
        let out = render_markdown_with_tasks(
            &[],
            &[TaskSnapshot {
                id: "outside".into(),
                title: "Outside history".into(),
                description: String::new(),
                status: TaskStatus::Todo,
                assignee: None,
                progress: 0,
                labels: vec![],
                note: String::new(),
                thread: None,
                created_ms: 1,
                updated_ms: 1,
            }],
        );
        assert!(out.contains("Outside history"));
    }

    fn env(seq: u64, ts_ms: u64, agent: AgentId, event: Event) -> Envelope {
        Envelope {
            session: "s".into(),
            agent,
            seq,
            ts_ms,
            event,
        }
    }

    fn one(event: Event) -> String {
        render_markdown(&[env(1, 0, AgentId::from("claude"), event)])
    }

    fn message(text: &str) -> Event {
        Event::Message {
            role: Role::Assistant,
            text: text.into(),
            delta: false,
        }
    }

    #[test]
    fn jsonl_is_one_envelope_per_line_in_order() {
        let events = [
            env(1, 10, AgentId::from("claude"), message("a")),
            env(
                2,
                20,
                AgentId::from("codex"),
                Event::Done {
                    reason: "ok".into(),
                },
            ),
        ];
        let out = render_jsonl(&events);
        assert!(out.ends_with('\n'));
        let parsed: Vec<Envelope> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(parsed, events);
        assert_eq!(render_jsonl(&[]), "");
    }

    #[test]
    fn empty_history() {
        let out = render_markdown(&[]);
        assert!(out.starts_with("# agentsflow session transcript"));
        assert!(out.contains("0 events"));
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn title_has_count_and_utc_range() {
        let events = [
            env(2, 1_700_000_005_000, AgentId::from("codex"), message("b")),
            env(1, 1_700_000_000_000, AgentId::from("claude"), message("a")),
        ];
        let out = render_markdown(&events);
        assert!(out.contains("2 events, 2023-11-14T22:13:20Z \u{2013} 2023-11-14T22:13:25Z UTC"));
        // Events keep the order they were given.
        assert!(out.find("claude (assistant):** b").is_none());
        assert!(out.find("codex (assistant):** b") < out.find("claude (assistant):** a"));
    }

    #[test]
    fn timestamps() {
        assert_eq!(format_ts(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_ts(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(format_ts(1_709_251_199_999), "2024-02-29T23:59:59Z");
        assert_eq!(format_ts(4_102_444_800_000), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn message_line() {
        let out = render_markdown(&[
            env(1, 0, AgentId::from("claude"), message("hello")),
            env(
                2,
                0,
                AgentId::orchestrator(),
                Event::Message {
                    role: Role::User,
                    text: "task".into(),
                    delta: false,
                },
            ),
        ]);
        assert!(out.contains("**claude (assistant):** hello\n"));
        assert!(out.contains("**orchestrator (user):** task\n"));
    }

    #[test]
    fn tool_call_is_one_line_with_short_args() {
        let out = one(Event::ToolCall {
            id: "1".into(),
            name: "read".into(),
            input: json!({"path": "a\nb"}),
        });
        let line = out.lines().find(|l| l.contains("`read`")).unwrap();
        assert!(line.contains(r#"`{"path":"a\nb"}`"#));

        let long = one(Event::ToolCall {
            id: "1".into(),
            name: "write".into(),
            input: json!({"content": "x".repeat(500)}),
        });
        let line = long.lines().find(|l| l.contains("`write`")).unwrap();
        assert!(line.contains(&"x".repeat(150)));
        assert!(!line.contains(&"x".repeat(200)));
    }

    #[test]
    fn tool_result_truncates_and_marks_errors() {
        let ok = one(Event::ToolResult {
            id: "1".into(),
            output: "fine".into(),
            is_error: false,
        });
        assert!(ok.contains("```\nfine\n```\n"));
        assert!(!ok.contains("error"));
        assert!(!ok.contains("truncated"));

        let exact = one(Event::ToolResult {
            id: "1".into(),
            output: "y".repeat(1500),
            is_error: false,
        });
        assert!(!exact.contains("truncated"));

        let long = one(Event::ToolResult {
            id: "1".into(),
            output: "я".repeat(1600),
            is_error: true,
        });
        assert!(long.contains("[error]"));
        assert!(long.contains(&"я".repeat(1500)));
        assert!(!long.contains(&"я".repeat(1501)));
        assert!(long.contains("[truncated, 100 more chars]"));
        // The note sits after the closing fence.
        assert!(long.find("[truncated").unwrap() > long.rfind("```").unwrap());
    }

    #[test]
    fn diff_block() {
        let out = one(Event::Diff {
            path: "src/a.rs".into(),
            patch: "-old\n+new".into(),
        });
        assert!(out.contains("`src/a.rs`"));
        assert!(out.contains("```diff\n-old\n+new\n```\n"));
    }

    #[test]
    fn permission_lines() {
        let out = render_markdown(&[
            env(
                1,
                0,
                AgentId::from("codex"),
                Event::PermissionRequest {
                    id: "p1".into(),
                    tool: "write".into(),
                    input: json!({}),
                    guard: Some(agentsflow_events::GuardNote {
                        rule: "rule".into(),
                        reason: "reason".into(),
                    }),
                },
            ),
            env(
                2,
                0,
                AgentId::from("codex"),
                Event::PermissionResolved {
                    id: "p1".into(),
                    allow: true,
                    blocked_by_guard: false,
                },
            ),
            env(
                3,
                0,
                AgentId::from("codex"),
                Event::PermissionResolved {
                    id: "p2".into(),
                    allow: false,
                    blocked_by_guard: false,
                },
            ),
        ]);
        assert!(out.contains("**codex** asked permission for `write`\n"));
        assert!(out.contains("Guard: rule - reason"));
        assert!(out.contains("**codex** permission `p1` allowed\n"));
        assert!(out.contains("**codex** permission `p2` denied\n"));
    }

    #[test]
    fn mail_blockquote_and_mail_read_skipped() {
        let mail = Event::Mail {
            id: "m1".into(),
            thread: "t".into(),
            from: AgentId::from("claude"),
            to: AgentId::from("codex"),
            subject: "split".into(),
            body: "line one\nline two".into(),
        };
        let read = Event::MailRead {
            agent: AgentId::from("codex"),
            ids: vec!["m1".into()],
        };
        let out = render_markdown(&[
            env(1, 0, AgentId::from("claude"), mail),
            env(2, 0, AgentId::from("codex"), read),
        ]);
        assert!(out.contains("> **claude -> codex** [split]\n>\n> line one\n> line two\n"));
        assert!(!out.contains("m1"));
        assert!(out.contains("2 events"));
        assert!(!out.contains("mail_read"));
        assert!(!out.contains("read"));
    }

    #[test]
    fn handoff_and_workflow_are_italic_lines() {
        let out = render_markdown(&[
            env(
                1,
                0,
                AgentId::orchestrator(),
                Event::Handoff {
                    from: AgentId::from("claude"),
                    to: AgentId::from("codex"),
                    step: "fix".into(),
                    text: "go\nnow".into(),
                },
            ),
            env(
                2,
                0,
                AgentId::orchestrator(),
                Event::Workflow {
                    name: "flow".into(),
                    step: Some("fix".into()),
                    status: WorkflowStatus::StepStarted,
                    detail: String::new(),
                },
            ),
            env(
                3,
                0,
                AgentId::orchestrator(),
                Event::Workflow {
                    name: "flow".into(),
                    step: None,
                    status: WorkflowStatus::Failed,
                    detail: "boom".into(),
                },
            ),
        ]);
        assert!(out.contains("*handoff claude -> codex [fix]: go now*\n"));
        assert!(out.contains("*workflow flow: step started (step fix)*\n"));
        assert!(out.contains("*workflow flow: failed - boom*\n"));
    }

    #[test]
    fn done_and_error_lines() {
        let done = one(Event::Done {
            reason: "completed".into(),
        });
        assert!(done.contains("**claude** done: completed\n"));
        let error = one(Event::Error {
            message: "bad\nthing".into(),
        });
        assert!(error.contains("**claude** error: bad thing\n"));
    }

    #[test]
    fn backticks_cannot_break_out_of_a_fence() {
        let nasty = "before\n```\n# not a heading\n````\nafter";
        for event in [
            Event::ToolResult {
                id: "1".into(),
                output: nasty.into(),
                is_error: false,
            },
            Event::Diff {
                path: "p".into(),
                patch: nasty.into(),
            },
        ] {
            let out = one(event);
            // The fence is longer than the longest run inside the text.
            assert!(out.contains("`````"));
            assert_eq!(out.lines().filter(|l| l.starts_with("`````")).count(), 2);
            let first = out.find("`````").unwrap();
            let last = out.rfind("`````").unwrap();
            let inside = &out[first..last];
            assert!(inside.contains("# not a heading"));
            assert!(inside.contains("after"));
        }
        assert_eq!(fenced("", "``").lines().next().unwrap(), "```");
        assert_eq!(fenced("", "``````").lines().next().unwrap(), "```````");
    }

    #[test]
    fn prose_cannot_open_a_fence() {
        let out = one(message("hi\n```rust\ncode\n~~~"));
        for line in out.lines() {
            assert!(!line.trim_start().starts_with("```"), "{line}");
            assert!(!line.trim_start().starts_with("~~~"), "{line}");
        }
        let mail = one(Event::Mail {
            id: "m".into(),
            thread: "t".into(),
            from: AgentId::from("claude"),
            to: AgentId::from("codex"),
            subject: "s".into(),
            body: "```\nx".into(),
        });
        for line in mail.lines().skip(3) {
            assert!(!line.trim_start_matches("> ").starts_with("```"), "{line}");
        }
    }

    #[test]
    fn no_raw_html() {
        let html = "<script>alert(1)</script><img src=x onerror=y>";
        let events = [
            message(html),
            Event::Mail {
                id: "m".into(),
                thread: "t".into(),
                from: AgentId::from("claude"),
                to: AgentId::from("codex"),
                subject: html.into(),
                body: html.into(),
            },
            Event::Handoff {
                from: AgentId::from("claude"),
                to: AgentId::from("codex"),
                step: html.into(),
                text: html.into(),
            },
            Event::Workflow {
                name: html.into(),
                step: Some(html.into()),
                status: WorkflowStatus::Started,
                detail: html.into(),
            },
            Event::Done {
                reason: html.into(),
            },
            Event::Error {
                message: html.into(),
            },
        ];
        for event in events {
            let out = one(event);
            assert!(!out.contains('<'), "{out}");
        }
    }

    #[test]
    fn inline_values_stay_on_one_line() {
        let out = one(Event::ToolCall {
            id: "1".into(),
            name: "we`ird\nname".into(),
            input: json!("`tick`"),
        });
        let line = out.lines().find(|l| l.contains("called")).unwrap();
        assert!(line.contains("``we`ird name``"));
        assert!(line.contains("``\"`tick`\"``"));
        assert_eq!(out.lines().count(), 5);
    }
}

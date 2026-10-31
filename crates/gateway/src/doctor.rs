//! `agentsflow doctor`: проверка окружения и, по желанию, живой прогон обоих агентов.
//! Живой прогон ловит смену формата вывода Claude или Codex, которую тесты без настоящих CLI не видят.

use agentsflow_adapters::{
    Agent,
    claude::{ClaudeOptions, ClaudeSession},
    codex::{CodexOptions, CodexSession},
};
use agentsflow_events::Event;
use std::{path::PathBuf, process::Command, time::Duration};

const LIVE_PROMPT: &str = "Reply with the single word ok. Do not use any tools.";
const LIVE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub live: bool,
    pub codex_bin: Option<PathBuf>,
}

impl Options {
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut options = Self {
            live: false,
            codex_bin: None,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--live" => options.live = true,
                "--codex-bin" => {
                    let value = args
                        .next()
                        .filter(|v| !v.is_empty() && !v.starts_with("--"))?;
                    options.codex_bin = Some(value.into());
                }
                _ => return None,
            }
        }
        Some(options)
    }
}

/// One line of the report: `✔` passed, `✖` failed, `·` informational.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Ok(String),
    Fail(String),
    Info(String),
}

impl Line {
    pub fn render(&self) -> String {
        match self {
            Self::Ok(text) => format!("✔ {text}"),
            Self::Fail(text) => format!("✖ {text}"),
            Self::Info(text) => format!("· {text}"),
        }
    }
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn version(bin: &std::path::Path) -> Result<String, String> {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("код выхода {:?}", output.status.code()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_owned())
}

fn tool_line(label: &str, bin: Option<PathBuf>, hint: &str) -> (Line, Option<PathBuf>) {
    match bin {
        None => (Line::Fail(format!("{label}: не найден. {hint}")), None),
        Some(bin) => match version(&bin) {
            Ok(text) => (
                Line::Ok(format!("{label}: {text} ({})", bin.display())),
                Some(bin),
            ),
            Err(error) => (
                Line::Fail(format!(
                    "{label}: {} не запускается: {error}",
                    bin.display()
                )),
                None,
            ),
        },
    }
}

async fn live_turn<A: Agent>(label: &str, mut agent: A) -> Line {
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(LIVE_TIMEOUT, async {
        agent.send(LIVE_PROMPT).await?;
        let (mut said, mut usage) = (false, false);
        loop {
            for event in agent.next_events().await? {
                match event {
                    Event::Message { .. } => said = true,
                    Event::Usage { .. } => usage = true,
                    Event::PermissionRequest { id, .. } => agent.respond(&id, false).await?,
                    Event::Error { message } => anyhow::bail!("ошибка агента: {message}"),
                    Event::Done { .. } => return Ok::<_, anyhow::Error>((said, usage)),
                    _ => {}
                }
            }
        }
    })
    .await;
    let _ = agent.shutdown().await;
    let seconds = started.elapsed().as_secs_f32();
    match result {
        Err(_) => Line::Fail(format!(
            "{label}: нет ответа за {} с",
            LIVE_TIMEOUT.as_secs()
        )),
        Ok(Err(error)) => Line::Fail(format!("{label}: {error:#}")),
        Ok(Ok((false, _))) => Line::Fail(format!(
            "{label}: ход закончился без ответа, возможно изменился формат вывода"
        )),
        Ok(Ok((true, usage))) => Line::Ok(format!(
            "{label}: живой ход прошёл за {seconds:.1} с{}",
            if usage {
                ", расход токенов получен"
            } else {
                ""
            }
        )),
    }
}

pub async fn run(options: Options) -> bool {
    let mut lines = Vec::new();
    let (git, _) = tool_line(
        "git",
        find_on_path(&["git", "git.exe"]),
        "Нужен для работы агентов с кодом.",
    );
    lines.push(git);
    let (claude, claude_bin) = tool_line(
        "claude",
        find_on_path(&["claude", "claude.exe", "claude.cmd"]),
        "Установите Claude Code и войдите в аккаунт.",
    );
    lines.push(claude);
    let codex_path = agentsflow_adapters::codex::find_binary(options.codex_bin.clone());
    let codex_found = if codex_path.is_absolute() || codex_path.components().count() > 1 {
        Some(codex_path)
    } else {
        find_on_path(&["codex", "codex.exe", "codex.cmd"])
    };
    let (codex, codex_bin) = tool_line(
        "codex",
        codex_found,
        "Установите Codex или укажите --codex-bin ПУТЬ либо переменную CODEX_BIN.",
    );
    lines.push(codex);
    let telegram = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    lines.push(Line::Info(format!(
        "Telegram: токен {}, список пользователей {} (значения не показываются)",
        if telegram("AGENTSFLOW_TELEGRAM_TOKEN") {
            "задан"
        } else {
            "не задан"
        },
        if telegram("AGENTSFLOW_TELEGRAM_USERS") {
            "задан"
        } else {
            "не задан"
        },
    )));
    if options.live {
        println!(
            "Живой прогон: каждый агент получает одну короткую задачу, это тратит немного токенов."
        );
        let dir = std::env::temp_dir();
        if claude_bin.is_some() {
            match ClaudeSession::start(ClaudeOptions {
                model: Some("haiku".into()),
                cwd: Some(dir.clone()),
                mail: None,
                effort: None,
                bypass: false,
            })
            .await
            {
                Ok(session) => lines.push(live_turn("claude (живой ход)", session).await),
                Err(error) => lines.push(Line::Fail(format!(
                    "claude: не удалось начать сессию: {error:#}"
                ))),
            }
        }
        if codex_bin.is_some() {
            match CodexSession::start(CodexOptions {
                bin: codex_bin,
                cwd: Some(dir),
                ..CodexOptions::default()
            })
            .await
            {
                Ok(session) => lines.push(live_turn("codex (живой ход)", session).await),
                Err(error) => lines.push(Line::Fail(format!(
                    "codex: не удалось начать сессию: {error:#}"
                ))),
            }
        }
    } else {
        lines.push(Line::Info(
            "Живой прогон агентов: agentsflow doctor --live".into(),
        ));
    }
    for line in &lines {
        println!("{}", line.render());
    }
    !lines.iter().any(|line| matches!(line, Line::Fail(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn parses_options() {
        assert_eq!(
            Options::parse(&args(&[])),
            Some(Options {
                live: false,
                codex_bin: None
            })
        );
        assert_eq!(
            Options::parse(&args(&["--live", "--codex-bin", "x"])),
            Some(Options {
                live: true,
                codex_bin: Some("x".into())
            })
        );
        assert!(Options::parse(&args(&["--codex-bin"])).is_none());
        assert!(Options::parse(&args(&["--bogus"])).is_none());
    }

    #[test]
    fn missing_tool_is_a_failure_with_a_hint() {
        let (line, bin) = tool_line("claude", None, "Установите.");
        assert!(bin.is_none());
        assert_eq!(line, Line::Fail("claude: не найден. Установите.".into()));
        assert_eq!(line.render(), "✖ claude: не найден. Установите.");
    }

    #[test]
    fn broken_binary_is_reported() {
        let (line, bin) = tool_line(
            "codex",
            Some(PathBuf::from("definitely-not-a-real-binary")),
            "",
        );
        assert!(bin.is_none());
        assert!(matches!(line, Line::Fail(text) if text.contains("не запускается")));
    }

    struct Scripted(Vec<Event>);
    impl Agent for Scripted {
        async fn send(&mut self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> anyhow::Result<Vec<Event>> {
            Ok(std::mem::take(&mut self.0))
        }
        async fn respond(&mut self, _: &str, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        async fn shutdown(self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn live_turn_detects_missing_answer_and_errors() {
        let message = Event::Message {
            role: agentsflow_events::Role::Assistant,
            text: "ok".into(),
            delta: false,
        };
        let done = Event::Done {
            reason: "success".into(),
        };
        let line = live_turn("a", Scripted(vec![message.clone(), done.clone()])).await;
        assert!(matches!(line, Line::Ok(text) if text.contains("живой ход прошёл")));
        let line = live_turn("a", Scripted(vec![done])).await;
        assert!(matches!(line, Line::Fail(text) if text.contains("без ответа")));
        let line = live_turn(
            "a",
            Scripted(vec![Event::Error {
                message: "boom".into(),
            }]),
        )
        .await;
        assert!(matches!(line, Line::Fail(text) if text.contains("boom")));
    }
}

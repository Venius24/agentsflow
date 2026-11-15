//! `agentsflow watch`: a separate small process that looks at the gateway
//! from outside. It asks `GET /api/alive` once a minute; when the gateway
//! stays silent for several asks in a row it tells the owner in Telegram
//! directly (not through the gateway) and, if the process is alive but hung,
//! ends it so that the existing restarter (the Windows task or systemd)
//! starts a new one.
//!
//! It shares nothing with the gateway but the bot token and the owner ids
//! from the environment, so a hang of the gateway cannot take it down.

use anyhow::{Context, Result, bail};
use std::{collections::VecDeque, time::Duration};

const KILL_GAP_MS: u64 = 5 * 60_000;
const MAX_KILLS_IN_A_ROW: u32 = 3;

/// What to do after one look at the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The gateway has been silent for `misses` asks; first noticed at `since_ms`.
    Alarm { since_ms: u64 },
    /// End the hung process.
    Kill,
    /// The gateway answers again after `down_ms`.
    Recovered { down_ms: u64 },
}

/// Counts asks and decides; knows nothing about HTTP or Telegram.
#[derive(Debug)]
pub struct Machine {
    need: u32,
    kill_hung: bool,
    missed: u32,
    down_since: Option<u64>,
    alarmed: bool,
    kills_in_a_row: u32,
    last_kill_ms: Option<u64>,
}

impl Machine {
    pub fn new(need: u32, kill_hung: bool) -> Self {
        Self {
            need: need.max(1),
            kill_hung,
            missed: 0,
            down_since: None,
            alarmed: false,
            kills_in_a_row: 0,
            last_kill_ms: None,
        }
    }

    pub fn step(&mut self, alive: bool, now_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        if alive {
            if self.alarmed
                && let Some(since) = self.down_since
            {
                actions.push(Action::Recovered {
                    down_ms: now_ms.saturating_sub(since),
                });
            }
            *self = Self::new(self.need, self.kill_hung);
            return actions;
        }
        self.missed += 1;
        self.down_since.get_or_insert(now_ms);
        if self.missed < self.need {
            return actions;
        }
        if !self.alarmed {
            self.alarmed = true;
            actions.push(Action::Alarm {
                since_ms: self.down_since.unwrap_or(now_ms),
            });
        }
        let gap_over = self
            .last_kill_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= KILL_GAP_MS);
        if self.kill_hung && self.kills_in_a_row < MAX_KILLS_IN_A_ROW && gap_over {
            self.kills_in_a_row += 1;
            self.last_kill_ms = Some(now_ms);
            actions.push(Action::Kill);
        }
        actions
    }
}

/// `HH:MM` at `offset_min` minutes east of UTC.
pub fn clock(ms: u64, offset_min: i32) -> String {
    let minutes = (ms / 60_000) as i64 + i64::from(offset_min);
    let day = minutes.rem_euclid(24 * 60);
    format!("{:02}:{:02}", day / 60, day % 60)
}

/// The pid that listens on 127.0.0.1:`port`, from `netstat -ano` (Windows) or
/// `ss -ltnpH` (Linux) output.
pub fn listening_pid(output: &str, port: u16) -> Option<u32> {
    let suffix = format!(":{port}");
    for line in output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        // netstat: TCP 127.0.0.1:7878 0.0.0.0:0 LISTENING 1234
        if words.first().is_some_and(|w| w.eq_ignore_ascii_case("tcp"))
            && words.iter().any(|w| w.eq_ignore_ascii_case("listening"))
            && words.get(1).is_some_and(|local| local.ends_with(&suffix))
            && let Some(pid) = words.last().and_then(|w| w.parse().ok())
        {
            return Some(pid);
        }
        // ss: LISTEN 0 128 127.0.0.1:7878 0.0.0.0:* users:(("agentsflow",pid=1234,fd=9))
        if words.first().is_some_and(|w| *w == "LISTEN")
            && words.iter().any(|w| w.ends_with(&suffix))
            && let Some(at) = line.find("pid=")
        {
            let digits: String = line[at + 4..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(pid) = digits.parse() {
                return Some(pid);
            }
        }
    }
    None
}

/// Ends the gateway that listens on `port`, and only if that process really is
/// `agentsflow`. Returns the pid, or why nothing was ended.
pub fn end_gateway(port: u16) -> Result<u32> {
    let (list, list_args): (&str, &[&str]) = if cfg!(windows) {
        ("netstat", &["-ano", "-p", "TCP"])
    } else {
        ("ss", &["-ltnpH"])
    };
    let out = std::process::Command::new(list)
        .args(list_args)
        .output()
        .with_context(|| format!("{list} не запустился"))?;
    let pid = listening_pid(&String::from_utf8_lossy(&out.stdout), port).with_context(|| {
        format!("порт {port} никто не слушает: процесса нет, перезапуск не нужен")
    })?;
    if !is_agentsflow(pid)? {
        bail!("порт {port} слушает не agentsflow (pid {pid}), не трогаю");
    }
    let status = if cfg!(windows) {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
    } else {
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
    }
    .context("не удалось завершить процесс")?;
    if !status.status.success() {
        bail!(
            "процесс {pid} не завершился: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    Ok(pid)
}

fn is_agentsflow(pid: u32) -> Result<bool> {
    if cfg!(windows) {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .context("tasklist не запустился")?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(image_is_agentsflow))
    } else {
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        Ok(name.trim() == "agentsflow")
    }
}

/// A `tasklist /FO CSV /NH` line of the gateway itself: `"agentsflow.exe","123",...`.
/// Not "anything with agentsflow in its name": the watcher's own copy and the
/// owner's other tools must never be ended by mistake.
pub fn image_is_agentsflow(line: &str) -> bool {
    line.split(',')
        .next()
        .map(|name| name.trim().trim_matches('"'))
        .is_some_and(|name| name.eq_ignore_ascii_case("agentsflow.exe"))
}

/// One ask: does the gateway answer `{"ok":true}` within five seconds?
pub async fn alive(client: &reqwest::Client, url: &str) -> bool {
    let Ok(response) = client.get(url).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response
        .text()
        .await
        .ok()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .is_some_and(|value| value["ok"] == true)
}

/// One message to one owner, straight to the Telegram API. True only when
/// Telegram took it. The error is printed without its address: the address
/// holds the bot token.
async fn send_one(client: &reqwest::Client, api: &str, token: &str, user: i64, text: &str) -> bool {
    let form = reqwest::multipart::Form::new()
        .text("chat_id", user.to_string())
        .text("text", text.to_owned());
    let result = client
        .post(format!("{api}/bot{token}/sendMessage"))
        .multipart(form)
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => true,
        Ok(response) => {
            eprintln!(
                "[watch] Telegram не принял сообщение: {}",
                response.status()
            );
            false
        }
        Err(error) => {
            eprintln!("[watch] Telegram недоступен: {}", error.without_url());
            false
        }
    }
}

/// Most messages kept for an owner who cannot be reached.
const OUTBOX_MAX: usize = 20;
/// Looks after which an undeliverable message is given up (about an hour).
const MAX_TRIES: u32 = 60;

#[derive(Debug)]
struct Pending {
    user: i64,
    text: String,
    tries: u32,
}

/// Messages the owner has not got yet. The network that failed the gateway may
/// have failed Telegram too: a message that did not go stays here and is tried
/// again at the next look, because an alarm that never arrives is worse than a
/// late one.
#[derive(Debug, Default)]
pub struct Outbox {
    items: VecDeque<Pending>,
}

impl Outbox {
    pub fn push(&mut self, users: &[i64], text: &str) {
        for user in users {
            self.items.push_back(Pending {
                user: *user,
                text: text.to_owned(),
                tries: 0,
            });
        }
        while self.items.len() > OUTBOX_MAX {
            self.items.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Tries every waiting message once, in order; what went is dropped, what
    /// did not stays (until it has failed `MAX_TRIES` times).
    pub async fn flush(&mut self, mut send: impl AsyncFnMut(i64, &str) -> bool) {
        let mut kept = VecDeque::new();
        while let Some(mut item) = self.items.pop_front() {
            if send(item.user, &item.text).await {
                continue;
            }
            item.tries += 1;
            if item.tries < MAX_TRIES {
                kept.push_back(item);
            }
        }
        self.items = kept;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub home: std::path::PathBuf,
    /// Look once, say what was seen, exit: for checking the setup.
    pub once: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut home = None;
        let mut once = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--home" => {
                    home = Some(std::path::PathBuf::from(
                        args.next().context("--home needs a folder")?,
                    ));
                }
                "--once" => once = true,
                other => bail!("unknown option {other}"),
            }
        }
        Ok(Self {
            home: home.context("usage: agentsflow watch --home DIR [--once]")?,
            once,
        })
    }
}

pub async fn run(options: Options) -> Result<()> {
    // Only the settings: the watcher must live when a department is broken.
    let config = crate::home::Config::load(&options.home)?;
    let watch = config.watch.clone();
    let telegram = crate::telegram::Config::from_env()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let port = watch.port()?;
    let offset = config.offset_minutes();
    let mut machine = Machine::new(watch.misses, watch.kill_hung);
    let mut outbox = Outbox::default();
    let mut log: VecDeque<String> = VecDeque::new();
    eprintln!(
        "[watch] слежу за {} раз в {} с; тревога после {} неудач подряд",
        watch.alive_url, watch.interval_secs, watch.misses
    );
    loop {
        let is_up = alive(&client, &watch.alive_url).await;
        if options.once {
            println!(
                "{}",
                if is_up {
                    "шлюз отвечает"
                } else {
                    "шлюз не отвечает"
                }
            );
            return if is_up {
                Ok(())
            } else {
                bail!("шлюз не отвечает")
            };
        }
        let now = unix_ms();
        for action in machine.step(is_up, now) {
            let text = match action {
                Action::Alarm { since_ms } => format!(
                    "Шлюз agentsflow не отвечает с {}. Запуски отделов могут стоять.",
                    clock(since_ms, offset)
                ),
                Action::Recovered { down_ms } => format!(
                    "Шлюз agentsflow снова работает (не отвечал {} мин). Что было прервано, пришлёт сам шлюз.",
                    down_ms.div_ceil(60_000)
                ),
                Action::Kill => match end_gateway(port) {
                    Ok(pid) => format!(
                        "Процесс шлюза (pid {pid}) завис и завершён. Автозапуск поднимет новый."
                    ),
                    Err(error) => {
                        // Nothing to end: the process is gone, its restarter has it.
                        log.push_back(format!("{error:#}"));
                        while log.len() > 5 {
                            log.pop_front();
                        }
                        eprintln!("[watch] {error:#}");
                        continue;
                    }
                },
            };
            eprintln!("[watch] {text}");
            outbox.push(telegram.users(), &text);
        }
        if !outbox.is_empty() {
            outbox
                .flush(async |user, text| {
                    send_one(&client, telegram.api(), telegram.token(), user, text).await
                })
                .await;
            if !outbox.is_empty() {
                eprintln!(
                    "[watch] не дошло до Telegram: {} сообщ., повторю при следующем взгляде",
                    outbox.len()
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(watch.interval_secs)).await;
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_gateway_itself_may_be_ended() {
        assert!(image_is_agentsflow(
            r#""agentsflow.exe","92552","Console","1","210 000 K""#
        ));
        assert!(image_is_agentsflow(
            r#""AgentsFlow.EXE","1","Services","0","1 K""#
        ));
        assert!(!image_is_agentsflow(
            r#""agentsflow-watch.exe","7","Console","1","1 K""#
        ));
        assert!(!image_is_agentsflow(
            r#""notagentsflow.exe","7","Console","1","1 K""#
        ));
        assert!(!image_is_agentsflow(
            "INFO: No tasks are running which match the specified criteria."
        ));
        assert!(!image_is_agentsflow(""));
    }

    #[test]
    fn an_alarm_comes_after_enough_misses_and_only_once() {
        let mut machine = Machine::new(3, false);
        assert!(machine.step(false, 0).is_empty());
        assert!(machine.step(false, 60_000).is_empty());
        assert_eq!(
            machine.step(false, 120_000),
            vec![Action::Alarm { since_ms: 0 }]
        );
        assert!(machine.step(false, 180_000).is_empty(), "said once");
        assert!(machine.step(false, 240_000).is_empty());
    }

    #[test]
    fn one_answer_between_misses_resets_the_count() {
        let mut machine = Machine::new(3, false);
        machine.step(false, 0);
        machine.step(false, 60_000);
        assert!(
            machine.step(true, 120_000).is_empty(),
            "no alarm, nothing to recover from"
        );
        machine.step(false, 180_000);
        assert!(machine.step(false, 240_000).is_empty());
    }

    #[test]
    fn recovery_is_reported_after_an_alarm() {
        let mut machine = Machine::new(2, false);
        machine.step(false, 0);
        machine.step(false, 60_000);
        assert_eq!(
            machine.step(true, 300_000),
            vec![Action::Recovered { down_ms: 300_000 }]
        );
        assert!(machine.step(true, 360_000).is_empty());
    }

    #[test]
    fn a_hung_process_is_ended_at_most_every_five_minutes_and_three_times() {
        let mut machine = Machine::new(1, true);
        let kills = |actions: &[Action]| actions.iter().filter(|a| **a == Action::Kill).count();
        assert_eq!(kills(&machine.step(false, 0)), 1);
        assert_eq!(kills(&machine.step(false, 60_000)), 0, "too soon");
        assert_eq!(kills(&machine.step(false, 299_999)), 0);
        assert_eq!(kills(&machine.step(false, 300_000)), 1);
        assert_eq!(kills(&machine.step(false, 600_000)), 1);
        assert_eq!(
            kills(&machine.step(false, 900_000)),
            0,
            "three in a row is enough"
        );
        assert_eq!(kills(&machine.step(false, 9_000_000)), 0);
        // After it came back, the allowance is new.
        machine.step(true, 9_060_000);
        assert_eq!(kills(&machine.step(false, 9_120_000)), 1);
    }

    #[test]
    fn nothing_is_ended_when_the_owner_did_not_allow_it() {
        let mut machine = Machine::new(1, false);
        assert!(
            machine
                .step(false, 0)
                .iter()
                .all(|action| *action != Action::Kill)
        );
    }

    #[test]
    fn the_listener_is_found_in_both_outputs() {
        let netstat = "\
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1000
  TCP    127.0.0.1:7878         0.0.0.0:0              LISTENING       4242
  TCP    127.0.0.1:7878         127.0.0.1:50000        ESTABLISHED     4242
";
        assert_eq!(listening_pid(netstat, 7878), Some(4242));
        assert_eq!(listening_pid(netstat, 7879), None);
        let ss = "LISTEN 0 128 127.0.0.1:7878 0.0.0.0:* users:((\"agentsflow\",pid=777,fd=9))\n";
        assert_eq!(listening_pid(ss, 7878), Some(777));
        assert_eq!(listening_pid(ss, 78), None);
    }

    #[tokio::test]
    async fn a_message_that_did_not_go_is_sent_at_the_next_look() {
        let mut outbox = Outbox::default();
        outbox.push(&[1, 2], "тревога");
        let sent = std::cell::RefCell::new(Vec::new());
        let down = std::cell::Cell::new(true);
        let mut send = async |user: i64, text: &str| {
            if down.get() && user == 1 {
                return false;
            }
            sent.borrow_mut().push((user, text.to_owned()));
            true
        };
        outbox.flush(&mut send).await;
        assert_eq!(*sent.borrow(), vec![(2, "тревога".to_owned())]);
        assert_eq!(outbox.len(), 1, "the one that failed waits");
        down.set(false);
        outbox.flush(&mut send).await;
        assert_eq!(sent.borrow().len(), 2);
        assert!(outbox.is_empty());
        outbox.flush(&mut send).await;
        assert_eq!(sent.borrow().len(), 2, "nothing is sent twice");
    }

    #[tokio::test]
    async fn an_owner_who_cannot_be_reached_does_not_pile_up_messages() {
        let mut outbox = Outbox::default();
        for n in 0..(OUTBOX_MAX + 5) {
            outbox.push(&[1], &format!("m{n}"));
        }
        assert_eq!(outbox.len(), OUTBOX_MAX);
        let mut never = async |_: i64, _: &str| false;
        for _ in 0..MAX_TRIES {
            outbox.flush(&mut never).await;
        }
        assert!(outbox.is_empty(), "given up after a long while");
    }

    #[test]
    fn clock_follows_the_owners_offset() {
        assert_eq!(clock(0, 0), "00:00");
        assert_eq!(clock(13 * 3_600_000 + 5 * 60_000, 5 * 60), "18:05");
        assert_eq!(clock(23 * 3_600_000, 2 * 60), "01:00");
        assert_eq!(clock(3_600_000, -2 * 60), "23:00");
    }

    #[test]
    fn options_need_a_home() {
        let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(Options::parse(&args(&[])).is_err());
        assert!(Options::parse(&args(&["--home"])).is_err());
        assert!(Options::parse(&args(&["--home", "h", "--x"])).is_err());
        let options = Options::parse(&args(&["--home", "h", "--once"])).unwrap();
        assert!(options.once);
    }

    #[tokio::test]
    async fn the_ask_wants_a_real_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((mut stream, _)) = listener.accept().await {
                n += 1;
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await;
                let body = match n {
                    1 => r#"{"ok":true,"version":"1"}"#,
                    2 => r#"{"ok":false}"#,
                    _ => "not json",
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/api/alive");
        assert!(alive(&client, &url).await);
        assert!(!alive(&client, &url).await, "ok:false is not alive");
        assert!(!alive(&client, &url).await, "not json is not alive");
        assert!(
            !alive(&client, "http://127.0.0.1:1/api/alive").await,
            "nobody listens"
        );
    }
}

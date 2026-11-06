//! Sites a department logs in to. The owner keeps logins and passwords in
//! `<home>/.agentsflow/sites.toml`: the folder is out of git and closed to
//! departments by the guard. An agent calls the `site_login` tool and sees only
//! the outcome; this process fills the form, so the password never reaches the
//! model or the event log. After that the agent works in the same browser
//! through its browser tools.

use crate::cdp::{Browser, Page, find_chrome};
use agentsflow_adapters::ExtraMcp;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const FILE: &str = "sites.toml";
const PLAYWRIGHT_MCP: &str = "@playwright/mcp@0.0.84";
const POLL: Duration = Duration::from_millis(500);
/// How long a page may stay without a login form before it counts as logged in:
/// sites redirect to their login page only after several seconds.
const SETTLE: Duration = Duration::from_secs(8);
/// How long a form may stay on screen after it was submitted.
const ANSWER: Duration = Duration::from_secs(15);
/// A form still silent after this gets a second way in: Enter in its field.
const NUDGE: Duration = Duration::from_secs(5);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);
/// How long Chrome keeps a login page open for a code from the owner.
const CODE_WAIT: Duration = Duration::from_secs(15 * 60);
const WAITING_FILE: &str = "waiting-for-code";
/// After a refusal the site is left alone for a while: more attempts and more
/// SMS can lock the account. Saving the sites file lifts the pause.
const PAUSE: Duration = Duration::from_secs(30 * 60);
const DEBUG_FILE: &str = "login-debug.log";

/// No `Debug`: this struct holds a password.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// Where to go to work.
    pub url: String,
    /// A part of the address of the login pages, e.g. the host of the login service.
    pub login_url: String,
    pub login: String,
    pub password: String,
    /// Text of a button to press first, e.g. the "Email" tab.
    #[serde(default)]
    pub before_login: Option<String>,
    #[serde(default)]
    pub login_selector: Option<String>,
    #[serde(default)]
    pub password_selector: Option<String>,
    #[serde(default)]
    pub code_selector: Option<String>,
    #[serde(default)]
    pub submit_selector: Option<String>,
    #[serde(default)]
    pub remember_selector: Option<String>,
    #[serde(default)]
    pub captcha_selector: Option<String>,
}

impl Site {
    /// A blank block is a site the owner has not got to yet; the others work.
    fn filled(&self) -> bool {
        !self.login.trim().is_empty() && !self.password.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sites {
    #[serde(default)]
    pub chrome: Option<PathBuf>,
    /// Show the browser window; useful on the owner's own PC.
    #[serde(default)]
    pub show_browser: bool,
    #[serde(default)]
    pub site: BTreeMap<String, Site>,
}

pub fn file(home: &Path) -> PathBuf {
    home.join(".agentsflow").join(FILE)
}

impl Sites {
    pub fn parse(text: &str) -> Result<Self> {
        // The parser quotes the offending line, and that line may hold a
        // password: an agent must get the line number only.
        let sites: Self = toml::from_str(text).map_err(|error| {
            let line = error
                .span()
                .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
            match line {
                Some(line) => anyhow::anyhow!("{FILE}: ошибка в строке {line}"),
                None => anyhow::anyhow!("{FILE}: ошибка в файле"),
            }
        })?;
        for (name, site) in &sites.site {
            if !site.url.starts_with("https://") && !site.url.starts_with("http://") {
                bail!("{FILE}: у сайта {name} адрес должен начинаться с https://");
            }
            if site.login_url.trim().is_empty() {
                bail!("{FILE}: у сайта {name} не заполнен login_url");
            }
        }
        Ok(sites)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "нет файла {}: хозяин ещё не записал логины и пароли",
                path.display()
            )
        })?;
        Self::parse(&text)
    }
}

/// A stable port per department: its Chrome is found again by the next run.
pub fn debug_port(department: &str) -> u16 {
    let hash = department.bytes().fold(2166136261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16777619)
    });
    19300 + (hash % 500) as u16
}

fn profile_dir(home: &Path, department: &str) -> PathBuf {
    home.join(".agentsflow").join("browser").join(department)
}

/// Closes the Chrome of a department when its run ends: a logged-in browser
/// must not stay open for other agents on this machine. A page waiting for a
/// code stays open for a while, so the next run can enter it.
pub async fn after_run(home: PathBuf, department: String) {
    let waiting = profile_dir(&home, &department).join(WAITING_FILE);
    if waiting.is_file() {
        tokio::time::sleep(CODE_WAIT).await;
        // The next run has taken the page over.
        if !waiting.is_file() {
            return;
        }
        let _ = std::fs::remove_file(&waiting);
    }
    if let Ok(browser) = Browser::connect(debug_port(&department)).await {
        browser.close().await;
    }
}

/// Problems of a department's sites for `home check`.
pub fn problems(home: &Path, department: &str, names: &[String]) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    let path = file(home);
    let sites = match Sites::load(&path) {
        Ok(sites) => sites,
        Err(error) => return vec![format!("{department}: {error:#}")],
    };
    let mut out = Vec::new();
    for name in names {
        match sites.site.get(name) {
            None => out.push(format!(
                "{department}: сайта {name} нет в {}",
                path.display()
            )),
            Some(site) if !site.filled() => out.push(format!(
                "{department}: у сайта {name} не заполнены login и password"
            )),
            Some(_) => {}
        }
    }
    if find_chrome(sites.chrome.as_deref()).is_none() {
        out.push(format!(
            "{department}: Chrome не найден, укажи chrome в {FILE}"
        ));
    }
    out
}

/// What the agent's Claude gets: the login tool and browser tools for one Chrome.
pub fn access(home: &Path, department: &str, names: &[String], run_dir: &Path) -> Result<ExtraMcp> {
    let exe = std::env::current_exe()?;
    let port = debug_port(department);
    let profile = profile_dir(home, department);
    let (command, mut args) = if cfg!(windows) {
        ("cmd", vec!["/c".to_owned(), "npx".to_owned()])
    } else {
        ("npx", Vec::new())
    };
    args.extend([
        "-y".to_owned(),
        PLAYWRIGHT_MCP.to_owned(),
        "--cdp-endpoint".to_owned(),
        format!("http://127.0.0.1:{port}"),
        "--output-dir".to_owned(),
        run_dir
            .join("artifacts")
            .join("browser")
            .display()
            .to_string(),
    ]);
    Ok(ExtraMcp {
        servers: json!({
            "site": {
                "command": exe,
                "args": [
                    "site-mcp",
                    "--sites", file(home),
                    "--profile", profile,
                    "--port", port.to_string(),
                    "--allow", names.join(","),
                ],
            },
            "browser": {"command": command, "args": args},
        }),
        allowed: vec!["mcp__site__site_login".into(), "mcp__browser".into()],
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    LoggedIn,
    CodeRequired,
    WrongCode,
    WrongCredentials,
    /// The form stayed and the site said nothing: not a refusal, so no pause.
    NoReaction,
    /// The code was right but late, and the site can't send a new one yet.
    CodeExpired,
    /// The code was late and the form asked the site for a new one.
    CodeResent,
    Captcha,
    NoForm,
}

impl Outcome {
    fn pauses(self) -> bool {
        matches!(
            self,
            Self::WrongCode | Self::WrongCredentials | Self::Captcha
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::LoggedIn => "logged_in",
            Self::CodeRequired => "code_required",
            Self::WrongCode => "wrong_code",
            Self::WrongCredentials => "wrong_credentials",
            Self::NoReaction => "no_reaction",
            Self::CodeExpired => "code_expired",
            Self::CodeResent => "code_resent",
            Self::Captcha => "captcha",
            Self::NoForm => "no_form",
        }
    }

    fn message(self, name: &str) -> String {
        match self {
            Self::LoggedIn => format!(
                "Вход на {name} выполнен. Дальше работай в этом же браузере через инструменты browser."
            ),
            Self::CodeRequired => format!(
                "{name} просит код подтверждения. Посмотри на странице через browser, куда он отправлен (почта, SMS, приложение), попроси его у хозяина через questions.md и закончи работу; браузер подождёт. Потом вызови site_login с этим кодом."
            ),
            Self::WrongCode => format!(
                "{name} не принял код. Вход на паузе на 30 минут, чтобы не заблокировать учётную запись: не проси новый код и не входи снова. Сообщи хозяину об этом в result.md и закончи работу."
            ),
            Self::WrongCredentials => format!(
                "{name} не принял логин или пароль. Вход на паузе на 30 минут, чтобы не заблокировать учётную запись: не входи снова. Сообщи хозяину об этом в result.md и закончи работу."
            ),
            Self::NoReaction => format!(
                "{name} никак не ответил на отправку формы и не показал ошибки. Это не отказ сайта: возможно, инструмент не справился с формой. Не проси новый код и не входи снова. Сообщи хозяину об этом в result.md и закончи работу."
            ),
            Self::CodeExpired => format!(
                "{name}: срок действия кода истёк, он дошёл слишком поздно. Новый код сайт пока не даёт отправить. Попроси хозяина через questions.md написать, когда он будет готов сразу ответить, и закончи работу. Это не отказ сайта и не ошибка хозяина."
            ),
            Self::CodeResent => format!(
                "{name}: срок действия кода истёк, он дошёл слишком поздно, поэтому сайт уже отправил новый код. Это не ошибка хозяина. Попроси новый код через questions.md, напиши, что отвечать надо сразу, потому что код живёт всего несколько минут, и закончи работу; браузер подождёт. Потом вызови site_login с этим кодом."
            ),
            Self::Captcha => format!(
                "{name} показывает капчу. Обходить её нельзя: сообщи хозяину через questions.md и закончи работу."
            ),
            Self::NoForm => format!(
                "Не нашёл форму входа на {name}. Сообщи хозяину через questions.md: нужно поправить настройки сайта в {FILE}."
            ),
        }
    }
}

/// The page as the login sees it. Field values never leave the page, only
/// whether a field is on screen and where.
const PROBE: &str = r#"(function(cfg){
  var on = function(e){
    var r = e.getBoundingClientRect(), s = getComputedStyle(e);
    if (!r.width || !r.height || s.visibility === 'hidden' || s.display === 'none' || e.disabled) return false;
    // Only vertically: a hidden tab sits beside the visible one, and scrolling sideways would reveal it.
    if (r.top < 0 || r.bottom > innerHeight) { e.scrollIntoView({block:'center', inline:'nearest'}); r = e.getBoundingClientRect(); }
    var top = document.elementFromPoint(r.left + r.width/2, r.top + r.height/2);
    return !!top && (top === e || e.contains(top) || (e.labels && [].some.call(e.labels, function(l){return l.contains(top)})));
  };
  var first = function(selector){ return [].filter.call(document.querySelectorAll(selector), on)[0]; };
  var byText = function(texts, root){
    var nodes = (root || document).querySelectorAll('button,input[type=submit],[role=button],[role=tab],a,label,div,span');
    var found = [].filter.call(nodes, function(e){
      var t = (e.innerText || e.value || '').trim().toLowerCase();
      return texts.indexOf(t) >= 0 && on(e);
    });
    // A wrapper has the same text as its button: take the innermost.
    return found.filter(function(e){ return !found.some(function(o){ return o !== e && e.contains(o); }); })[0];
  };
  // The button that belongs to the field: the closest one around it.
  var near = function(field, texts){
    for (var n = field && field.parentElement; n; n = n.parentElement) { var found = byText(texts, n); if (found) return found; }
    return byText(texts);
  };
  // "Remember me" may be asked at any step; a custom box hides its input and shows a label.
  var rememberTarget = function(){
    if (cfg.remember) { var own = first(cfg.remember); return own && !own.checked ? own : null; }
    var boxes = document.querySelectorAll('input[type=checkbox],[role=checkbox]');
    for (var i = 0; i < boxes.length; i++) {
      var box = boxes[i], around = box, words = '';
      for (var up = 0; up < 4 && around && !words; up++) { around = around.parentElement; words = around ? (around.innerText || '').trim().toLowerCase() : ''; }
      if (!/^(запомнить|не выходить|remember|keep me|stay signed)/.test(words)) continue;
      var label = box.closest('label');
      var target = on(box) ? box : (label && on(label) ? label : null);
      if (!target) continue;
      return box.checked || box.getAttribute('aria-checked') === 'true' ? null : target;
    }
    return null;
  };
  // The site's own words of refusal; a toast may live for a second only.
  var refusals = (document.body ? document.body.innerText : '').split('\n').map(function(l){ return l.trim(); }).filter(function(l){
    return l.length <= 200 && /невер|неправил|ошибк|не найден|истек|истёк|не подход|заблокир|попробуйте позже|слишком много|invalid|incorrect|wrong|expired|error|try again/i.test(l);
  });
  var spot = function(e){ if(!e) return null; var r = e.getBoundingClientRect(); return {x:r.left+r.width/2, y:r.top+r.height/2}; };
  var ready = document.readyState === 'complete';
  var password = first(cfg.password);
  var code = first(cfg.code);
  var login = first(cfg.login);
  if (login && (login === code || login === password)) login = null;
  // A code may be split into cells, one digit each.
  var cells = [].filter.call(document.querySelectorAll(cfg.code), on);
  if (code && code.maxLength === 1) cells = [].filter.call(document.querySelectorAll('input[maxlength="1"]'), on);
  var captcha = !!first(cfg.captcha);
  var resend = code ? near(code, cfg.resendTexts) : null;
  var submit = cfg.submit ? first(cfg.submit) : near(code || password || login, cfg.submitTexts);
  var before = cfg.before ? byText([cfg.before.toLowerCase()]) : null;
  var remember = rememberTarget();
  return JSON.stringify({
    url: location.href,
    ready: ready,
    captcha: captcha,
    login: spot(login),
    password: spot(password),
    code: spot(code),
    cells: cells.map(spot),
    refusals: refusals,
    expired: refusals.filter(function(l){ return /истек|истёк|устарел|expired/i.test(l); }),
    resend: spot(resend),
    submit: spot(submit),
    before: spot(before),
    remember: spot(remember)
  });
})"#;

fn probe_script(site: &Site) -> String {
    let config = json!({
        "login": site.login_selector.as_deref().unwrap_or(
            "input[type=email],input[type=tel],input[name*=login i],input[name*=user i],input[id*=login i],input[id*=user i],input[type=text]"
        ),
        "password": site.password_selector.as_deref().unwrap_or("input[type=password]"),
        "code": site.code_selector.as_deref().unwrap_or(
            "input[autocomplete=one-time-code],input[name*=code i],input[id*=code i],input[name*=otp i],input[id*=otp i]"
        ),
        "captcha": site.captcha_selector.as_deref().unwrap_or(
            "iframe[src*='recaptcha/api2/anchor'],iframe[src*=hcaptcha],iframe[src*=turnstile],img[src*=captcha i],input[name*=captcha i],input[id*=captcha i]"
        ),
        "submit": site.submit_selector,
        "submitTexts": ["войти", "вход", "продолжить", "далее", "подтвердить", "login", "log in", "sign in", "continue", "next", "submit"],
        "resendTexts": ["отправить новый код", "отправить код повторно", "отправить код ещё раз", "отправить код еще раз", "resend code", "send new code", "resend"],
        "before": site.before_login,
        "remember": site.remember_selector,
    });
    format!("{PROBE}({config})")
}

/// What the owner needs to see why a login stopped: the shape of the form and
/// the page's own words. Field values are not read, only their lengths, and
/// not even that for a password.
const SNAPSHOT: &str = r#"(function(){
  var on = function(e){ var r = e.getBoundingClientRect(), s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; };
  return JSON.stringify({
    url: location.origin + location.pathname,
    size: [innerWidth, innerHeight],
    checks: [].map.call(document.querySelectorAll('input[type=checkbox],[role=checkbox]'), function(b){
      var a = b, words = '', shape = [];
      for (var up = 0; up < 4 && a && !words; up++) { a = a.parentElement; if (a) { words = (a.innerText || '').trim(); shape.push(a.tagName.toLowerCase()); } }
      var r = b.getBoundingClientRect();
      return {text: words.slice(0, 40), shape: shape.join('<'), place: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)], checked: b.checked || b.getAttribute('aria-checked') === 'true'};
    }),
    inputs: [].filter.call(document.querySelectorAll('input'), on).map(function(i){
      return {type:i.type, id:i.id, name:i.name, placeholder:i.placeholder, maxLength:i.maxLength, autocomplete:i.autocomplete, length: i.type === 'password' ? null : i.value.length};
    }),
    buttons: [].filter.call(document.querySelectorAll('button,[role=button],a,input[type=submit]'), on).map(function(b){
      return (b.innerText || b.value || '').trim().slice(0, 40) + (b.disabled ? ' [disabled]' : '');
    }),
    text: document.body.innerText.slice(0, 800)
  });
})()"#;

async fn note(page: &mut Page<'_>, debug: Option<&Path>, stage: &str) {
    let Some(path) = debug else { return };
    let snapshot = page.evaluate(SNAPSHOT).await.unwrap_or(Value::Null);
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    let line = json!({"unix": at, "stage": stage, "page": snapshot});
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }
}

fn spot(state: &Value, field: &str) -> Option<(f64, f64)> {
    let place = state.get(field)?;
    Some((place.get("x")?.as_f64()?, place.get("y")?.as_f64()?))
}

async fn fill(page: &mut Page<'_>, place: (f64, f64), text: &str) -> Result<()> {
    page.click(place.0, place.1).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    page.type_text(text).await
}

/// Enters a code key by key, as a person does: code fields often react only to
/// keys, and split ones move on by themselves or take a click per cell.
async fn enter_code(page: &mut Page<'_>, state: &Value, code: &str) -> Result<()> {
    let cells: Vec<(f64, f64)> = state["cells"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cell| Some((cell.get("x")?.as_f64()?, cell.get("y")?.as_f64()?)))
        .collect();
    let digits: Vec<char> = code.chars().collect();
    if cells.len() > 1 && cells.len() == digits.len() {
        for (cell, digit) in cells.iter().zip(&digits) {
            page.click(cell.0, cell.1).await?;
            page.type_keys(&digit.to_string()).await?;
        }
        return Ok(());
    }
    let place = spot(state, "code").context("no code field")?;
    page.click(place.0, place.1).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    page.clear_field().await?;
    page.type_keys(code).await
}

async fn submit(page: &mut Page<'_>, state: &Value) -> Result<()> {
    match spot(state, "submit") {
        Some(place) => page.click(place.0, place.1).await,
        None => page.press_enter().await,
    }
}

/// A form sent to the site, while we wait for its answer.
struct Sent {
    at: Instant,
    /// Warnings that were on the page before, so they are not taken for the answer.
    known: Vec<Value>,
    nudged: bool,
}

impl Sent {
    fn new(state: &Value) -> Self {
        Self {
            at: Instant::now(),
            known: state["refusals"].as_array().cloned().unwrap_or_default(),
            nudged: false,
        }
    }
}

/// The form is still on screen. Only the site's own words count as a refusal;
/// silence means our click may have missed, so Enter gets one try.
async fn watch(
    page: &mut Page<'_>,
    sent: &mut Sent,
    state: &Value,
    field: (f64, f64),
    refused: Outcome,
    debug: Option<&Path>,
) -> Result<Option<Outcome>> {
    let said_no = state["refusals"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|line| !sent.known.contains(line));
    if said_no {
        let late = state["expired"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|line| !sent.known.contains(line));
        if late && refused == Outcome::WrongCode {
            return Ok(Some(match spot(state, "resend") {
                Some(place) => {
                    page.click(place.0, place.1).await?;
                    note(page, debug, "asked_for_new_code").await;
                    Outcome::CodeResent
                }
                None => Outcome::CodeExpired,
            }));
        }
        return Ok(Some(refused));
    }
    if !sent.nudged && sent.at.elapsed() > NUDGE {
        sent.nudged = true;
        page.click(field.0, field.1).await?;
        page.press_enter().await?;
        note(page, debug, "pressed_enter").await;
    }
    if sent.at.elapsed() > ANSWER {
        return Ok(Some(Outcome::NoReaction));
    }
    Ok(None)
}

/// Fills the login form of `site`, step by step, until the page lets us in or
/// asks for something only the owner can give. A page already waiting for a
/// code stays where it is: going back to the start would send another code.
pub async fn login(
    page: &mut Page<'_>,
    site: &Site,
    code: Option<&str>,
    debug: Option<&Path>,
) -> Result<Outcome> {
    let script = probe_script(site);
    // A page that is navigating has no execution context for a moment.
    let here = page.evaluate(&script).await.unwrap_or(Value::Null);
    let waiting_for_code = spot(&here, "code").is_some()
        && here["url"]
            .as_str()
            .is_some_and(|url| url.contains(&site.login_url));
    if !waiting_for_code {
        page.navigate(&site.url).await?;
    }
    let started = Instant::now();
    let mut quiet_since = Instant::now();
    let (mut login_sent, mut password_sent, mut code_sent): (
        Option<Sent>,
        Option<Sent>,
        Option<Sent>,
    ) = (None, None, None);
    let mut pressed_before = false;
    while started.elapsed() < LOGIN_TIMEOUT {
        tokio::time::sleep(POLL).await;
        // A page that is navigating has no execution context for a moment.
        let Ok(state) = page.evaluate(&script).await else {
            continue;
        };
        let url = state["url"].as_str().unwrap_or_default();
        if url.starts_with("about:") || !state["ready"].as_bool().unwrap_or(false) {
            continue;
        }
        if !url.contains(&site.login_url) {
            if quiet_since.elapsed() >= SETTLE {
                return Ok(Outcome::LoggedIn);
            }
            continue;
        }
        quiet_since = Instant::now();
        if state["captcha"].as_bool().unwrap_or(false) {
            return Ok(Outcome::Captcha);
        }
        let (field_login, field_password, field_code) = (
            spot(&state, "login"),
            spot(&state, "password"),
            spot(&state, "code"),
        );
        if let (Some(place), None) = (field_code, field_password) {
            let Some(code) = code else {
                return Ok(Outcome::CodeRequired);
            };
            match code_sent.as_mut() {
                None => {
                    let state = match spot(&state, "remember") {
                        Some(remember) => {
                            page.click(remember.0, remember.1).await?;
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            page.evaluate(&script).await?
                        }
                        None => state,
                    };
                    enter_code(page, &state, code).await?;
                    // Many forms go on by themselves on the last digit.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    note(page, debug, "code_typed").await;
                    let state = page.evaluate(&script).await?;
                    let sent = Sent::new(&state);
                    if spot(&state, "code").is_some() {
                        submit(page, &state).await?;
                    }
                    code_sent = Some(sent);
                }
                Some(sent) => {
                    let found = watch(page, sent, &state, place, Outcome::WrongCode, debug).await?;
                    if let Some(outcome) = found {
                        return Ok(outcome);
                    }
                }
            }
        } else if let Some(place) = field_password {
            match password_sent.as_mut() {
                None => {
                    if let Some(login_place) = field_login {
                        fill(page, login_place, &site.login).await?;
                    }
                    fill(page, place, &site.password).await?;
                    if let Some(remember) = spot(&state, "remember") {
                        page.click(remember.0, remember.1).await?;
                    }
                    let state = page.evaluate(&script).await?;
                    let sent = Sent::new(&state);
                    submit(page, &state).await?;
                    password_sent = Some(sent);
                }
                Some(sent) => {
                    let found =
                        watch(page, sent, &state, place, Outcome::WrongCredentials, debug).await?;
                    if let Some(outcome) = found {
                        return Ok(outcome);
                    }
                }
            }
        } else if let Some(place) = field_login {
            if let (false, Some(before)) = (pressed_before, spot(&state, "before")) {
                pressed_before = true;
                page.click(before.0, before.1).await?;
                continue;
            }
            match login_sent.as_mut() {
                None => {
                    fill(page, place, &site.login).await?;
                    let state = page.evaluate(&script).await?;
                    let sent = Sent::new(&state);
                    submit(page, &state).await?;
                    login_sent = Some(sent);
                }
                Some(sent) => {
                    let found =
                        watch(page, sent, &state, place, Outcome::WrongCredentials, debug).await?;
                    if let Some(outcome) = found {
                        return Ok(outcome);
                    }
                }
            }
        }
    }
    Ok(Outcome::NoForm)
}

/// The pause holds until it runs out or the owner saves the sites file.
fn is_paused(pause: &Path, sites_file: &Path) -> bool {
    let modified = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    };
    let Some(since) = modified(pause) else {
        return false;
    };
    let expired = since.elapsed().map_or(true, |age| age >= PAUSE);
    let edited = modified(sites_file).is_some_and(|edit| edit > since);
    !expired && !edited
}

struct Server {
    sites_file: PathBuf,
    profile: PathBuf,
    port: u16,
    allowed: Vec<String>,
}

impl Server {
    fn sites(&self) -> Result<Sites> {
        Sites::load(&self.sites_file)
    }

    async fn browser(&self) -> Result<Browser> {
        let sites = self.sites()?;
        let chrome = find_chrome(sites.chrome.as_deref())
            .with_context(|| format!("Chrome не найден, укажи chrome в {FILE}"))?;
        Browser::start(&chrome, &self.profile, self.port, !sites.show_browser).await
    }

    fn tools(&self) -> Value {
        json!([{
            "name": "site_login",
            "description": "Войти на сайт под учётной записью отдела. Логин и пароль подставляет сам инструмент, ты их не видишь и знать не должен. Вызывай первым делом, а потом работай в браузере инструментами browser.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "site": {"type": "string", "enum": self.allowed},
                    "code": {"type": "string", "description": "Код подтверждения от хозяина, если сайт его просил."}
                },
                "required": ["site"]
            }
        }])
    }

    async fn site_login(&mut self, args: &Value) -> Result<String> {
        let name = args
            .get("site")
            .and_then(Value::as_str)
            .context("missing site")?;
        if !self.allowed.iter().any(|allowed| allowed == name) {
            bail!("сайт {name} закрыт для этого отдела");
        }
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|code| !code.is_empty());
        let sites = self.sites()?;
        let site = sites
            .site
            .get(name)
            .with_context(|| format!("сайта {name} нет в {FILE}: его должен добавить хозяин"))?;
        if !site.filled() {
            bail!("у сайта {name} не заполнены логин и пароль: сообщи хозяину через questions.md");
        }
        let pause = self.profile.join(format!("paused-{name}"));
        if is_paused(&pause, &self.sites_file) {
            bail!(
                "вход на {name} на паузе после отказа сайта, чтобы не заблокировать учётную запись: не входи снова. Сообщи хозяину об этом в result.md и закончи работу."
            );
        }
        let mut browser = self.browser().await?;
        let mut page = browser.attach_page().await?;
        let debug = self.profile.join(DEBUG_FILE);
        let outcome = login(&mut page, site, code, Some(&debug)).await?;
        note(&mut page, Some(&debug), outcome.label()).await;
        page.detach().await;
        if outcome.pauses() {
            std::fs::write(&pause, "")?;
        }
        let waiting = self.profile.join(WAITING_FILE);
        if matches!(outcome, Outcome::CodeRequired | Outcome::CodeResent) {
            std::fs::write(waiting, "")?;
        } else {
            let _ = std::fs::remove_file(waiting);
        }
        Ok(outcome.message(name))
    }

    async fn handle(&mut self, request: Value) -> Option<Value> {
        let id = request.get("id")?.clone();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match request.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-03-26"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "site", "version": env!("CARGO_PKG_VERSION")}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": self.tools()}),
            "tools/call" => {
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let reply = match params.get("name").and_then(Value::as_str) {
                    Some("site_login") => self.site_login(&args).await,
                    _ => Err(anyhow::anyhow!("unknown tool")),
                };
                match reply {
                    Ok(text) => {
                        json!({"content": [{"type": "text", "text": text}], "isError": false})
                    }
                    Err(error) => {
                        json!({"content": [{"type": "text", "text": format!("{error:#}")}], "isError": true})
                    }
                }
            }
            _ => {
                return Some(
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}),
                );
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

fn parse_args(args: &[String]) -> Option<Server> {
    let mut server = Server {
        sites_file: PathBuf::new(),
        profile: PathBuf::new(),
        port: 0,
        allowed: Vec::new(),
    };
    for pair in args.chunks(2) {
        let [key, value] = pair else { return None };
        match key.as_str() {
            "--sites" => server.sites_file = value.into(),
            "--profile" => server.profile = value.into(),
            "--port" => server.port = value.parse().ok()?,
            "--allow" => {
                server.allowed = value.split(',').map(str::to_owned).collect();
            }
            _ => return None,
        }
    }
    (server.port != 0 && !server.allowed.is_empty()).then_some(server)
}

pub const USAGE: &str =
    "Usage: agentsflow site-mcp --sites FILE --profile DIR --port N --allow a,b";

/// The MCP server of the login tool. It starts Chrome at once, because the
/// agent's browser tools connect to the same Chrome and need it running.
pub async fn run(args: &[String]) -> Result<()> {
    let Some(mut server) = parse_args(args) else {
        bail!("{USAGE}");
    };
    let started = server.browser().await;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = server.handle(request).await {
            let mut line = serde_json::to_vec(&response)?;
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
        }
    }
    // Usually the agent's CLI kills this process and `after_run` closes Chrome;
    // this is for a quiet end of the session.
    if !server.profile.join(WAITING_FILE).is_file() {
        match started {
            Ok(browser) => browser.close().await,
            Err(_) => {
                if let Ok(browser) = Browser::connect(server.port).await {
                    browser.close().await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    const LOGIN_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>login</title>
<div id=one><input id=user placeholder=Email><button id=next>Продолжить</button></div>
<div id=two style="display:none"><input id=pass type=password><button id=enter>Войти</button></div>
<div id=three style="display:none"><input id=the_code name=code><button id=check>Подтвердить</button></div>
<div id=four style="display:none"><input class=c name=code1 maxlength=1><input class=c name=code2 maxlength=1><input class=c name=code3 maxlength=1><input class=c name=code4 maxlength=1><button id=cellsok>Подтвердить</button></div>
<script>
var seen = {};
function toast(t){ var d = document.createElement('div'); d.textContent = t; document.body.appendChild(d); setTimeout(function(){ d.remove(); }, 1500); }
next.onclick = function(){ seen.user = user.value; one.style.display='none'; two.style.display='block'; };
enter.onclick = function(){
  if (seen.user === 'owner@test.local' && pass.value === 'test-secret-1') {
    if (location.search.indexOf('cells') >= 0) { two.style.display='none'; four.style.display='block'; }
    else if (location.search.indexOf('code') >= 0) { two.style.display='none'; three.style.display='block'; }
    else { location.href = '/app'; }
  } else { toast('Неверный пароль'); }
};
check.onclick = function(){ if (the_code.value === '4242') location.href = '/app'; else toast('Неверный код'); };
cellsok.onclick = function(){ var v=''; document.querySelectorAll('.c').forEach(function(c){v+=c.value}); if (v === '4242') location.href = '/app'; else toast('Неверный код'); };
</script>"#;

    /// Shaped like the Kaspi form: a hidden tab with a clipped twin of the
    /// button comes first, and "remember" sits next to the code.
    const PANELS_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>login</title>
<style>#card{margin-left:300px;width:300px;height:200px;overflow:hidden;position:relative}#tab{position:absolute;left:-300px;top:0;width:300px}.box{display:inline-block;width:20px;height:20px;background:#ccc}</style>
<div id=card>
<div id=tab><input name=username><div><label><input type=checkbox style="position:absolute;opacity:0;width:1px;height:1px"><i class=box></i></label><span>Запомнить вход с этого устройства</span></div><button id=twin style="width:100%">Продолжить</button></div>
<div><input id=the_code name=security-code maxlength=6>
<div><label><input type=checkbox id=keep style="position:absolute;opacity:0;width:1px;height:1px"><i class=box></i></label><span>Запомнить вход с этого устройства</span></div>
<div id=late></div><button id=go type=button>Продолжить</button><button id=again type=button style="display:none">Отправить новый код</button></div>
</div>
<script>
var mode = location.search;
var fresh = false;
again.onclick = function(){ fresh = true; again.style.display = 'none'; late.textContent = ''; };
go.onclick = function(){
  if (mode === '?late' && !fresh) { late.textContent = 'Срок действия кода истек'; if (location.hash !== '#wait') again.style.display = 'block'; return; }
  if ((mode === '' || mode === '?late') && the_code.value === '424242' && keep.checked) location.href = '/app';
};
the_code.onkeydown = function(e){ if (mode === '?enter' && e.key === 'Enter') location.href = '/app'; };
</script>"#;

    async fn serve_pages() -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = vec![0; 4096];
                    let size = stream.read(&mut buffer).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buffer[..size]).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let body = if path.starts_with("/app") {
                        // A search box must not be taken for a login form.
                        "<!doctype html><title>app</title><input type=text id=search>".to_owned()
                    } else if path.starts_with("/login-panels") {
                        PANELS_PAGE.to_owned()
                    } else {
                        LOGIN_PAGE.to_owned()
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        port
    }

    fn site(url: String, password: &str) -> Site {
        Site {
            url,
            login_url: "/login".into(),
            login: "owner@test.local".into(),
            password: password.into(),
            before_login: None,
            login_selector: None,
            password_selector: None,
            code_selector: None,
            submit_selector: None,
            remember_selector: None,
            captcha_selector: None,
        }
    }

    #[test]
    fn a_broken_file_does_not_show_its_lines() {
        let text = "[site.kaspi]\nurl = \"https://x\"\nlogin_url = \"x\"\nlogin = \"a\"\npassword = \"p4ss\nword\"\n";
        let error = Sites::parse(text).err().unwrap().to_string();
        assert!(!error.contains("p4ss"), "{error}");
        assert!(error.contains("строке"), "{error}");
        let unknown = "[site.kaspi]\nurl = \"https://x\"\nlogin_url = \"x\"\nlogin = \"a\"\npassword = \"p4ss\"\npasswd = \"p4ss\"\n";
        let error = Sites::parse(unknown).err().unwrap().to_string();
        assert!(!error.contains("p4ss"), "{error}");
    }

    #[test]
    fn a_blank_site_does_not_block_the_others() {
        let text = "[site.kaspi]\nurl = \"https://x\"\nlogin_url = \"x\"\nlogin = ''\npassword = ''\n\
                    [site.onec]\nurl = \"https://y\"\nlogin_url = \"y\"\nlogin = 'a'\npassword = 'b'\n";
        let sites = Sites::parse(text).unwrap();
        assert!(!sites.site["kaspi"].filled());
        assert!(sites.site["onec"].filled());
    }

    #[test]
    fn a_pause_holds_until_the_owner_saves_the_file() {
        let dir = std::env::temp_dir().join(format!("af-pause-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (pause, sites) = (dir.join("paused-kaspi"), dir.join(FILE));
        std::fs::write(&sites, "").unwrap();
        assert!(!is_paused(&pause, &sites));
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&pause, "").unwrap();
        assert!(is_paused(&pause, &sites));
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&sites, "edited").unwrap();
        assert!(!is_paused(&pause, &sites));
    }

    #[test]
    fn ports_are_stable_and_in_range() {
        assert_eq!(debug_port("shop"), debug_port("shop"));
        assert!((19300..19800).contains(&debug_port("shop")));
    }

    #[test]
    fn missing_file_is_a_problem_only_for_departments_with_sites() {
        let dir = std::env::temp_dir().join(format!("af-sites-{}", uuid::Uuid::new_v4()));
        assert!(problems(&dir, "shop", &[]).is_empty());
        let found = problems(&dir, "shop", &["kaspi".into()]);
        assert!(found[0].contains("нет файла"), "{found:?}");
    }

    /// A real Chrome against a local fake site; skipped where there is no Chrome.
    #[tokio::test]
    async fn logs_in_step_by_step_and_stops_where_the_owner_is_needed() {
        let Some(chrome) = find_chrome(None) else {
            return;
        };
        let port = serve_pages().await;
        let base = format!("http://127.0.0.1:{port}");
        let profile = std::env::temp_dir().join(format!("af-chrome-{}", uuid::Uuid::new_v4()));
        let debug = 19850 + (port % 100);
        let mut browser = Browser::start(&chrome, &profile, debug, true)
            .await
            .unwrap();
        let mut page = browser.attach_page().await.unwrap();

        let right = site(format!("{base}/login"), "test-secret-1");
        assert_eq!(
            login(&mut page, &right, None, None).await.unwrap(),
            Outcome::LoggedIn
        );
        let state = page.evaluate(&probe_script(&right)).await.unwrap();
        assert!(state["url"].as_str().unwrap().ends_with("/app"));

        let wrong = site(format!("{base}/login"), "wrong");
        assert_eq!(
            login(&mut page, &wrong, None, None).await.unwrap(),
            Outcome::WrongCredentials
        );

        let coded = site(format!("{base}/login?code"), "test-secret-1");
        assert_eq!(
            login(&mut page, &coded, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        // The page keeps waiting: the code from the owner goes into the same form.
        assert_eq!(
            login(&mut page, &coded, Some("4242"), None).await.unwrap(),
            Outcome::LoggedIn
        );

        let cells = site(format!("{base}/login?cells"), "test-secret-1");
        assert_eq!(
            login(&mut page, &cells, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        assert_eq!(
            login(&mut page, &cells, Some("4242"), None).await.unwrap(),
            Outcome::LoggedIn
        );
        // A rejected code is reported, and the snapshot holds no field values.
        let log = profile.join("debug.log");
        assert_eq!(
            login(&mut page, &cells, None, Some(&log)).await.unwrap(),
            Outcome::CodeRequired
        );
        assert_eq!(
            login(&mut page, &cells, Some("1111"), Some(&log))
                .await
                .unwrap(),
            Outcome::WrongCode
        );
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("code_typed"), "{written}");
        assert!(!written.contains("1111") && !written.contains("test-secret-1"));

        // The hidden twin is not pressed, and "remember" is ticked on the way.
        page.navigate(&format!("{base}/app")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let panels = site(format!("{base}/login-panels"), "test-secret-1");
        assert_eq!(
            login(&mut page, &panels, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        // Asking again keeps the same form: no new code goes out.
        page.evaluate("window.mark = 1; '1'").await.unwrap();
        assert_eq!(
            login(&mut page, &panels, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        let mark = page.evaluate("String(window.mark || 0)").await.unwrap();
        assert_eq!(mark, json!(1));
        assert_eq!(
            login(&mut page, &panels, Some("424242"), None)
                .await
                .unwrap(),
            Outcome::LoggedIn
        );

        // A button that does nothing: Enter gets its try.
        let enter = site(format!("{base}/login-panels?enter"), "test-secret-1");
        assert_eq!(
            login(&mut page, &enter, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        assert_eq!(
            login(&mut page, &enter, Some("424242"), None)
                .await
                .unwrap(),
            Outcome::LoggedIn
        );

        // A late code is not a refusal: no pause, and the form gets a new one.
        let late = site(format!("{base}/login-panels?late"), "test-secret-1");
        assert_eq!(
            login(&mut page, &late, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        let resent = login(&mut page, &late, Some("424242"), None).await.unwrap();
        assert_eq!(resent, Outcome::CodeResent);
        assert!(!resent.pauses());
        assert_eq!(
            login(&mut page, &late, Some("424242"), None).await.unwrap(),
            Outcome::LoggedIn
        );
        // The site does not offer a new code yet.
        let waits = site(format!("{base}/login-panels?late#wait"), "test-secret-1");
        assert_eq!(
            login(&mut page, &waits, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        let expired = login(&mut page, &waits, Some("424242"), None)
            .await
            .unwrap();
        assert_eq!(expired, Outcome::CodeExpired);
        assert!(!expired.pauses());

        // Silence is not a refusal, and does not pause anything.
        let dead = site(format!("{base}/login-panels?dead"), "test-secret-1");
        assert_eq!(
            login(&mut page, &dead, None, None).await.unwrap(),
            Outcome::CodeRequired
        );
        let silent = login(&mut page, &dead, Some("424242"), None).await.unwrap();
        assert_eq!(silent, Outcome::NoReaction);
        assert!(!silent.pauses());

        page.detach().await;
        browser.close().await;
    }
}

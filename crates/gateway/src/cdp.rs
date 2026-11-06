//! A small client of the Chrome DevTools Protocol: just enough to start Chrome
//! and fill a login form. Secrets go only through `Input.insertText`, never
//! into a script, and the page never reports them back.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(20);

pub fn find_chrome(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(path.to_path_buf());
    }
    let mut known: Vec<PathBuf> = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        known.push(PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe"));
    }
    known.into_iter().find(|path| path.is_file())
}

/// Asks Chrome on `port` for the address of its browser socket.
async fn browser_socket(port: u16) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let request = format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    // Chrome may keep the connection open: read up to the announced length.
    let mut reply = Vec::new();
    let mut chunk = [0u8; 4096];
    let body = loop {
        let size = stream.read(&mut chunk).await?;
        if size == 0 {
            bail!("Chrome closed the connection");
        }
        reply.extend_from_slice(&chunk[..size]);
        let Some(split) = reply.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&reply[..split]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .context("no length in the answer from Chrome")?
            .trim()
            .parse()?;
        if reply.len() >= split + 4 + length {
            break &reply[split + 4..split + 4 + length];
        }
    };
    let version: Value = serde_json::from_slice(body)?;
    let url = version["webSocketDebuggerUrl"]
        .as_str()
        .context("no browser socket")?;
    if !url.starts_with("ws://127.0.0.1:") {
        bail!("unexpected browser socket");
    }
    Ok(url.to_owned())
}

pub struct Browser {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Browser {
    /// Connects to the Chrome on this port, if it is alive.
    pub async fn connect(port: u16) -> Result<Self> {
        let attempt = async {
            let url = browser_socket(port).await?;
            let (socket, _) = connect_async(url).await?;
            anyhow::Ok(socket)
        };
        let socket = tokio::time::timeout(Duration::from_secs(5), attempt)
            .await
            .context("Chrome does not answer")??;
        Ok(Self { socket, next_id: 0 })
    }

    /// Connects to the running Chrome of this profile or starts a new one.
    pub async fn start(chrome: &Path, profile: &Path, port: u16, headless: bool) -> Result<Self> {
        if let Ok(browser) = Self::connect(port).await {
            return Ok(browser);
        }
        std::fs::create_dir_all(profile)?;
        let mut command = tokio::process::Command::new(chrome);
        command
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--disable-background-networking",
                "--disable-features=PasswordLeakDetection,PasswordManagerOnboarding",
            ]);
        if headless {
            command.args(["--headless=new", "--window-size=1400,1000"]);
        }
        command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        command
            .spawn()
            .with_context(|| format!("cannot start {}", chrome.display()))?;
        let started = std::time::Instant::now();
        loop {
            match Self::connect(port).await {
                Ok(browser) => return Ok(browser),
                Err(error) if started.elapsed() > START_TIMEOUT => {
                    bail!("Chrome did not start in {START_TIMEOUT:?}: {error:#}");
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }

    pub async fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await?;
        let wait = async {
            while let Some(message) = self.socket.next().await {
                let Message::Text(text) = message? else {
                    continue;
                };
                let reply: Value = serde_json::from_str(text.as_str())?;
                if reply.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = reply.get("error") {
                    // Only the method and Chrome's own message: the parameters may hold a secret.
                    bail!(
                        "{method}: {}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                    );
                }
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
            bail!("Chrome closed the connection")
        };
        tokio::time::timeout(CALL_TIMEOUT, wait)
            .await
            .with_context(|| format!("{method}: no answer from Chrome"))?
    }

    /// Attaches to the first ordinary tab: the one the agent's browser tools see too.
    pub async fn attach_page(&mut self) -> Result<Page<'_>> {
        let targets = self.call(None, "Target.getTargets", json!({})).await?;
        let existing = targets["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|target| target["type"] == "page")
            .and_then(|target| target["targetId"].as_str())
            .map(str::to_owned);
        let target = match existing {
            Some(target) => target,
            None => self
                .call(None, "Target.createTarget", json!({"url": "about:blank"}))
                .await?["targetId"]
                .as_str()
                .context("no target id")?
                .to_owned(),
        };
        let attached = self
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .context("no session id")?
            .to_owned();
        Ok(Page {
            browser: self,
            session,
        })
    }

    /// Asks Chrome to close itself, so it saves the session cookies.
    pub async fn close(mut self) {
        let _ = self.call(None, "Browser.close", json!({})).await;
    }
}

pub struct Page<'a> {
    browser: &'a mut Browser,
    session: String,
}

impl Page<'_> {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let session = self.session.clone();
        self.browser.call(Some(&session), method, params).await
    }

    pub async fn navigate(&mut self, url: &str) -> Result<()> {
        self.call("Page.navigate", json!({"url": url})).await?;
        Ok(())
    }

    /// Runs a script that returns a JSON string and parses it.
    pub async fn evaluate(&mut self, script: &str) -> Result<Value> {
        let reply = self
            .call(
                "Runtime.evaluate",
                json!({"expression": script, "returnByValue": true}),
            )
            .await?;
        if reply.get("exceptionDetails").is_some() {
            bail!("page script failed");
        }
        let text = reply["result"]["value"]
            .as_str()
            .context("page script returned no text")?;
        Ok(serde_json::from_str(text)?)
    }

    /// A real mouse click: pages ignore synthetic ones more often.
    pub async fn click(&mut self, x: f64, y: f64) -> Result<()> {
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Replaces the value of the focused field.
    pub async fn type_text(&mut self, text: &str) -> Result<()> {
        self.evaluate(
            "(function(){var e=document.activeElement;if(e&&e.select)e.select();return '{}'})()",
        )
        .await?;
        self.call("Input.insertText", json!({"text": text})).await?;
        Ok(())
    }

    /// Selects the content of the focused field, so the next input replaces it.
    pub async fn clear_field(&mut self) -> Result<()> {
        self.evaluate(
            "(function(){var e=document.activeElement;if(e&&e.select)e.select();return '{}'})()",
        )
        .await?;
        Ok(())
    }

    /// Types key by key into the focused field.
    pub async fn type_keys(&mut self, text: &str) -> Result<()> {
        for key in text.chars() {
            let key = key.to_string();
            self.call(
                "Input.dispatchKeyEvent",
                json!({"type": "keyDown", "key": key, "text": key}),
            )
            .await?;
            self.call(
                "Input.dispatchKeyEvent",
                json!({"type": "keyUp", "key": key}),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        Ok(())
    }

    pub async fn press_enter(&mut self) -> Result<()> {
        for kind in ["keyDown", "keyUp"] {
            self.call(
                "Input.dispatchKeyEvent",
                json!({"type": kind, "key": "Enter", "code": "Enter",
                       "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13, "text": "\r"}),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn detach(self) {
        let session = self.session.clone();
        let _ = self
            .browser
            .call(
                None,
                "Target.detachFromTarget",
                json!({"sessionId": session}),
            )
            .await;
    }
}

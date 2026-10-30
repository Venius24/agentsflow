use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::future::Future;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn tools() -> Value {
    let peers =
        std::env::var("AGENTSFLOW_PEERS").unwrap_or_else(|_| "claude,codex,orchestrator".into());
    tools_for(&peers)
}
fn tools_for(peers: &str) -> Value {
    let peers: Vec<_> = peers.split(',').filter(|s| !s.is_empty()).collect();
    json!([
        {"name":"send_mail", "description":format!("Send a request or reply to a team member. Recipients: {}. Set thread to reply in the same conversation.", peers.join(", ")), "inputSchema":{"type":"object","properties":{"to":{"type":"string","enum":peers},"subject":{"type":"string"},"body":{"type":"string"},"thread":{"type":"string","description":"Thread ID for a reply in the same conversation."}},"required":["to","subject","body"]}},
        {"name":"check_inbox", "description":"Check your inbox for unread requests and replies from other agents and mark them read. Reply with send_mail using the same thread when appropriate.","inputSchema":{"type":"object","properties":{}}}
    ])
}
fn text_result(text: String, error: bool) -> Value {
    json!({"content":[{"type":"text","text":text}],"isError":error})
}

async fn tool_call<F, Fut>(me: &str, params: &Value, send: &F) -> Result<String>
where
    F: Fn(&str, Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let args = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match params.get("name").and_then(Value::as_str).unwrap_or("") {
        "send_mail" => {
            let get = |key: &str| {
                args.get(key)
                    .and_then(Value::as_str)
                    .with_context(|| format!("missing {key}"))
            };
            let to = get("to")?;
            let mut body = json!({"from":me,"to":to,"subject":get("subject")?,"body":get("body")?});
            if let Some(thread) = args.get("thread") {
                body["thread"] = Value::String(
                    thread
                        .as_str()
                        .context("thread must be a string")?
                        .to_owned(),
                );
            }
            let result = send("/api/mail/send", body).await?;
            Ok(format!(
                "sent {} to {} (thread {})",
                result["id"].as_str().context("missing id")?,
                to,
                result["thread"].as_str().context("missing thread")?
            ))
        }
        "check_inbox" => {
            let response = send("/api/mail/inbox", json!({"agent":me})).await?;
            let messages = response["messages"]
                .as_array()
                .context("missing messages")?;
            if messages.is_empty() {
                return Ok("inbox empty".into());
            }
            messages
                .iter()
                .map(|message| {
                    let field = |name: &str| {
                        message[name]
                            .as_str()
                            .with_context(|| format!("missing {name}"))
                    };
                    Ok(format!(
                        "[{}] thread={} from={} subject={}\n{}",
                        field("id")?,
                        field("thread")?,
                        field("from")?,
                        field("subject")?,
                        field("body")?
                    ))
                })
                .collect::<Result<Vec<_>>>()
                .map(|lines| lines.join("\n\n"))
        }
        name => bail!("unknown tool: {name}"),
    }
}

async fn handle<F, Fut>(request: Value, me: &str, send: &F) -> Option<Value>
where
    F: Fn(&str, Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let id = request.get("id")?.clone();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-03-26"),"capabilities":{"tools":{}},"serverInfo":{"name":"mail","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => match tool_call(me, &params, send).await {
            Ok(text) => text_result(text, false),
            Err(error) => text_result(format!("{error:#}"), true),
        },
        _ => {
            return Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}}),
            );
        }
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn port(url: &str) -> Result<u16> {
    let value = url
        .strip_prefix("http://127.0.0.1:")
        .context("AGENTSFLOW_URL must use loopback HTTP")?;
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        bail!("invalid loopback port");
    }
    let port: u16 = value.parse()?;
    if port == 0 {
        bail!("invalid loopback port");
    }
    Ok(port)
}
async fn post(url: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    let port = port(url)?;
    if token.contains(['\r', '\n']) {
        bail!("invalid token");
    }
    let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let body = serde_json::to_vec(&body)?;
    let headers = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-agentsflow-token: {token}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&body).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("invalid HTTP response")?;
    let header = std::str::from_utf8(&response[..split])?;
    let status: u16 = header
        .lines()
        .next()
        .context("missing status")?
        .split_whitespace()
        .nth(1)
        .context("invalid status")?
        .parse()?;
    let mut body = &response[split + 4..];
    if let Some(length) = header.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("content-length")
            .then_some(value.trim())
    }) {
        let length: usize = length.parse()?;
        if body.len() < length {
            bail!("truncated HTTP response");
        }
        body = &body[..length];
    }
    let json: Value = serde_json::from_slice(body).context("invalid gateway response")?;
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            json.get("error")
                .and_then(Value::as_str)
                .unwrap_or("gateway request failed")
        );
    }
    Ok(json)
}

pub async fn run(agent: &str) -> Result<()> {
    let url = std::env::var("AGENTSFLOW_URL").context("AGENTSFLOW_URL is missing")?;
    let token = std::env::var("AGENTSFLOW_TOKEN").context("AGENTSFLOW_TOKEN is missing")?;
    port(&url)?;
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                stdout.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"parse error\"}}\n").await?;
                stdout.flush().await?;
                continue;
            }
        };
        let send = |path: &str, body: Value| {
            let path = path.to_owned();
            let url = url.clone();
            let token = token.clone();
            async move { post(&url, &token, &path, body).await }
        };
        if let Some(response) = handle(request, agent, &send).await {
            let mut line = serde_json::to_vec(&response)?;
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn tools_list_configured_peers() {
        let tools = tools_for("lead,main,junior,orchestrator");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["to"]["enum"],
            json!(["lead", "main", "junior", "orchestrator"])
        );
    }

    #[tokio::test]
    async fn protocol_and_tools() {
        let send = |path: &str, body: Value| {
            let path = path.to_owned();
            async move {
                match path.as_str() {
                    "/api/mail/send" if body["to"] == "codex" => {
                        Ok(json!({"id":"abc","thread":"thread"}))
                    }
                    "/api/mail/send" => bail!("gateway refused"),
                    _ => Ok(
                        json!({"messages":[{"id":"one","thread":"t","from":"codex","subject":"s","body":"hello"}]}),
                    ),
                }
            }
        };
        let init = handle(
            json!({"id":1,"method":"initialize","params":{"protocolVersion":"v"}}),
            "claude",
            &send,
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "v");
        assert_eq!(
            handle(json!({"id":2,"method":"tools/list"}), "claude", &send)
                .await
                .unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            handle(
                json!({"method":"notifications/initialized"}),
                "claude",
                &send
            )
            .await
            .is_none()
        );
        assert_eq!(
            handle(json!({"id":3,"method":"unknown"}), "claude", &send)
                .await
                .unwrap()["error"]["code"],
            -32601
        );
        let call = |name: &str, args: Value| json!({"id":4,"method":"tools/call","params":{"name":name,"arguments":args}});
        let sent = handle(
            call("send_mail", json!({"to":"codex","subject":"s","body":"b"})),
            "claude",
            &send,
        )
        .await
        .unwrap();
        assert_eq!(
            sent["result"]["content"][0]["text"],
            "sent abc to codex (thread thread)"
        );
        let inbox = handle(call("check_inbox", json!({})), "claude", &send)
            .await
            .unwrap();
        assert_eq!(
            inbox["result"]["content"][0]["text"],
            "[one] thread=t from=codex subject=s\nhello"
        );
        let error = handle(
            call("send_mail", json!({"to":"claude","subject":"s","body":"b"})),
            "claude",
            &send,
        )
        .await
        .unwrap();
        assert_eq!(error["result"]["isError"], true);
        assert!(
            error["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("gateway refused")
        );
        let empty = |_: &str, _: Value| async { Ok(json!({"messages":[]})) };
        assert_eq!(
            handle(call("check_inbox", json!({})), "claude", &empty)
                .await
                .unwrap()["result"]["content"][0]["text"],
            "inbox empty"
        );
    }

    #[tokio::test]
    async fn local_http_client() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(request.contains("x-agentsflow-token: secret\r\n"));
            assert!(request.contains(&format!("Host: 127.0.0.1:{port}\r\n")));
            let length: usize = request
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["agent"],
                "claude"
            );
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"messages\":[]}").await.unwrap();
        });
        assert_eq!(
            post(
                &format!("http://127.0.0.1:{port}"),
                "secret",
                "/api/mail/inbox",
                json!({"agent":"claude"})
            )
            .await
            .unwrap(),
            json!({"messages":[]})
        );
        server.await.unwrap();
        assert!(super::port("http://localhost:123").is_err());
    }
}

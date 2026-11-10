use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};

pub async fn run() -> Result<()> {
    let mut sessions = HashSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(request) => handle(&mut out, &mut sessions, &request).await?,
            Err(error) => eprintln!("stub-agent input error: {error}"),
        }
    }
    Ok(())
}

async fn handle(out: &mut Stdout, sessions: &mut HashSet<String>, request: &Value) -> Result<()> {
    let id = request.get("id");
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match method {
        "initialize" => {
            if let Some(id) = id {
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "protocolVersion": 1, "agentCapabilities": {} },
                    }),
                )
                .await?;
            }
        }
        "session/new" => {
            let session_id = uuid_v4();
            sessions.insert(session_id.clone());
            if let Some(id) = id {
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "sessionId": session_id },
                    }),
                )
                .await?;
            }
        }
        "session/prompt" => {
            let params = request.get("params");
            let session_id = params
                .and_then(|params| params.get("sessionId"))
                .and_then(Value::as_str);
            match session_id {
                Some(session_id) if sessions.contains(session_id) => {
                    let text = prompt_text(params);
                    let reply = if std::env::var_os("STUB_REPORT_CWD").is_some() {
                        let cwd = std::env::current_dir()
                            .map(|path| path.display().to_string())
                            .unwrap_or_default();
                        format!("STUB:cwd={cwd}\n{}", first_chars(&text, 2000))
                    } else {
                        format!("STUB:{}", first_chars(&text, 2000))
                    };
                    let split = reply.chars().count().div_ceil(2);
                    let (first, second) = split_chars(&reply, split);
                    chunk(out, session_id, &first).await?;
                    chunk(out, session_id, &second).await?;
                    if let Some(id) = id {
                        send(
                            out,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "stopReason": "end_turn" },
                            }),
                        )
                        .await?;
                    }
                }
                _ => {
                    if let Some(id) = id {
                        send(
                            out,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32602, "message": "unknown sessionId" },
                            }),
                        )
                        .await?;
                    }
                }
            }
        }
        _ => {
            if let Some(id) = id {
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("method not found: {method}"),
                        },
                    }),
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn prompt_text(params: Option<&Value>) -> String {
    let Some(parts) = params
        .and_then(|params| params.get("prompt"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    parts
        .iter()
        .map(|part| {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                part.get("text").and_then(Value::as_str).unwrap_or_default()
            } else {
                ""
            }
        })
        .collect()
}

async fn chunk(out: &mut Stdout, session_id: &str, text: &str) -> Result<()> {
    send(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text },
                },
            },
        }),
    )
    .await
}

async fn send(out: &mut Stdout, payload: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(payload)?;
    line.push(b'\n');
    out.write_all(&line).await?;
    out.flush().await?;
    Ok(())
}

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !read {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        bytes[..16].copy_from_slice(&nanos.to_le_bytes()[..16]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

fn split_chars(text: &str, at: usize) -> (String, String) {
    let chars: Vec<char> = text.chars().collect();
    let split = at.min(chars.len());
    (
        chars[..split].iter().collect(),
        chars[split..].iter().collect(),
    )
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

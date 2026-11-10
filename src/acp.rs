use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};

use crate::config::AgentSpec;

struct Pending {
    method: String,
    sender: oneshot::Sender<std::result::Result<Value, String>>,
}

#[derive(Debug, Default)]
pub struct ToolCallEvidence {
    pub kind: String,
    pub status: String,
    pub title: String,
    pub url: Option<String>,
}

#[derive(Debug, Default)]
pub struct PromptResult {
    pub text: String,
    pub tool_calls: HashMap<String, ToolCallEvidence>,
}

impl PromptResult {
    pub fn fetched_url(&self, url: &str) -> bool {
        self.tool_calls.values().any(|call| {
            call.kind == "fetch"
                && call.status == "completed"
                && (call.url.as_deref() == Some(url)
                    || call.title.strip_prefix("Fetch ") == Some(url))
        })
    }
}

#[derive(Default)]
struct Inner {
    pending: HashMap<u64, Pending>,
    collectors: HashMap<String, PromptResult>,
    stderr_tail: String,
}

type Stdin = Arc<AsyncMutex<Option<ChildStdin>>>;

pub struct AcpClient {
    name: String,
    cwd: String,
    inner: Arc<Mutex<Inner>>,
    stdin: Stdin,
    exited: Arc<AtomicBool>,
    reaped: watch::Receiver<bool>,
    close_lock: AsyncMutex<bool>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    next_id: AtomicU64,
    pid: u32,
    session_meta: Option<Value>,
    model: Option<String>,
    mode: Option<String>,
}

impl AcpClient {
    pub async fn spawn(name: &str, spec: &AgentSpec, cwd: &str) -> Result<Self> {
        let (program, args) = spec
            .command
            .split_first()
            .ok_or_else(|| anyhow!("ACP agent \"{name}\" has an empty command"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        command.kill_on_drop(true);
        for (key, value) in &spec.env {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        let mut child = command.spawn().map_err(|error| {
            anyhow!("ACP agent \"{name}\" (cwd {cwd}) could not start: {error}")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ACP agent \"{name}\" (cwd {cwd}) has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ACP agent \"{name}\" (cwd {cwd}) has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("ACP agent \"{name}\" (cwd {cwd}) has no stderr"))?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("spawned ACP agent has no process id"))?;

        let inner = Arc::new(Mutex::new(Inner::default()));
        let stdin = Arc::new(AsyncMutex::new(Some(stdin)));
        let exited = Arc::new(AtomicBool::new(false));
        let (reaped_sender, reaped) = watch::channel(false);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();

        let reader_inner = inner.clone();
        let reader_stdin = stdin.clone();
        let reader_name = name.to_string();
        let reader_cwd = cwd.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                handle_line(
                    &reader_name,
                    &reader_cwd,
                    &reader_inner,
                    &reader_stdin,
                    line,
                )
                .await;
            }
        });

        let stderr_inner = inner.clone();
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        let chunk = String::from_utf8_lossy(&buffer[..read]);
                        let mut inner = stderr_inner.lock().expect("acp state mutex");
                        inner.stderr_tail.push_str(&chunk);
                        truncate_tail(&mut inner.stderr_tail);
                    }
                    Err(_) => break,
                }
            }
        });

        let waiter_inner = inner.clone();
        let waiter_exited = exited.clone();
        let waiter_name = name.to_string();
        let waiter_cwd = cwd.to_string();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.map_err(anyhow::Error::from),
                _ = shutdown_receiver => async {
                    signal_group(pid, libc::SIGTERM)?;
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(2), child.wait()
                    ).await {
                        Ok(status) => status.map_err(anyhow::Error::from),
                        Err(_) => {
                            signal_group(pid, libc::SIGKILL)?;
                            child.wait().await.map_err(anyhow::Error::from)
                        }
                    }
                }.await,
            };
            let _ = reaped_sender.send(status.is_ok());
            waiter_exited.store(true, Ordering::SeqCst);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), stderr_task).await;
            let tail = {
                let inner = waiter_inner.lock().expect("acp state mutex");
                last_chars(inner.stderr_tail.trim(), 800)
            };
            let how = match &status {
                Ok(status) => exit_description(Some(status)),
                Err(error) => format!("shutdown/wait failed: {error}"),
            };
            let detail = if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            };
            fail_all(
                &waiter_inner,
                format!("ACP agent \"{waiter_name}\" (cwd {waiter_cwd}) exited ({how}){detail}"),
            );
        });

        Ok(Self {
            name: name.to_string(),
            cwd: cwd.to_string(),
            inner,
            stdin,
            exited,
            reaped,
            close_lock: AsyncMutex::new(false),
            shutdown: Mutex::new(Some(shutdown_sender)),
            next_id: AtomicU64::new(1),
            pid,
            session_meta: spec.session_meta.clone(),
            model: spec.model.clone(),
            mode: spec.mode.clone(),
        })
    }

    pub async fn initialize(&self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": { "name": "mattermost-acp-assist", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn new_session(&self, cwd: &str) -> Result<String> {
        let mut params = json!({ "cwd": cwd, "mcpServers": [] });
        if let Some(meta) = &self.session_meta {
            params["_meta"] = meta.clone();
        }
        let result = self.request("session/new", params).await?;
        match result.get("sessionId").and_then(Value::as_str) {
            Some(session_id) if !session_id.is_empty() => {
                if let Some(mode) = &self.mode {
                    require_advertised_mode(&result, mode)?;
                }
                if let Some(model) = &self.model {
                    self.request(
                        "session/set_model",
                        json!({ "sessionId": session_id, "modelId": model }),
                    )
                    .await?;
                }
                if let Some(mode) = &self.mode {
                    self.request(
                        "session/set_mode",
                        json!({ "sessionId": session_id, "modeId": mode }),
                    )
                    .await?;
                }
                Ok(session_id.to_string())
            }
            _ => Err(anyhow!(
                "ACP agent \"{}\" (cwd {cwd}) returned no sessionId",
                self.name
            )),
        }
    }

    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<PromptResult> {
        self.inner
            .lock()
            .expect("acp state mutex")
            .collectors
            .insert(session_id.to_string(), PromptResult::default());
        let result = self
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await;
        let parts = self
            .inner
            .lock()
            .expect("acp state mutex")
            .collectors
            .remove(session_id)
            .unwrap_or_default();
        result?;
        Ok(parts)
    }

    pub fn has_exited(&self) -> bool {
        *self.reaped.borrow()
    }

    pub async fn close(&self) -> Result<()> {
        let mut closed = self.close_lock.lock().await;
        if *closed {
            return Ok(());
        }
        self.exited.store(true, Ordering::SeqCst);
        self.fail_all(format!(
            "ACP agent \"{}\" (cwd {}) was closed",
            self.name, self.cwd
        ));
        if let Some(shutdown) = self.shutdown.lock().expect("shutdown mutex").take() {
            let _ = shutdown.send(());
        }
        let mut reaped = self.reaped.clone();
        let wait = async {
            while !*reaped.borrow() {
                reaped
                    .changed()
                    .await
                    .map_err(|_| anyhow!("ACP exit waiter stopped"))?;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .map_err(|_| anyhow!("ACP exit timed out; scratch must be retained"))??;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while group_exists(self.pid)? {
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "ACP process group {} still exists or cannot be inspected after leader exit; scratch must be retained",
                    self.pid
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.stdin.lock().await.take();
        *closed = true;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "ACP agent \"{}\" (cwd {}) is not running",
                self.name,
                self.cwd
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.inner.lock().expect("acp state mutex").pending.insert(
            id,
            Pending {
                method: method.to_string(),
                sender,
            },
        );
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&payload)?;
        line.push(b'\n');
        let write_error = {
            let mut guard = self.stdin.lock().await;
            match guard.as_mut() {
                Some(stdin) => match stdin.write_all(&line).await {
                    Ok(()) => {
                        let _ = stdin.flush().await;
                        None
                    }
                    Err(error) => Some(error.to_string()),
                },
                None => Some("stdin is closed".to_string()),
            }
        };
        if let Some(error) = write_error {
            self.fail_all(format!(
                "ACP agent \"{}\" (cwd {}) stdin write failed: {error}",
                self.name, self.cwd
            ));
        }
        match receiver.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(_) => Err(anyhow!(
                "ACP agent \"{}\" (cwd {}) request channel closed",
                self.name,
                self.cwd
            )),
        }
    }

    fn fail_all(&self, message: String) {
        fail_all(&self.inner, message);
    }
}

fn require_advertised_mode(session: &Value, mode: &str) -> Result<()> {
    let advertised = session
        .get("modes")
        .and_then(|modes| modes.get("availableModes"))
        .and_then(Value::as_array)
        .is_some_and(|modes| {
            modes
                .iter()
                .any(|entry| entry.get("id").and_then(Value::as_str) == Some(mode))
        });
    if !advertised {
        return Err(anyhow!("requested ACP session mode is not advertised by session/new; check the mode setting and adapter support"));
    }
    Ok(())
}

fn signal_group(pid: u32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).with_context(|| {
            format!("could not signal ACP process group {pid} with signal {signal}")
        })
    }
}

fn group_exists(pid: u32) -> Result<bool> {
    let result = unsafe { libc::kill(-(pid as i32), 0) };
    if result == 0 {
        Ok(true)
    } else {
        classify_group_error(pid, std::io::Error::last_os_error())
    }
}

fn classify_group_error(pid: u32, error: std::io::Error) -> Result<bool> {
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).with_context(|| {
            format!("could not inspect ACP process group {pid}; scratch must be retained")
        }),
    }
}

fn fail_all(inner: &Mutex<Inner>, message: String) {
    let mut inner = inner.lock().expect("acp state mutex");
    for (_, pending) in inner.pending.drain() {
        let _ = pending.sender.send(Err(message.clone()));
    }
    inner.collectors.clear();
}

async fn handle_line(name: &str, cwd: &str, inner: &Mutex<Inner>, stdin: &Stdin, line: &str) {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        let mut state = inner.lock().expect("acp state mutex");
        state.stderr_tail.push_str(&format!("\n{line}"));
        truncate_tail(&mut state.stderr_tail);
        return;
    };

    if let Some(method) = message.get("method").and_then(Value::as_str) {
        match message.get("id") {
            Some(id) if !id.is_null() => {
                handle_server_request(stdin, id, method, message.get("params")).await;
            }
            _ => {
                if method == "session/update" {
                    handle_update(inner, message.get("params"));
                }
            }
        }
        return;
    }

    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };
    let pending = inner.lock().expect("acp state mutex").pending.remove(&id);
    let Some(pending) = pending else { return };
    match message.get("error") {
        None | Some(Value::Null) => {
            let result = message.get("result").cloned().unwrap_or(Value::Null);
            let _ = pending.sender.send(Ok(result));
        }
        Some(error) => {
            let message = rpc_error(name, cwd, &pending.method, error);
            let _ = pending.sender.send(Err(message));
        }
    }
}

async fn handle_server_request(stdin: &Stdin, id: &Value, method: &str, params: Option<&Value>) {
    let response = if method == "session/request_permission" {
        let options = params
            .and_then(|params| params.get("options"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let by_kind = |kind: &str| {
            options
                .iter()
                .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
        };
        let option = by_kind("allow_once")
            .or_else(|| by_kind("allow_always"))
            .or_else(|| options.first());
        match option
            .and_then(|option| option.get("optionId"))
            .and_then(Value::as_str)
        {
            Some(option_id) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "outcome": { "outcome": "selected", "optionId": option_id } },
            }),
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "outcome": { "outcome": "cancelled" } },
            }),
        }
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not supported: {method}") },
        })
    };
    let mut line = serde_json::to_vec(&response).unwrap_or_default();
    line.push(b'\n');
    let mut guard = stdin.lock().await;
    if let Some(stdin) = guard.as_mut() {
        let _ = stdin.write_all(&line).await;
        let _ = stdin.flush().await;
    }
}

fn handle_update(inner: &Mutex<Inner>, params: Option<&Value>) {
    let Some(params) = params else { return };
    let Some(update) = params.get("update") else {
        return;
    };
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let mut inner = inner.lock().expect("acp state mutex");
    let Some(result) = inner.collectors.get_mut(session_id) else {
        return;
    };
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            if let Some(text) = update
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
            {
                result.text.push_str(text);
            }
        }
        Some("tool_call" | "tool_call_update") => {
            let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                return;
            };
            let call = result.tool_calls.entry(id.to_string()).or_default();
            if let Some(kind) = update.get("kind").and_then(Value::as_str) {
                call.kind = kind.to_string();
            }
            if let Some(status) = update.get("status").and_then(Value::as_str) {
                call.status = status.to_string();
            }
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                call.title = title.to_string();
            }
            if let Some(url) = update
                .get("rawInput")
                .and_then(|input| input.get("url"))
                .and_then(Value::as_str)
            {
                call.url = Some(url.to_string());
            }
        }
        _ => {}
    }
}

fn rpc_error(name: &str, cwd: &str, method: &str, error: &Value) -> String {
    let message = error.get("message").and_then(Value::as_str);
    let payload = match error.get("data") {
        None => message
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
        Some(data) => format!("{}: {data}", message.unwrap_or("error")),
    };
    let code = match error.get("code") {
        None | Some(Value::Null) => String::new(),
        Some(code) => match code {
            Value::String(text) => format!(" (code {text})"),
            other => format!(" (code {other})"),
        },
    };
    format!("ACP agent \"{name}\" (cwd {cwd}) {method} failed: {payload}{code}")
}

fn exit_description(status: Option<&std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "code null".to_string();
    };
    if let Some(code) = status.code() {
        return format!("code {code}");
    }
    match status.signal() {
        Some(signal) => signal_name(signal).to_string(),
        None => "code null".to_string(),
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => "SIGUNKNOWN",
    }
}

fn truncate_tail(tail: &mut String) {
    if tail.chars().count() <= 4000 {
        return;
    }
    *tail = last_chars(tail, 4000);
}

fn last_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_mode_must_be_advertised_by_the_agent() {
        let session = json!({
            "sessionId": "session",
            "modes": {
                "currentModeId": "default",
                "availableModes": [{"id": "default"}, {"id": "bypassPermissions"}, {"id": "custom-mode"}],
            },
        });
        assert!(require_advertised_mode(&session, "bypassPermissions").is_ok());
        assert!(require_advertised_mode(&session, "custom-mode").is_ok());
        assert!(require_advertised_mode(&session, "unsupported").is_err());
        for modes in [
            Value::Null,
            json!({}),
            json!({"availableModes": []}),
            json!({"availableModes": [{"id": 1}]}),
        ] {
            let session = json!({"sessionId": "session", "modes": modes});
            assert!(require_advertised_mode(&session, "bypassPermissions").is_err());
        }
        assert!(
            require_advertised_mode(&json!({"sessionId": "session"}), "bypassPermissions").is_err()
        );
    }

    #[test]
    fn group_probe_permission_error_does_not_mean_absent() {
        assert!(classify_group_error(123, std::io::Error::from_raw_os_error(libc::EPERM)).unwrap());
        assert!(
            !classify_group_error(123, std::io::Error::from_raw_os_error(libc::ESRCH)).unwrap()
        );
        let error =
            classify_group_error(123, std::io::Error::from_raw_os_error(libc::EINVAL)).unwrap_err();
        assert!(error.to_string().contains("process group 123"));
        assert!(error.to_string().contains("scratch must be retained"));
    }

    #[test]
    fn web_evidence_requires_matching_completed_fetch() {
        let inner = Mutex::new(Inner::default());
        inner
            .lock()
            .unwrap()
            .collectors
            .insert("session".into(), PromptResult::default());
        handle_update(
            &inner,
            Some(&json!({
                "sessionId": "session",
                "update": {
                    "sessionUpdate": "tool_call", "toolCallId": "fetch-1",
                    "kind": "fetch", "status": "in_progress",
                    "rawInput": {"url": "https://example.org/page"}
                }
            })),
        );
        assert!(
            !inner.lock().unwrap().collectors["session"].fetched_url("https://example.org/page")
        );
        handle_update(
            &inner,
            Some(&json!({
                "sessionId": "session",
                "update": {"sessionUpdate": "tool_call_update", "toolCallId": "fetch-1", "status": "completed"}
            })),
        );
        let mut state = inner.lock().unwrap();
        assert!(state.collectors["session"].fetched_url("https://example.org/page"));
        assert!(!state.collectors["session"].fetched_url("https://example.org/other"));
        state
            .collectors
            .get_mut("session")
            .unwrap()
            .tool_calls
            .get_mut("fetch-1")
            .unwrap()
            .status = "failed".into();
        assert!(!state.collectors["session"].fetched_url("https://example.org/page"));
        state
            .collectors
            .insert("another-session".into(), PromptResult::default());
        assert!(!state.collectors["another-session"].fetched_url("https://example.org/page"));
    }
}

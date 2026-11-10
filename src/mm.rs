use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Method;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmUser {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub username: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmChannel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(rename = "type", default)]
    pub channel_type: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmChannelMember {
    #[serde(default)]
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmPost {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub root_id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub create_at: i64,
    #[serde(default)]
    pub props: Option<Value>,
}

pub const APP_PROP: &str = "acp_assist";

impl MmPost {
    pub fn is_app_post(&self) -> bool {
        self.props
            .as_ref()
            .and_then(|props| props.get(APP_PROP))
            .and_then(Value::as_bool)
            == Some(true)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmReaction {
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub post_id: String,
    #[serde(default)]
    pub emoji_name: String,
    #[serde(default)]
    pub channel_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmThread {
    #[serde(default)]
    pub posts: HashMap<String, MmPost>,
}

pub struct MmEvent {
    pub event: String,
    pub data: Map<String, Value>,
}

pub fn json_field<T: DeserializeOwned>(value: &Value) -> Option<T> {
    match value {
        Value::String(text) => serde_json::from_str(text).ok(),
        Value::Null => None,
        other => serde_json::from_value(other.clone()).ok(),
    }
}

pub struct MattermostClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl MattermostClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .build()
                .expect("build the Mattermost HTTP client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    pub async fn get_post(&self, post_id: &str) -> Result<MmPost> {
        self.typed("GET", &format!("/posts/{post_id}"), None).await
    }

    pub async fn get_user(&self, user_id: &str) -> Result<MmUser> {
        self.typed("GET", &format!("/users/{user_id}"), None).await
    }

    pub async fn get_channel(&self, channel_id: &str) -> Result<MmChannel> {
        self.typed("GET", &format!("/channels/{channel_id}"), None)
            .await
    }

    pub async fn get_channel_members(&self, channel_id: &str) -> Result<Vec<MmChannelMember>> {
        self.typed(
            "GET",
            &format!("/channels/{channel_id}/members?per_page=200"),
            None,
        )
        .await
    }

    pub async fn create_direct(&self, user_ids: &[String]) -> Result<MmChannel> {
        let body = Value::Array(user_ids.iter().cloned().map(Value::String).collect());
        self.typed("POST", "/channels/direct", Some(&body)).await
    }

    pub async fn create_post(
        &self,
        channel_id: &str,
        message: &str,
        root_id: Option<&str>,
        props: Option<Value>,
    ) -> Result<MmPost> {
        let mut body = Map::new();
        body.insert(
            "channel_id".to_string(),
            Value::String(channel_id.to_string()),
        );
        body.insert("message".to_string(), Value::String(message.to_string()));
        if let Some(root_id) = root_id.filter(|id| !id.is_empty()) {
            body.insert("root_id".to_string(), Value::String(root_id.to_string()));
        }
        if let Some(props) = props {
            body.insert("props".to_string(), props);
        }
        self.typed("POST", "/posts", Some(&Value::Object(body)))
            .await
    }

    pub async fn add_reaction(&self, user_id: &str, post_id: &str, emoji_name: &str) -> Result<()> {
        let body = Value::Object(Map::from_iter([
            ("user_id".to_string(), Value::String(user_id.to_string())),
            ("post_id".to_string(), Value::String(post_id.to_string())),
            (
                "emoji_name".to_string(),
                Value::String(emoji_name.to_string()),
            ),
        ]));
        self.request("POST", "/reactions", Some(&body)).await?;
        Ok(())
    }

    pub async fn get_thread(&self, post_id: &str) -> Result<MmThread> {
        self.typed("GET", &format!("/posts/{post_id}/thread"), None)
            .await
    }

    pub async fn get_users_by_ids(&self, user_ids: &[String]) -> Result<Vec<MmUser>> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let body = Value::Array(user_ids.iter().cloned().map(Value::String).collect());
        self.typed("POST", "/users/ids", Some(&body)).await
    }

    async fn typed<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let value = self.request(method, path, body).await?;
        let value = value.ok_or_else(|| anyhow!("Mattermost {method} {path} returned no body"))?;
        serde_json::from_value(value).map_err(Into::into)
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<Value>> {
        let url = format!("{}/api/v4{path}", self.base_url);
        let mut request = self
            .http
            .request(
                Method::from_bytes(method.as_bytes()).expect("valid HTTP method"),
                &url,
            )
            .header(AUTHORIZATION, format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(serde_json::to_string(body)?);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "Mattermost {method} {path} failed ({}): {}",
                status.as_u16(),
                first_chars(&text, 500)
            ));
        }
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text)?))
    }
}

pub type EventHandler = Arc<dyn Fn(MmEvent) + Send + Sync>;

pub struct MattermostSocket {
    supervisor: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct SocketTiming {
    connect: Duration,
    auth: Duration,
    write: Duration,
    ping: Duration,
    pong: Duration,
    retry_initial: Duration,
    retry_max: Duration,
    healthy: Duration,
}

impl Default for SocketTiming {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            auth: Duration::from_secs(15),
            write: Duration::from_secs(5),
            ping: Duration::from_secs(15),
            pong: Duration::from_secs(5),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            healthy: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SocketFailure {
    ConnectTimeout,
    ConnectFailed,
    AuthTimeout,
    AuthRejected,
    Closed,
    ReadFailed,
    WriteTimeout,
    WriteFailed,
    PongTimeout,
}

impl SocketFailure {
    fn label(&self) -> &'static str {
        match self {
            Self::ConnectTimeout => "connection timed out",
            Self::ConnectFailed => "connection failed",
            Self::AuthTimeout => "authentication timed out",
            Self::AuthRejected => "authentication rejected",
            Self::Closed => "connection closed",
            Self::ReadFailed => "read failed",
            Self::WriteTimeout => "write timed out",
            Self::WriteFailed => "write failed",
            Self::PongTimeout => "heartbeat timed out",
        }
    }
}

enum Heartbeat {
    Idle { ping_at: Instant },
    Waiting { nonce: [u8; 8], deadline: Instant },
}

impl Heartbeat {
    fn timer(&self) -> Instant {
        match self {
            Self::Idle { ping_at } => *ping_at,
            Self::Waiting { deadline, .. } => *deadline,
        }
    }

    fn write_deadline(&self, timing: SocketTiming) -> Instant {
        match self {
            Self::Idle { .. } => Instant::now() + timing.write,
            Self::Waiting { deadline, .. } => *deadline,
        }
    }
}

impl MattermostSocket {
    pub async fn connect(
        base_url: &str,
        token: &str,
        owner: &str,
        handler: EventHandler,
    ) -> Result<Self> {
        Self::connect_with_timing(base_url, token, owner, handler, SocketTiming::default()).await
    }

    async fn connect_with_timing(
        base_url: &str,
        token: &str,
        owner: &str,
        handler: EventHandler,
        timing: SocketTiming,
    ) -> Result<Self> {
        let base = base_url.trim_end_matches('/');
        let ws_base = match base.strip_prefix("http") {
            Some(rest) => format!("ws{rest}"),
            None => base.to_string(),
        };
        let url = format!("{ws_base}/api/v4/websocket");
        let owner = safe_owner_label(owner);
        let socket = authenticated_socket(&url, token, timing)
            .await
            .map_err(|error| anyhow!("Mattermost websocket ({owner}): {}", error.label()))?;
        let token = token.to_string();
        Ok(Self {
            supervisor: Some(tokio::spawn(supervise_socket(
                socket, url, token, owner, handler, timing,
            ))),
        })
    }

    pub async fn close(&mut self) {
        if let Some(supervisor) = self.supervisor.as_mut() {
            supervisor.abort();
            let _ = supervisor.await;
        }
        self.supervisor.take();
    }
}

impl Drop for MattermostSocket {
    fn drop(&mut self) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.abort();
        }
    }
}

fn safe_owner_label(owner: &str) -> String {
    owner
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect()
}

async fn write_socket(
    socket: &mut Socket,
    message: Option<Message>,
    deadline: Instant,
    timing: SocketTiming,
) -> std::result::Result<(), SocketFailure> {
    let deadline = deadline.min(Instant::now() + timing.write);
    if Instant::now() >= deadline {
        return Err(SocketFailure::WriteTimeout);
    }
    tokio::time::timeout_at(deadline, async {
        match message {
            Some(message) => socket.send(message).await,
            None => socket.flush().await,
        }
    })
    .await
    .map_err(|_| SocketFailure::WriteTimeout)?
    .map_err(|_| SocketFailure::WriteFailed)
}

async fn authenticated_socket(
    url: &str,
    token: &str,
    timing: SocketTiming,
) -> std::result::Result<Socket, SocketFailure> {
    let (mut socket, _) = tokio::time::timeout(timing.connect, connect_async(url))
        .await
        .map_err(|_| SocketFailure::ConnectTimeout)?
        .map_err(|_| SocketFailure::ConnectFailed)?;
    let deadline = Instant::now() + timing.auth;
    let auth = serde_json::json!({
        "seq": 1,
        "action": "authentication_challenge",
        "data": { "token": token },
    });
    write_socket(
        &mut socket,
        Some(Message::Text(auth.to_string().into())),
        deadline,
        timing,
    )
    .await?;
    loop {
        let message = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => return Err(SocketFailure::AuthTimeout),
            message = socket.next() => message,
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                let Ok(payload) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if payload.get("seq_reply").and_then(Value::as_u64) == Some(1) {
                    if payload.get("status").and_then(Value::as_str) == Some("OK") {
                        return Ok(socket);
                    }
                    return Err(SocketFailure::AuthRejected);
                }
            }
            Some(Ok(Message::Ping(_))) => {
                write_socket(&mut socket, None, deadline, timing).await?;
            }
            Some(Ok(Message::Close(_))) | None => return Err(SocketFailure::Closed),
            Some(Err(_)) => return Err(SocketFailure::ReadFailed),
            _ => {}
        }
    }
}

async fn connected_socket(
    socket: &mut Socket,
    handler: &EventHandler,
    timing: SocketTiming,
) -> SocketFailure {
    let mut heartbeat = Heartbeat::Idle {
        ping_at: Instant::now() + timing.ping,
    };
    let mut sequence = 0u64;
    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(heartbeat.timer()) => {
                if matches!(heartbeat, Heartbeat::Waiting { .. }) {
                    return SocketFailure::PongTimeout;
                }
                sequence = sequence.wrapping_add(1);
                let nonce = sequence.to_be_bytes();
                let deadline = Instant::now() + timing.pong;
                if let Err(error) = write_socket(
                    socket, Some(Message::Ping(nonce.to_vec().into())), deadline, timing,
                ).await {
                    return error;
                }
                heartbeat = Heartbeat::Waiting { nonce, deadline };
            }
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(payload) = serde_json::from_str::<Value>(text.as_str()) {
                        if let Some(event) = event_from(&payload) {
                            handler(event);
                        }
                    }
                }
                Some(Ok(Message::Pong(payload))) => {
                    if let Heartbeat::Waiting { nonce, .. } = &heartbeat {
                        if payload.as_ref() == nonce {
                            heartbeat = Heartbeat::Idle {
                                ping_at: Instant::now() + timing.ping,
                            };
                        }
                    }
                }
                Some(Ok(Message::Ping(_))) => {
                    if let Err(error) = write_socket(
                        socket, None, heartbeat.write_deadline(timing), timing,
                    ).await {
                        return error;
                    }
                }
                Some(Ok(Message::Close(_))) | None => return SocketFailure::Closed,
                Some(Err(_)) => return SocketFailure::ReadFailed,
                _ => {}
            },
        }
    }
}

fn retry_after_connection(delay: Duration, connected: Duration, timing: SocketTiming) -> Duration {
    if connected >= timing.healthy {
        timing.retry_initial
    } else {
        delay
    }
}

fn advance_retry(delay: Duration, timing: SocketTiming) -> Duration {
    delay.saturating_mul(2).min(timing.retry_max)
}

async fn supervise_socket(
    mut socket: Socket,
    url: String,
    token: String,
    owner: String,
    handler: EventHandler,
    timing: SocketTiming,
) {
    let mut delay = timing.retry_initial;
    loop {
        let connected = Instant::now();
        let failure = connected_socket(&mut socket, &handler, timing).await;
        delay = retry_after_connection(delay, connected.elapsed(), timing);
        drop(socket);
        eprintln!(
            "Mattermost websocket ({owner}) disconnected: {}",
            failure.label()
        );
        loop {
            eprintln!(
                "Mattermost websocket ({owner}) retry in {}s",
                delay.as_secs_f64()
            );
            tokio::time::sleep(delay).await;
            delay = advance_retry(delay, timing);
            match authenticated_socket(&url, &token, timing).await {
                Ok(reconnected) => {
                    socket = reconnected;
                    eprintln!("Mattermost websocket ({owner}) reconnected");
                    break;
                }
                Err(error) => eprintln!(
                    "Mattermost websocket ({owner}) retry failed: {}",
                    error.label(),
                ),
            }
        }
    }
}

fn event_from(payload: &Value) -> Option<MmEvent> {
    let event = payload.get("event")?.as_str()?.to_string();
    let data = match payload.get("data") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    Some(MmEvent { event, data })
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod socket_tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};
    use tokio_tungstenite::accept_async;

    type Peer = WebSocketStream<TcpStream>;

    fn timing() -> SocketTiming {
        SocketTiming {
            connect: Duration::from_secs(2),
            auth: Duration::from_secs(2),
            write: Duration::from_secs(1),
            ping: Duration::from_millis(100),
            pong: Duration::from_millis(500),
            retry_initial: Duration::from_millis(30),
            retry_max: Duration::from_millis(120),
            healthy: Duration::from_secs(1),
        }
    }

    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(8), future)
            .await
            .expect("websocket test deadline")
    }

    fn handler() -> (EventHandler, mpsc::UnboundedReceiver<String>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let handler: EventHandler = Arc::new(move |event| {
            let _ = sender.send(event.event);
        });
        (handler, receiver)
    }

    async fn fixture() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    async fn accept(listener: &TcpListener) -> Peer {
        let (stream, _) = within(listener.accept()).await.unwrap();
        within(accept_async(stream)).await.unwrap()
    }

    async fn read_auth(peer: &mut Peer) {
        let message = within(peer.next()).await.unwrap().unwrap();
        let Message::Text(text) = message else {
            panic!("expected authentication request");
        };
        let payload: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["seq"], 1);
        assert_eq!(payload["action"], "authentication_challenge");
        assert_eq!(payload["data"]["token"], "test-token");
    }

    async fn authenticated(peer: &mut Peer) {
        read_auth(peer).await;
        peer.send(Message::Text(r#"{"seq_reply":1,"status":"OK"}"#.into()))
            .await
            .unwrap();
    }

    async fn event(peer: &mut Peer, name: &str) {
        peer.send(Message::Text(
            serde_json::json!({"event": name, "data": {}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }

    async fn wait_closed(peer: &mut Peer) {
        within(async {
            while let Some(Ok(message)) = peer.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn close_and_eof_reconnect_with_fresh_auth_and_same_handler() {
        for close_frame in [true, false] {
            let (listener, url) = fixture().await;
            let (handler, mut events) = handler();
            let (disconnect, disconnected) = oneshot::channel();
            let server = tokio::spawn(async move {
                let mut first = accept(&listener).await;
                authenticated(&mut first).await;
                event(&mut first, "first").await;
                disconnected.await.unwrap();
                if close_frame {
                    first.send(Message::Close(None)).await.unwrap();
                }
                drop(first);
                let mut rejected = accept(&listener).await;
                read_auth(&mut rejected).await;
                event(&mut rejected, "rejected-pre-auth").await;
                rejected
                    .send(Message::Text(
                        r#"{"seq_reply":1,"status":"test-token private-retry-reason"}"#.into(),
                    ))
                    .await
                    .unwrap();
                wait_closed(&mut rejected).await;
                drop(rejected);
                let mut second = accept(&listener).await;
                read_auth(&mut second).await;
                event(&mut second, "pre-auth").await;
                second
                    .send(Message::Text(r#"{"seq_reply":2,"status":"OK"}"#.into()))
                    .await
                    .unwrap();
                second
                    .send(Message::Text(r#"{"seq_reply":1,"status":"OK"}"#.into()))
                    .await
                    .unwrap();
                event(&mut second, "second").await;
                wait_closed(&mut second).await;
            });
            let mut client = MattermostSocket::connect_with_timing(
                &url,
                "test-token",
                "owner1",
                handler,
                timing(),
            )
            .await
            .unwrap();
            assert_eq!(within(events.recv()).await.as_deref(), Some("first"));
            disconnect.send(()).unwrap();
            assert_eq!(within(events.recv()).await.as_deref(), Some("second"));
            client.close().await;
            client.close().await;
            within(server).await.unwrap();
            assert_eq!(within(events.recv()).await, None);
        }
    }

    #[tokio::test]
    async fn authentication_discards_events_and_requires_exact_sequence() {
        let (listener, url) = fixture().await;
        let (handler, mut events) = handler();
        let (wrong_sent, wrong_received) = oneshot::channel();
        let (allow_auth, auth_allowed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut peer = accept(&listener).await;
            read_auth(&mut peer).await;
            peer.send(Message::Text(r#"{"seq_reply":2,"status":"OK"}"#.into()))
                .await
                .unwrap();
            event(&mut peer, "must-not-dispatch").await;
            peer.send(Message::Ping(vec![7].into())).await.unwrap();
            assert_eq!(
                within(peer.next()).await.unwrap().unwrap(),
                Message::Pong(vec![7].into())
            );
            wrong_sent.send(()).unwrap();
            auth_allowed.await.unwrap();
            peer.send(Message::Text(r#"{"seq_reply":1,"status":"OK"}"#.into()))
                .await
                .unwrap();
            event(&mut peer, "authenticated").await;
            wait_closed(&mut peer).await;
        });
        let connection = tokio::spawn(async move {
            MattermostSocket::connect_with_timing(&url, "test-token", "owner1", handler, timing())
                .await
        });
        within(wrong_received).await.unwrap();
        assert!(!connection.is_finished());
        assert!(events.try_recv().is_err());
        allow_auth.send(()).unwrap();
        let mut client = within(connection).await.unwrap().unwrap();
        assert_eq!(
            within(events.recv()).await.as_deref(),
            Some("authenticated")
        );
        client.close().await;
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn initial_auth_failure_is_failfast_and_redacted() {
        let (listener, url) = fixture().await;
        let server = tokio::spawn(async move {
            let mut peer = accept(&listener).await;
            read_auth(&mut peer).await;
            peer.send(Message::Text(
                r#"{"seq_reply":1,"status":"test-token remote-private-body"}"#.into(),
            ))
            .await
            .unwrap();
        });
        let (handler, _) = handler();
        let error = MattermostSocket::connect_with_timing(
            &url,
            "test-token",
            "owner1\n\u{1b}[31m",
            handler,
            timing(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("authentication rejected"));
        assert!(!error.contains("test-token"));
        assert!(!error.contains("remote-private-body"));
        assert!(!error.contains('\n'));
        assert!(!error.contains('\u{1b}'));
        assert!(!error.contains(&url));
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn wrong_auth_sequence_times_out_without_dispatch() {
        let (listener, url) = fixture().await;
        let server = tokio::spawn(async move {
            let mut peer = accept(&listener).await;
            read_auth(&mut peer).await;
            peer.send(Message::Text(r#"{"seq_reply":99,"status":"OK"}"#.into()))
                .await
                .unwrap();
            event(&mut peer, "ignored").await;
            wait_closed(&mut peer).await;
        });
        let (handler, mut events) = handler();
        let error = within(MattermostSocket::connect_with_timing(
            &url,
            "test-token",
            "owner1",
            handler,
            SocketTiming {
                auth: Duration::from_millis(200),
                ..timing()
            },
        ))
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("authentication timed out"));
        assert!(events.try_recv().is_err());
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn responsive_idle_connection_stays_online() {
        let (listener, url) = fixture().await;
        let server = tokio::spawn(async move {
            let mut peer = accept(&listener).await;
            authenticated(&mut peer).await;
            for _ in 0..3 {
                assert!(matches!(
                    within(peer.next()).await.unwrap().unwrap(),
                    Message::Ping(_)
                ));
                peer.flush().await.unwrap();
            }
            event(&mut peer, "still-online").await;
            wait_closed(&mut peer).await;
        });
        let (handler, mut events) = handler();
        let mut client =
            MattermostSocket::connect_with_timing(&url, "test-token", "owner1", handler, timing())
                .await
                .unwrap();
        assert_eq!(within(events.recv()).await.as_deref(), Some("still-online"));
        client.close().await;
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn silent_peer_reconnects_after_heartbeat_deadline() {
        let (listener, url) = fixture().await;
        let server = tokio::spawn(async move {
            let mut silent = accept(&listener).await;
            authenticated(&mut silent).await;
            let mut replacement = accept(&listener).await;
            authenticated(&mut replacement).await;
            event(&mut replacement, "reconnected").await;
            wait_closed(&mut replacement).await;
            drop(silent);
        });
        let (handler, mut events) = handler();
        let mut client =
            MattermostSocket::connect_with_timing(&url, "test-token", "owner1", handler, timing())
                .await
                .unwrap();
        assert_eq!(within(events.recv()).await.as_deref(), Some("reconnected"));
        client.close().await;
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn wrong_pongs_and_chat_do_not_extend_heartbeat_deadline() {
        let (listener, url) = fixture().await;
        let server = tokio::spawn(async move {
            let mut peer = accept(&listener).await;
            authenticated(&mut peer).await;
            let mut pulse = tokio::time::interval(Duration::from_millis(5));
            let stream = loop {
                tokio::select! {
                    accepted = listener.accept() => break accepted.unwrap().0,
                    _ = pulse.tick() => {
                        if peer.send(Message::Pong(vec![9].into())).await.is_err() {
                            break within(listener.accept()).await.unwrap().0;
                        }
                        if peer.send(Message::Text(r#"{"event":"noise","data":{}}"#.into())).await.is_err() {
                            break within(listener.accept()).await.unwrap().0;
                        }
                    }
                }
            };
            let mut replacement = accept_async(stream).await.unwrap();
            authenticated(&mut replacement).await;
            event(&mut replacement, "reconnected").await;
            wait_closed(&mut replacement).await;
        });
        let (handler, mut events) = handler();
        let mut client =
            MattermostSocket::connect_with_timing(&url, "test-token", "owner1", handler, timing())
                .await
                .unwrap();
        within(async {
            loop {
                if events.recv().await.as_deref() == Some("reconnected") {
                    break;
                }
            }
        })
        .await;
        client.close().await;
        within(server).await.unwrap();
    }

    #[test]
    fn retries_increase_cap_and_reset_only_after_healthy_connection() {
        let timing = SocketTiming::default();
        let mut delay = timing.retry_initial;
        for expected in [1, 2, 4, 8, 16, 30, 30] {
            assert_eq!(delay, Duration::from_secs(expected));
            assert_eq!(
                retry_after_connection(delay, Duration::from_secs(59), timing),
                delay
            );
            delay = advance_retry(delay, timing);
        }
        assert_eq!(
            retry_after_connection(delay, Duration::from_secs(60), timing),
            timing.retry_initial
        );
    }

    #[derive(Clone, Copy)]
    enum RetryStage {
        Backoff,
        Dial,
        Auth,
    }

    #[tokio::test]
    async fn close_and_drop_stop_backoff_dial_and_reauthentication() {
        for stage in [RetryStage::Backoff, RetryStage::Dial, RetryStage::Auth] {
            for drop_client in [false, true] {
                let (listener, url) = fixture().await;
                let (handler, mut events) = handler();
                let (ready, waiting) = oneshot::channel();
                let (stop, stopped) = oneshot::channel();
                let server = tokio::spawn(async move {
                    let mut peer = accept(&listener).await;
                    authenticated(&mut peer).await;
                    peer.send(Message::Close(None)).await.unwrap();
                    wait_closed(&mut peer).await;
                    match stage {
                        RetryStage::Backoff => {
                            ready.send(()).unwrap();
                        }
                        RetryStage::Dial => {
                            let (mut stream, _) = within(listener.accept()).await.unwrap();
                            ready.send(()).unwrap();
                            let mut buffer = [0; 4096];
                            within(async {
                                while stream.read(&mut buffer).await.unwrap_or(0) != 0 {}
                            })
                            .await;
                        }
                        RetryStage::Auth => {
                            let mut peer = accept(&listener).await;
                            read_auth(&mut peer).await;
                            ready.send(()).unwrap();
                            wait_closed(&mut peer).await;
                        }
                    }
                    stopped.await.unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(400), listener.accept())
                            .await
                            .is_err()
                    );
                });
                let mut client = MattermostSocket::connect_with_timing(
                    &url,
                    "test-token",
                    "owner1",
                    handler,
                    SocketTiming {
                        retry_initial: Duration::from_millis(200),
                        retry_max: Duration::from_millis(200),
                        ..timing()
                    },
                )
                .await
                .unwrap();
                within(waiting).await.unwrap();
                if drop_client {
                    drop(client);
                } else {
                    client.close().await;
                    client.close().await;
                }
                stop.send(()).unwrap();
                within(server).await.unwrap();
                assert_eq!(within(events.recv()).await, None);
            }
        }
    }

    #[tokio::test]
    async fn cancelled_close_retains_handle_and_online_drop_stops_connection() {
        for cancel_close in [false, true] {
            let (listener, url) = fixture().await;
            let server = tokio::spawn(async move {
                let mut peer = accept(&listener).await;
                authenticated(&mut peer).await;
                wait_closed(&mut peer).await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), listener.accept())
                        .await
                        .is_err()
                );
            });
            let (handler, mut events) = handler();
            let mut client = MattermostSocket::connect_with_timing(
                &url,
                "test-token",
                "owner1",
                handler,
                timing(),
            )
            .await
            .unwrap();
            if cancel_close {
                {
                    let mut close = Box::pin(client.close());
                    std::future::poll_fn(|context| {
                        let _ = close.as_mut().poll(context);
                        Poll::Ready(())
                    })
                    .await;
                }
                assert!(client.supervisor.is_some());
                client.close().await;
                assert!(client.supervisor.is_none());
            } else {
                drop(client);
            }
            within(server).await.unwrap();
            assert_eq!(within(events.recv()).await, None);
        }
    }
}

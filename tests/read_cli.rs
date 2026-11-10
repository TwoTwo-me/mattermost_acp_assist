use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROOT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const REACTED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
const SIBLING: &str = "cccccccccccccccccccccccccc";
const LATEST: &str = "dddddddddddddddddddddddddd";

struct FixtureDir(PathBuf);

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn post(id: &str, root: &str, text: &str, time: i64) -> Value {
    json!({"id": id, "root_id": root, "message": text, "create_at": time, "channel_id": "channel", "user_id": "author"})
}

#[test]
fn read_from_scratch_includes_entire_thread_and_refreshes_only_when_invoked() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = FixtureDir(
        std::env::temp_dir().join(format!("acp-read-test-{}-{nonce}", std::process::id())),
    );
    std::fs::create_dir(&home.0).unwrap();
    std::fs::create_dir(home.0.join("acp")).unwrap();
    std::fs::create_dir(home.0.join("scratch")).unwrap();
    std::fs::write(
        home.0.join("acp/agents.json"),
        r#"{"stub":{"command":["self","stub-agent"]}}"#,
    )
    .unwrap();
    std::fs::write(
        home.0.join("acp/registry.json"),
        r#"{"username":{"agent":"stub","cwd":"."}}"#,
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::fs::write(home.0.join("test.env"), format!(
        "MM_URL={url}\nMM_TEAM=test\nMM_MONITOR_CHANNEL_IDS=channel\nTRIGGER_EMOJI=robot\nMM_OWNER1_TOKEN=fixture-token\nMM_OWNER1_USER_ID=owner\n"
    )).unwrap();
    let phase = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server_phase = phase.clone();
    let server_requests = requests.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while server_requests.lock().unwrap().len() < 10 {
            assert!(
                Instant::now() < deadline,
                "fixture did not receive expected requests"
            );
            let (mut socket, _) = match listener.accept() {
                Ok(socket) => socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let header_end = loop {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + content_length {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            assert!(headers
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-token"));
            let path = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_string();
            server_requests.lock().unwrap().push(path.clone());
            let phase = server_phase.load(Ordering::SeqCst);
            let body = if path == format!("/api/v4/posts/{REACTED}") {
                post(REACTED, ROOT, "reacted middle reply", 2)
            } else if path == format!("/api/v4/posts/{ROOT}/thread") {
                let mut posts = serde_json::Map::from_iter([
                    (ROOT.into(), post(ROOT, "", "root context", 1)),
                    (
                        REACTED.into(),
                        post(REACTED, ROOT, "reacted middle reply", 2),
                    ),
                    (SIBLING.into(), post(SIBLING, ROOT, "sibling context", 3)),
                ]);
                if phase >= 1 {
                    posts.insert(
                        LATEST.into(),
                        post(LATEST, ROOT, "new reply after initial context", 4),
                    );
                }
                if phase == 2 {
                    posts.remove(ROOT);
                }
                json!({"order": [SIBLING, REACTED, ROOT], "posts": posts})
            } else if path == "/api/v4/channels/channel" {
                json!({"id":"channel", "name":"source", "type":"O"})
            } else if path == "/api/v4/users/ids" {
                json!([{"id":"author", "username":"alice"}])
            } else {
                panic!("unexpected source request {path}");
            };
            let body = body.to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let read_source = || -> Output {
        Command::new(env!("CARGO_BIN_EXE_mattermost-acp-assist"))
            .args(["read", "--as", "owner1", REACTED])
            .current_dir(home.0.join("scratch"))
            .env_clear()
            .env("ACP_ASSIST_HOME", &home.0)
            .env("ACP_ASSIST_ENV", home.0.join("test.env"))
            .output()
            .unwrap()
    };
    let first = read_source();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first = String::from_utf8(first.stdout).unwrap();
    assert!(first.find("root context").unwrap() < first.find("reacted middle reply").unwrap());
    assert!(first.find("reacted middle reply").unwrap() < first.find("sibling context").unwrap());
    assert!(first.contains("[reacted] alice:"));
    assert!(!first.contains("new reply after initial context"));
    assert_eq!(requests.lock().unwrap().len(), 4);
    phase.store(1, Ordering::SeqCst);
    assert_eq!(requests.lock().unwrap().len(), 4);
    let refreshed = read_source();
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    assert!(String::from_utf8(refreshed.stdout)
        .unwrap()
        .contains("new reply after initial context"));
    assert_eq!(requests.lock().unwrap().len(), 8);
    phase.store(2, Ordering::SeqCst);
    let failed = read_source();
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8(failed.stderr)
        .unwrap()
        .contains("missing the root or reacted post"));
    server.join().unwrap();
}

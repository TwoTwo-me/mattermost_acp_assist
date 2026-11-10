use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const URL: &str = "https://example.com/source";
const QUESTION: &str = "질문 {{language}}";
const AGENT: &str = r#"/usr/bin/env > "$2"
while IFS= read -r line; do
    id=${line#*'"id":'}
    id=${id%%,*}
    case "$line" in
        *'"method":"initialize"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id"
            ;;
        *'"method":"session/new"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"fixture"}}\n' "$id"
            ;;
        *'"method":"session/prompt"'*)
            printf '%s\n' "$line" > "$1"
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fixture","update":{"sessionUpdate":"tool_call","toolCallId":"fetch","kind":"fetch","status":"completed","rawInput":{"url":"https://example.com/source"}}}}'
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fixture","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Fixture citation https://example.com/source"}}}}'
            printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
            ;;
    esac
done
"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = Self(
            std::env::temp_dir().join(format!("acp-prompt-test-{}-{nonce}", std::process::id())),
        );
        for directory in ["acp", "workspace", "settings", "tmp", "workspaces/owner1"] {
            std::fs::create_dir_all(fixture.0.join(directory)).unwrap();
        }
        std::fs::write(fixture.0.join("agent.sh"), AGENT).unwrap();
        fixture.configure("claude", json!({}), "");
        fixture
    }

    fn configure(&self, name: &str, agent_env: Value, extra_env: &str) {
        std::fs::write(
            self.0.join("acp/agents.json"),
            json!({name: {
                "command": ["/bin/sh", self.0.join("agent.sh"), self.0.join("request.json"), self.0.join("child.env")],
                "env": agent_env
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            self.0.join("acp/registry.json"),
            json!({"alice": {"agent": name, "cwd": "workspace"}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            self.0.join("settings/test.env"),
            format!(
                "MM_URL=http://127.0.0.1:1\nMM_TEAM=test\nMM_MONITOR_CHANNEL_IDS=channel\nTRIGGER_EMOJI=robot\nMM_OWNER1_TOKEN=fixture-token\nMM_OWNER1_USER_ID=owner\nACP_RESPONSE_LANGUAGE=English\nANTHROPIC_API_KEY=file-secret\nOPENAI_API_KEY=other-file-secret\nUNRELATED_SECRET=not-exported\n{extra_env}"
            ),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mattermost-acp-assist"));
        command
            .args([
                "smoke",
                "--web",
                URL,
                "--profile",
                "alice",
                "--question",
                QUESTION,
            ])
            .current_dir(self.0.join("workspace"))
            .env_clear()
            .env("ACP_ASSIST_HOME", &self.0)
            .env("ACP_ASSIST_ENV", "settings/test.env")
            .env("HOME", &self.0)
            .env("TMPDIR", self.0.join("tmp"));
        command
    }

    fn prompt(&self) -> String {
        let request: Value =
            serde_json::from_slice(&std::fs::read(self.0.join("request.json")).unwrap()).unwrap();
        request["params"]["prompt"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn environment(&self) -> HashMap<String, String> {
        std::fs::read_to_string(self.0.join("child.env"))
            .unwrap()
            .lines()
            .map(|line| {
                let (name, value) = line.split_once('=').unwrap();
                (name.to_string(), value.to_string())
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn template_path_precedence_literal_insertion_and_restart() {
    let fixture = Fixture::new();
    fixture.configure("claude", json!({}), "ACP_PROMPT_FILE=acp/prompt.txt\n");
    std::fs::write(
        fixture.0.join("acp/prompt.txt"),
        "Custom {{language}} | {{mattermosturl}} | {{question}}",
    )
    .unwrap();
    success(fixture.command().output().unwrap());
    let prompt = fixture.prompt();
    assert!(prompt.contains(&format!("Custom English | {URL} | {QUESTION}\nRead {URL}")));
    assert!(prompt.contains("Answer in English."));
    assert!(prompt.contains("Session scratch directory:"));
    assert!(!prompt.contains("User question:"));
    assert!(!prompt.contains("Initial source thread snapshot:"));

    let absolute = fixture.0.join(" absolute prompt.txt ");
    std::fs::write(&absolute, "Absolute {{question}}").unwrap();
    success(
        fixture
            .command()
            .env("ACP_PROMPT_FILE", &absolute)
            .output()
            .unwrap(),
    );
    assert!(fixture.prompt().contains(&format!("Absolute {QUESTION}")));
    success(
        fixture
            .command()
            .env("ACP_PROMPT_FILE", " absolute prompt.txt ")
            .output()
            .unwrap(),
    );
    assert!(fixture.prompt().contains(&format!("Absolute {QUESTION}")));

    success(
        fixture
            .command()
            .env("ACP_PROMPT_FILE", "")
            .output()
            .unwrap(),
    );
    let default = fixture.prompt();
    assert!(default.contains(&format!("User question:\n{QUESTION}")));
    assert!(default.contains(&format!("Mattermost thread permalink: {URL}")));
    assert!(!default.contains("Custom English"));

    for body in ["", "Instructions only", "Restarted {{question}}"] {
        std::fs::write(fixture.0.join("acp/prompt.txt"), body).unwrap();
        success(fixture.command().output().unwrap());
        let prompt = fixture.prompt();
        if body.contains("{{question}}") {
            assert!(prompt.contains(&format!("Restarted {QUESTION}")));
        } else {
            assert!(prompt.contains(&format!("User question:\n{QUESTION}")));
        }
    }
}

#[test]
fn invalid_template_fails_before_child_spawn_without_echoing_contents() {
    let fixture = Fixture::new();
    for contents in [
        None,
        Some(b"secret {{invalid}}".as_slice()),
        Some(b"\xff".as_slice()),
    ] {
        let path = fixture.0.join("acp/invalid.txt");
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        let output = fixture
            .command()
            .env("ACP_PROMPT_FILE", &path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("ACP_PROMPT_FILE"));
        assert!(error.contains(path.to_str().unwrap()));
        assert!(!error.contains("secret"));
        if contents == Some(b"secret {{invalid}}".as_slice()) {
            assert!(error.contains("byte offset 7"));
        }
        assert!(!fixture.0.join("child.env").exists());
    }
}

#[test]
fn claude_endpoint_precedence_preserves_null_empty_and_verbatim_values() {
    let fixture = Fixture::new();
    for explicit in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!(" explicit endpoint ")),
    ] {
        for process in [None, Some(""), Some("https://process.example/prefix/")] {
            for file in [None, Some(""), Some("https://file.example/prefix/")] {
                let mut agent_env = json!({"KEEP_AGENT_VALUE": "kept"});
                if let Some(explicit) = &explicit {
                    agent_env["ANTHROPIC_BASE_URL"] = explicit.clone();
                }
                let file_line = file
                    .map(|value| format!("ANTHROPIC_BASE_URL={value}\n"))
                    .unwrap_or_default();
                fixture.configure("claude", agent_env, &file_line);
                let mut command = fixture.command();
                if let Some(process) = process {
                    command.env("ANTHROPIC_BASE_URL", process);
                }
                command.env("PROCESS_ONLY", "inherited");
                success(command.output().unwrap());
                let observed = fixture.environment();
                let expected = match &explicit {
                    Some(value) => value.as_str(),
                    None => process.or(file),
                };
                assert_eq!(
                    observed.get("ANTHROPIC_BASE_URL").map(String::as_str),
                    expected
                );
                assert_eq!(observed["KEEP_AGENT_VALUE"], "kept");
                assert_eq!(observed["PROCESS_ONLY"], "inherited");
                for secret in [
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "UNRELATED_SECRET",
                    "MM_OWNER1_TOKEN",
                ] {
                    assert!(!observed.contains_key(secret));
                }
            }
        }
    }
}

#[test]
fn endpoint_fallback_is_claude_only_and_does_not_validate_urls() {
    let fixture = Fixture::new();
    fixture.configure(
        "claude",
        json!({}),
        "ANTHROPIC_BASE_URL=' opaque endpoint '\n",
    );
    success(fixture.command().output().unwrap());
    assert_eq!(
        fixture.environment()["ANTHROPIC_BASE_URL"],
        " opaque endpoint "
    );

    fixture.configure(
        "other",
        json!({}),
        "ANTHROPIC_BASE_URL=https://file.example\n",
    );
    success(fixture.command().output().unwrap());
    assert!(!fixture.environment().contains_key("ANTHROPIC_BASE_URL"));
    success(
        fixture
            .command()
            .env("ANTHROPIC_BASE_URL", "inherited")
            .output()
            .unwrap(),
    );
    assert_eq!(fixture.environment()["ANTHROPIC_BASE_URL"], "inherited");

    fixture.configure(
        "other",
        json!({"ANTHROPIC_BASE_URL": "explicit"}),
        "ANTHROPIC_BASE_URL=https://file.example\n",
    );
    success(fixture.command().output().unwrap());
    assert_eq!(fixture.environment()["ANTHROPIC_BASE_URL"], "explicit");
}

#[test]
fn no_argument_smoke_keeps_its_fixed_prompt() {
    let fixture = Fixture::new();
    fixture.configure("claude", json!({}), "ACP_PROMPT_FILE=acp/prompt.txt\n");
    std::fs::write(fixture.0.join("acp/prompt.txt"), "Custom {{question}}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mattermost-acp-assist"))
        .arg("smoke")
        .current_dir(&fixture.0)
        .env_clear()
        .env("ACP_ASSIST_HOME", &fixture.0)
        .env("ACP_ASSIST_ENV", "settings/test.env")
        .output()
        .unwrap();
    success(output);
    assert_eq!(fixture.prompt(), "acp smoke prompt");
}

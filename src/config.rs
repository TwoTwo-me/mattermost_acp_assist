use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub struct AgentSpec {
    pub command: Vec<String>,
    pub env: HashMap<String, Option<String>>,
    pub session_meta: Option<Value>,
    pub model: Option<String>,
    pub mode: Option<String>,
}

pub struct RegistryEntry {
    pub agent: String,
    pub cwd: PathBuf,
}

pub struct Owner {
    pub name: String,
    pub token: String,
    pub user_id: String,
}

pub struct Config {
    pub config_home: PathBuf,
    pub env_file: PathBuf,
    pub mm_url: String,
    pub mm_team: String,
    pub monitor_channel_ids: HashSet<String>,
    pub reader_token: Option<String>,
    pub trigger_emojis: HashSet<String>,
    pub owners: Vec<Owner>,
    pub agents: Vec<(String, AgentSpec)>,
    pub registry: HashMap<String, RegistryEntry>,
    pub state_file: PathBuf,
    pub response_language: String,
}

impl Config {
    pub fn agent(&self, name: &str) -> Option<&AgentSpec> {
        self.agents
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, spec)| spec)
    }

    pub fn owner(&self, name: &str) -> Option<&Owner> {
        self.owners.iter().find(|owner| owner.name == name)
    }
}

fn has_config(dir: &Path) -> bool {
    dir.join("acp").join("agents.json").is_file()
}

pub fn config_home() -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("ACP_ASSIST_HOME").filter(|value| !value.is_empty())
    {
        let path = PathBuf::from(configured);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .context("could not read current directory")?
                .join(path)
        };
        if has_config(&path) {
            return Ok(path);
        }
        bail!("ACP_ASSIST_HOME {} has no acp/agents.json", path.display());
    }
    let cwd = std::env::current_dir().context("could not read current directory")?;
    if let Some(dir) = cwd.ancestors().find(|dir| has_config(dir)) {
        return Ok(dir.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent().filter(|dir| has_config(dir)) {
            return Ok(dir.to_path_buf());
        }
    }
    bail!(
        "no acp/agents.json found above {} and ACP_ASSIST_HOME is not set",
        cwd.display()
    )
}

fn parse_dot_env(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for raw_line in text.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        if eq == 0 {
            continue;
        }
        let mut key = line[..eq].trim();
        let export_rest = key
            .strip_prefix("export")
            .filter(|rest| rest.chars().next().is_some_and(char::is_whitespace));
        if let Some(rest) = export_rest {
            key = rest.trim_start();
        }
        let mut value = line[eq + 1..].trim();
        if value.len() > 1 {
            let bytes = value.as_bytes();
            let quoted = (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'');
            if quoted {
                value = &value[1..value.len() - 1];
            }
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

fn read_dot_env(root: &Path) -> (PathBuf, HashMap<String, String>) {
    let path = std::env::var("ACP_ASSIST_ENV")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| {
            let path = Path::new(&value);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| root.join(".env"));
    let values = std::fs::read_to_string(&path)
        .map(|text| parse_dot_env(&text))
        .unwrap_or_default();
    (path, values)
}

fn env_value(file_env: &HashMap<String, String>, key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .or_else(|| file_env.get(key).cloned())
}

fn required(file_env: &HashMap<String, String>, key: &str) -> Result<String> {
    match env_value(file_env, key) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => bail!("missing required environment value {key}"),
    }
}

fn read_json(file: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| anyhow!("could not read {}: {error}", file.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| anyhow!("could not read {}: {error}", file.display()))
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn load_agents(
    root: &Path,
    file_env: &HashMap<String, String>,
) -> Result<Vec<(String, AgentSpec)>> {
    let file = root.join("acp").join("agents.json");
    let raw = read_json(&file)?;
    let Value::Object(entries) = raw else {
        bail!("{} must map agent names to spawn configs", file.display());
    };
    let mut extra_args =
        parse_claude_extra_args(env_value(file_env, "ACP_CLAUDE_EXTRA_ARGS").as_deref())?;
    if extra_args.is_some() && !entries.contains_key("claude") {
        bail!("ACP_CLAUDE_EXTRA_ARGS requires an agent named claude");
    }
    let effort = parse_claude_effort(env_value(file_env, "ACP_CLAUDE_EFFORT").as_deref())?;
    if effort.is_some() && !entries.contains_key("claude") {
        bail!("ACP_CLAUDE_EFFORT requires an agent named claude");
    }
    let mut agents = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        let Value::Object(spec) = value else {
            bail!(
                "{}: agent \"{name}\" needs a non-empty command array",
                file.display()
            );
        };
        let command = match spec.get("command") {
            Some(Value::Array(items)) if !items.is_empty() => {
                items.iter().map(js_string).collect::<Vec<_>>()
            }
            _ => bail!(
                "{}: agent \"{name}\" needs a non-empty command array",
                file.display()
            ),
        };
        let mut command = command;
        if command.first().map(String::as_str) == Some("self") {
            command[0] = std::env::current_exe()
                .context("could not resolve the current executable")?
                .to_string_lossy()
                .into_owned();
        } else if let Some(first) = command.first_mut() {
            if first.contains('/') && Path::new(first).is_relative() {
                *first = root.join(&*first).to_string_lossy().into_owned();
            }
        }
        let mut spawn_env = HashMap::new();
        if let Some(Value::Object(vars)) = spec.get("env") {
            for (key, entry) in vars {
                match entry {
                    Value::Null => {
                        spawn_env.insert(key.clone(), None);
                    }
                    Value::String(text) => {
                        spawn_env.insert(key.clone(), Some(text.clone()));
                    }
                    _ => bail!(
                        "{}: agent \"{name}\" env \"{key}\" must be a string or null",
                        file.display()
                    ),
                }
            }
        }
        let uppercase_name = name.to_uppercase();
        let model_key = format!("ACP_{uppercase_name}_MODEL");
        let model = parse_agent_selection(
            spec.get("model"),
            env_value(file_env, &model_key).as_deref(),
        )
        .with_context(|| format!("agent \"{name}\" has invalid model configuration"))?;
        let mode_key = format!("ACP_{uppercase_name}_MODE");
        let mode =
            parse_agent_selection(spec.get("mode"), env_value(file_env, &mode_key).as_deref())
                .with_context(|| format!("agent \"{name}\" has invalid mode configuration"))?;
        let mut session_meta = spec.get("sessionMeta").cloned();
        if name == "claude" {
            if let Some(extra_args) = extra_args.take() {
                merge_claude_extra_args(&mut session_meta, extra_args)?;
            }
            apply_claude_effort(&mut session_meta, effort)?;
        }
        agents.push((
            name,
            AgentSpec {
                command,
                env: spawn_env,
                session_meta,
                model,
                mode,
            },
        ));
    }
    Ok(agents)
}

fn parse_claude_extra_args(value: Option<&str>) -> Result<Option<Map<String, Value>>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(value)
        .map_err(|_| anyhow!("ACP_CLAUDE_EXTRA_ARGS must be a JSON object"))?;
    let Value::Object(arguments) = value else {
        bail!("ACP_CLAUDE_EXTRA_ARGS must be a JSON object");
    };
    validate_claude_extra_args(&arguments)?;
    Ok(Some(arguments))
}

fn validate_claude_extra_args(arguments: &Map<String, Value>) -> Result<()> {
    for (flag, value) in arguments {
        if !flag
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            || !flag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !matches!(value, Value::String(_) | Value::Null)
        {
            bail!("Claude extraArgs requires flag names starting with a letter or digit, containing only ASCII letters, digits or hyphens, and string or null values");
        }
    }
    Ok(())
}

fn merge_claude_extra_args(
    session_meta: &mut Option<Value>,
    arguments: Map<String, Value>,
) -> Result<()> {
    if arguments.is_empty() {
        return Ok(());
    }
    let container = claude_options_mut(session_meta)?
        .entry("extraArgs")
        .or_insert_with(|| Value::Object(Map::new()));
    let existing = container
        .as_object_mut()
        .context("Claude sessionMeta extraArgs must be a JSON object")?;
    validate_claude_extra_args(existing)?;
    existing.extend(arguments);
    Ok(())
}

fn claude_options_mut(session_meta: &mut Option<Value>) -> Result<&mut Map<String, Value>> {
    let mut container = session_meta.get_or_insert_with(|| Value::Object(Map::new()));
    for key in ["claudeCode", "options"] {
        container = container
            .as_object_mut()
            .context("Claude sessionMeta, claudeCode and options must be JSON objects")?
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()));
    }
    container
        .as_object_mut()
        .context("Claude sessionMeta options must be a JSON object")
}

fn parse_claude_effort(value: Option<&str>) -> Result<Option<&'static str>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.chars().any(char::is_control) {
        bail!("Claude effort must be low, medium, high or max");
    }
    match value.trim() {
        "" => Ok(None),
        "low" => Ok(Some("low")),
        "medium" => Ok(Some("medium")),
        "high" => Ok(Some("high")),
        "max" => Ok(Some("max")),
        _ => bail!("Claude effort must be low, medium, high or max"),
    }
}

fn apply_claude_effort(session_meta: &mut Option<Value>, effort: Option<&str>) -> Result<()> {
    let mut configured = session_meta.as_ref();
    for key in ["claudeCode", "options"] {
        configured = match configured {
            None => None,
            Some(value) => value
                .as_object()
                .context("Claude sessionMeta, claudeCode and options must be JSON objects")?
                .get(key),
        };
    }
    if let Some(options) = configured {
        if let Some(value) = options
            .as_object()
            .context("Claude sessionMeta options must be a JSON object")?
            .get("effort")
        {
            let value = value
                .as_str()
                .context("configured Claude effort must be a non-empty string")?;
            if parse_claude_effort(Some(value))?.is_none() {
                bail!("configured Claude effort must be a non-empty string");
            }
        }
    }
    if let Some(effort) = effort {
        claude_options_mut(session_meta)?
            .insert("effort".into(), Value::String(effort.to_string()));
    }
    Ok(())
}

fn parse_agent_selection(
    configured: Option<&Value>,
    override_value: Option<&str>,
) -> Result<Option<String>> {
    let configured = match configured {
        None => None,
        Some(Value::String(value))
            if !value.trim().is_empty() && !value.chars().any(char::is_control) =>
        {
            Some(value.trim())
        }
        Some(_) => bail!("agent selection must be a non-empty, single-line string"),
    };
    match override_value {
        Some(value) if value.chars().any(char::is_control) => {
            bail!("agent selection override must be a single-line string")
        }
        Some(value) => Ok((!value.trim().is_empty()).then(|| value.trim().to_string())),
        None => Ok(configured.map(str::to_string)),
    }
}

fn load_registry(
    root: &Path,
    workspace: &Path,
    file_env: &HashMap<String, String>,
    agents: &[(String, AgentSpec)],
) -> Result<HashMap<String, RegistryEntry>> {
    let configured =
        env_value(file_env, "ACP_REGISTRY").unwrap_or_else(|| "acp/registry.json".to_string());
    let path = Path::new(&configured);
    let file = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let raw = read_json(&file)?;
    let Value::Object(entries) = raw else {
        bail!(
            "{} must map Mattermost usernames to registry entries",
            file.display()
        );
    };
    let mut registry = HashMap::with_capacity(entries.len());
    for (username, value) in entries {
        let Value::Object(entry) = value else {
            bail!(
                "{}: \"{username}\" needs agent and cwd strings",
                file.display()
            );
        };
        let Some(agent) = entry.get("agent").and_then(Value::as_str) else {
            bail!("{}: \"{username}\" needs an agent string", file.display());
        };
        if !agents.iter().any(|(name, _)| name == agent) {
            bail!(
                "{}: \"{username}\" names unknown agent \"{agent}\"",
                file.display()
            );
        }
        let cwd = match entry.get("cwd").and_then(Value::as_str).map(str::trim) {
            None | Some("") | Some(".") => workspace.to_path_buf(),
            Some(value) => {
                let path = Path::new(value);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                }
            }
        };
        registry.insert(
            username,
            RegistryEntry {
                agent: agent.to_string(),
                cwd,
            },
        );
    }
    Ok(registry)
}

fn owner_key(key: &str, suffix: &str) -> Option<String> {
    let middle = key.strip_prefix("MM_OWNER")?.strip_suffix(suffix)?;
    if middle.is_empty() {
        return None;
    }
    Some(format!("owner{}", middle.to_lowercase()))
}

fn load_owners(file_env: &HashMap<String, String>) -> Result<Vec<Owner>> {
    let mut merged = file_env.clone();
    merged.extend(std::env::vars());
    let mut tokens = HashMap::new();
    let mut user_ids = HashMap::new();
    for (key, value) in merged {
        if value.is_empty() {
            continue;
        }
        if let Some(name) = owner_key(&key, "_TOKEN") {
            tokens.insert(name, value);
        } else if let Some(name) = owner_key(&key, "_USER_ID") {
            user_ids.insert(name, value);
        }
    }
    let mut names: Vec<String> = tokens
        .keys()
        .chain(user_ids.keys())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    names.sort();
    let mut owners = Vec::with_capacity(names.len());
    for name in names {
        let key = name.to_uppercase();
        let Some(token) = tokens.remove(&name) else {
            bail!("{name} has a user id but no MM_{key}_TOKEN");
        };
        let Some(user_id) = user_ids.remove(&name) else {
            bail!("{name} has a token but no MM_{key}_USER_ID");
        };
        owners.push(Owner {
            name,
            token,
            user_id,
        });
    }
    if owners.is_empty() {
        bail!("no owners configured; set MM_OWNER1_TOKEN and MM_OWNER1_USER_ID");
    }
    Ok(owners)
}

fn load_trigger_emojis(file_env: &HashMap<String, String>) -> Result<HashSet<String>> {
    let configured = required(file_env, "TRIGGER_EMOJI")?;
    let emojis: HashSet<String> = configured
        .split(',')
        .map(str::trim)
        .filter(|emoji| !emoji.is_empty())
        .map(str::to_string)
        .collect();
    if emojis.is_empty() {
        bail!("TRIGGER_EMOJI has no emoji names");
    }
    Ok(emojis)
}

fn load_monitor_channel_ids(file_env: &HashMap<String, String>) -> Result<HashSet<String>> {
    let configured = env_value(file_env, "MM_MONITOR_CHANNEL_IDS")
        .or_else(|| env_value(file_env, "MM_CHANNEL_ID"))
        .unwrap_or_default();
    let ids: HashSet<String> = configured
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        bail!("no monitor channels; set MM_MONITOR_CHANNEL_IDS or MM_CHANNEL_ID");
    }
    Ok(ids)
}

fn load_config() -> Result<Config> {
    let root = config_home()?;
    let workspace = std::env::current_dir().context("could not read current directory")?;
    let (env_file, file_env) = read_dot_env(&root);
    let agents = load_agents(&root, &file_env)?;
    let owners = load_owners(&file_env)?;
    let mm_url = required(&file_env, "MM_URL")?
        .trim_end_matches('/')
        .to_string();
    let mm_team = required(&file_env, "MM_TEAM")?;
    let monitor_channel_ids = load_monitor_channel_ids(&file_env)?;
    let reader_token = env_value(&file_env, "MM_READER_TOKEN").filter(|value| !value.is_empty());
    let trigger_emojis = load_trigger_emojis(&file_env)?;
    let registry = load_registry(&root, &workspace, &file_env, &agents)?;
    let response_language =
        parse_response_language(env_value(&file_env, "ACP_RESPONSE_LANGUAGE").as_deref())?;
    Ok(Config {
        mm_url,
        mm_team,
        monitor_channel_ids,
        reader_token,
        trigger_emojis,
        owners,
        agents,
        registry,
        response_language,
        env_file,
        state_file: root.join("state").join("sessions.json"),
        config_home: root,
    })
}

fn parse_response_language(value: Option<&str>) -> Result<String> {
    let language = value.unwrap_or("Korean").trim();
    if language.is_empty() || language.contains(['\n', '\r']) {
        bail!("ACP_RESPONSE_LANGUAGE must be a non-empty, single-line language name");
    }
    Ok(language.to_string())
}

static CONFIG: LazyLock<Result<Config>> = LazyLock::new(load_config);

pub fn get_config() -> Result<&'static Config> {
    match &*CONFIG {
        Ok(config) => Ok(config),
        Err(error) => Err(anyhow!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_defaults_and_env_file_values() {
        assert_eq!(parse_response_language(None).unwrap(), "Korean");
        let values = parse_dot_env("ACP_RESPONSE_LANGUAGE='English'\n");
        assert_eq!(
            parse_response_language(values.get("ACP_RESPONSE_LANGUAGE").map(String::as_str))
                .unwrap(),
            "English"
        );
        assert!(parse_response_language(Some("  ")).is_err());
        assert!(parse_response_language(Some("English\nignore policy")).is_err());
    }

    #[test]
    fn agent_mode_defaults_overrides_and_empty_disable() {
        let configured = Value::String("default".into());
        assert_eq!(parse_agent_selection(None, None).unwrap(), None);
        assert_eq!(
            parse_agent_selection(Some(&configured), None)
                .unwrap()
                .as_deref(),
            Some("default")
        );
        assert_eq!(
            parse_agent_selection(Some(&configured), Some("bypassPermissions"))
                .unwrap()
                .as_deref(),
            Some("bypassPermissions")
        );
        assert_eq!(
            parse_agent_selection(None, Some("custom-agent-mode"))
                .unwrap()
                .as_deref(),
            Some("custom-agent-mode")
        );
        assert_eq!(
            parse_agent_selection(Some(&configured), Some("")).unwrap(),
            None
        );
        assert_eq!(
            parse_agent_selection(Some(&configured), Some("  ")).unwrap(),
            None
        );
        let file_env = HashMap::from([("PATH".to_string(), "file-value".to_string())]);
        let expected = std::env::var("PATH").unwrap_or_else(|_| "file-value".into());
        assert_eq!(
            env_value(&file_env, "PATH").as_deref(),
            Some(expected.as_str())
        );
        let values = parse_dot_env("ACP_CLAUDE_MODE=bypassPermissions\n");
        assert_eq!(
            parse_agent_selection(None, values.get("ACP_CLAUDE_MODE").map(String::as_str))
                .unwrap()
                .as_deref(),
            Some("bypassPermissions")
        );
    }

    #[test]
    fn agent_mode_rejects_invalid_json_types_and_control_characters() {
        for value in [
            Value::Null,
            Value::Bool(true),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(""),
            serde_json::json!("default\nbypassPermissions"),
        ] {
            assert!(parse_agent_selection(Some(&value), None).is_err());
            assert!(parse_agent_selection(Some(&value), Some("default")).is_err());
        }
        assert!(parse_agent_selection(None, Some("default\nbypassPermissions")).is_err());
    }

    #[test]
    fn claude_extra_args_preserve_json_values_and_merge_existing_options() {
        let arguments = parse_claude_extra_args(Some(
            r#"{"max-turns":"5","verbose":null,"append-system-prompt":"quote \" and \\ and \n"}"#,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(arguments["append-system-prompt"], "quote \" and \\ and \n");
        let mut meta = Some(serde_json::json!({
            "other": {"keep": true},
            "claudeCode": {"options": {
                "effort": "medium",
                "extraArgs": {"max-turns": "2", "another-flag": "kept"},
            }},
        }));
        merge_claude_extra_args(&mut meta, arguments).unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta["other"]["keep"], true);
        assert_eq!(meta["claudeCode"]["options"]["effort"], "medium");
        assert_eq!(meta["claudeCode"]["options"]["extraArgs"]["max-turns"], "5");
        assert_eq!(
            meta["claudeCode"]["options"]["extraArgs"]["another-flag"],
            "kept"
        );
        assert!(meta["claudeCode"]["options"]["extraArgs"]["verbose"].is_null());
    }

    #[test]
    fn claude_extra_args_unset_or_empty_do_not_change_metadata() {
        assert!(parse_claude_extra_args(None).unwrap().is_none());
        assert!(parse_claude_extra_args(Some("  ")).unwrap().is_none());
        let mut meta = None;
        merge_claude_extra_args(
            &mut meta,
            parse_claude_extra_args(Some("{}")).unwrap().unwrap(),
        )
        .unwrap();
        assert!(meta.is_none());
        merge_claude_extra_args(
            &mut meta,
            parse_claude_extra_args(Some(r#"{"max-turns":"5"}"#))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            meta.unwrap()["claudeCode"]["options"]["extraArgs"]["max-turns"],
            "5"
        );
    }

    #[test]
    fn claude_extra_args_reject_invalid_types_names_and_containers_without_echoing_values() {
        for input in [
            "not-json-secret",
            "[]",
            "null",
            r#"{"flag":true}"#,
            r#"{"flag":5}"#,
            r#"{"flag":[]}"#,
            r#"{"--flag":"secret"}"#,
            r#"{"":"secret"}"#,
            r#"{"bad flag":"secret"}"#,
            r#"{"bad\nflag":"secret"}"#,
        ] {
            let error = parse_claude_extra_args(Some(input))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
        }
        for malformed in [
            serde_json::json!(null),
            serde_json::json!({"claudeCode": []}),
            serde_json::json!({"claudeCode": {"options": false}}),
            serde_json::json!({"claudeCode": {"options": {"extraArgs": []}}}),
            serde_json::json!({"claudeCode": {"options": {"extraArgs": {"flag": true}}}}),
        ] {
            let mut meta = Some(malformed);
            let arguments = parse_claude_extra_args(Some(r#"{"max-turns":"5"}"#))
                .unwrap()
                .unwrap();
            assert!(merge_claude_extra_args(&mut meta, arguments).is_err());
        }
    }

    #[test]
    fn model_selection_defaults_overrides_and_empty_disable() {
        let configured = serde_json::json!("haiku");
        assert_eq!(parse_agent_selection(None, None).unwrap(), None);
        assert_eq!(
            parse_agent_selection(Some(&configured), None)
                .unwrap()
                .as_deref(),
            Some("haiku")
        );
        let file_env = parse_dot_env("ACP_CLAUDE_MODEL=sonnet\n");
        assert_eq!(
            parse_agent_selection(
                Some(&configured),
                file_env.get("ACP_CLAUDE_MODEL").map(String::as_str)
            )
            .unwrap()
            .as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            parse_agent_selection(Some(&configured), Some("")).unwrap(),
            None
        );
        assert!(parse_agent_selection(Some(&configured), Some(" \t")).is_err());
        for value in [
            Value::Null,
            serde_json::json!(5),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(""),
            serde_json::json!("haiku\nsecret"),
        ] {
            assert!(parse_agent_selection(Some(&value), None).is_err());
            assert!(parse_agent_selection(Some(&value), Some("sonnet")).is_err());
        }
        let error = parse_agent_selection(None, Some("haiku\nsecret"))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
    }

    #[test]
    fn claude_effort_accepts_pinned_sdk_levels_and_preserves_blank_defaults() {
        assert_eq!(parse_claude_effort(None).unwrap(), None);
        assert_eq!(parse_claude_effort(Some("  ")).unwrap(), None);
        for level in ["low", "medium", "high", "max"] {
            assert_eq!(parse_claude_effort(Some(level)).unwrap(), Some(level));
        }
        for invalid in ["xhigh", "ultra", "MAX", "high\nsecret", "\tmedium"] {
            let error = parse_claude_effort(Some(invalid)).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
        let mut metadata = Some(serde_json::json!({
            "other": {"keep": true},
            "claudeCode": {"options": {
                "effort": "medium",
                "extraArgs": {"max-turns": "5"},
            }},
        }));
        let original = metadata.clone();
        apply_claude_effort(&mut metadata, None).unwrap();
        assert_eq!(metadata, original);
        apply_claude_effort(&mut metadata, parse_claude_effort(Some("")).unwrap()).unwrap();
        assert_eq!(metadata, original);
        let file_env = parse_dot_env("ACP_CLAUDE_EFFORT=high\n");
        apply_claude_effort(
            &mut metadata,
            parse_claude_effort(file_env.get("ACP_CLAUDE_EFFORT").map(String::as_str)).unwrap(),
        )
        .unwrap();
        let metadata = metadata.unwrap();
        assert_eq!(metadata["claudeCode"]["options"]["effort"], "high");
        assert_eq!(
            metadata["claudeCode"]["options"]["extraArgs"]["max-turns"],
            "5"
        );
        assert_eq!(metadata["other"]["keep"], true);
        let mut empty = None;
        apply_claude_effort(&mut empty, None).unwrap();
        assert!(empty.is_none());
        apply_claude_effort(&mut empty, Some("low")).unwrap();
        assert_eq!(empty.unwrap()["claudeCode"]["options"]["effort"], "low");
    }

    #[test]
    fn claude_effort_rejects_invalid_metadata_without_echoing_values() {
        for invalid in [
            serde_json::json!(null),
            serde_json::json!({"claudeCode": []}),
            serde_json::json!({"claudeCode": {"options": false}}),
            serde_json::json!({"claudeCode": {"options": {"effort": true}}}),
            serde_json::json!({"claudeCode": {"options": {"effort": 5}}}),
            serde_json::json!({"claudeCode": {"options": {"effort": null}}}),
            serde_json::json!({"claudeCode": {"options": {"effort": ""}}}),
            serde_json::json!({"claudeCode": {"options": {"effort": "secret-unsupported"}}}),
        ] {
            let mut metadata = Some(invalid);
            let error = apply_claude_effort(&mut metadata, Some("high"))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-unsupported"));
            assert!(apply_claude_effort(&mut metadata, None).is_err());
        }
    }
}

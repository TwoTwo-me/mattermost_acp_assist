use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;

use crate::acp::AcpClient;
use crate::config::{get_config, Config, Owner};
use crate::mm::{json_field, MattermostSocket, MmEvent, MmPost, MmReaction};
use crate::prompt::{refresh_command, PromptContext, INITIAL_QUESTION};
use crate::scratch::ScratchDir;
use crate::self_dm::SelfDm;
use crate::store::{SessionRecord, SessionStatus, SessionStore};

fn session_key(owner: &str, post_id: &str) -> String {
    format!("{owner}:{post_id}")
}

struct Runner {
    client: AcpClient,
    scratch: Mutex<ScratchDir>,
    permalink: String,
    refresh_command: String,
    queue: AsyncMutex<()>,
    accepting: AtomicBool,
}

impl Runner {
    fn accepts_turn(&self) -> bool {
        self.accepting.load(Ordering::SeqCst)
    }

    async fn close(&self) -> Result<()> {
        self.accepting.store(false, Ordering::SeqCst);
        self.client.close().await?;
        self.scratch
            .lock()
            .expect("scratch mutex")
            .release_after_exit()
    }
}

struct Shared {
    config: &'static Config,
    store: SessionStore,
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    in_flight: Mutex<HashSet<String>>,
    handled_posts: Mutex<HashSet<String>>,
    closing: AtomicBool,
    tasks: Mutex<JoinSet<()>>,
}

impl Shared {
    fn open(config: &'static Config) -> Result<Self> {
        Ok(Self {
            config,
            store: SessionStore::new(&config.state_file)?,
            runners: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            handled_posts: Mutex::new(HashSet::new()),
            closing: AtomicBool::new(false),
            tasks: Mutex::new(JoinSet::new()),
        })
    }
}

pub struct Listener {
    socket: MattermostSocket,
    shared: Arc<Shared>,
}

impl Listener {
    pub async fn close(mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.socket.close().await;
        let mut tasks = {
            let mut tasks = self.shared.tasks.lock().expect("tasks mutex");
            std::mem::take(&mut *tasks)
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        let runners: Vec<(String, Arc<Runner>)> = self
            .shared
            .runners
            .lock()
            .expect("runners mutex")
            .iter()
            .map(|(key, runner)| (key.clone(), runner.clone()))
            .collect();
        for (key, runner) in runners {
            finish_session(
                &self.shared,
                &key,
                &runner,
                SessionStatus::Closed,
                "Listener stopped",
            )
            .await;
        }
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let config = get_config()?;
    let selected = parse_as(args)?;
    let shared = Arc::new(Shared::open(config)?);
    let owners: Vec<&'static Owner> = match selected {
        Some(name) => vec![config
            .owner(&name)
            .ok_or_else(|| anyhow!("unknown owner \"{name}\""))?],
        None => config.owners.iter().collect(),
    };

    let mut listeners = Vec::with_capacity(owners.len());
    for owner in owners {
        match start_with(shared.clone(), owner).await {
            Ok(listener) => listeners.push(listener),
            Err(error) => {
                for listener in listeners {
                    listener.close().await;
                }
                return Err(error);
            }
        }
    }
    wait_for_shutdown().await;
    for listener in listeners {
        listener.close().await;
    }
    Ok(())
}

pub async fn start_listener(owner_name: &str) -> Result<Listener> {
    let config = get_config()?;
    let owner = config
        .owner(owner_name)
        .ok_or_else(|| anyhow!("unknown owner \"{owner_name}\""))?;
    let shared = Arc::new(Shared::open(config)?);
    start_with(shared, owner).await
}

async fn start_with(shared: Arc<Shared>, owner: &'static Owner) -> Result<Listener> {
    let config = shared.config;
    let dm = Arc::new(SelfDm::open(&config.mm_url, owner).await?);
    let handler_shared = shared.clone();
    let handler_dm = dm.clone();
    let socket = MattermostSocket::connect(
        &config.mm_url,
        &owner.token,
        &owner.name,
        Arc::new(move |event| {
            let shared = handler_shared.clone();
            let dm = handler_dm.clone();
            let mut tasks = shared.tasks.lock().expect("tasks mutex");
            if shared.closing.load(Ordering::SeqCst) {
                return;
            }
            while tasks.try_join_next().is_some() {}
            let task_shared = shared.clone();
            tasks.spawn(async move { dispatch(task_shared, dm, event).await });
        }),
    )
    .await?;
    let maintenance_shared = shared.clone();
    let owner_name = owner.name.clone();
    shared.tasks.lock().expect("tasks mutex").spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            finish_exited_sessions(&maintenance_shared, &owner_name).await;
        }
    });

    println!(
        "listening as {} ({}) on {} monitor channel(s) at {}",
        owner.name,
        owner.user_id,
        config.monitor_channel_ids.len(),
        config.mm_url
    );

    Ok(Listener { socket, shared })
}

async fn dispatch(shared: Arc<Shared>, dm: Arc<SelfDm>, event: MmEvent) {
    if shared.closing.load(Ordering::SeqCst) {
        return;
    }
    match event.event.as_str() {
        "reaction_added" => handle_reaction(shared, dm, event).await,
        "posted" => handle_posted(shared, dm, event).await,
        _ => {}
    }
}

async fn handle_reaction(shared: Arc<Shared>, dm: Arc<SelfDm>, event: MmEvent) {
    let config = shared.config;
    let Some(reaction) = event
        .data
        .get("reaction")
        .and_then(json_field::<MmReaction>)
    else {
        return;
    };
    if reaction.user_id != dm.user_id() {
        return;
    }
    if !config.trigger_emojis.contains(&reaction.emoji_name) {
        return;
    }
    if let Some(channel_id) = reaction.channel_id.as_deref().filter(|id| !id.is_empty()) {
        if !config.monitor_channel_ids.contains(channel_id) {
            return;
        }
    }
    let post_id = reaction.post_id.clone();
    let key = session_key(dm.owner_name(), &post_id);
    if post_id.is_empty()
        || shared
            .runners
            .lock()
            .expect("runners mutex")
            .contains_key(&key)
    {
        return;
    }
    {
        let mut in_flight = shared.in_flight.lock().expect("in-flight mutex");
        if !in_flight.insert(key.clone()) {
            return;
        }
    }

    let outcome = open_session(&shared, &dm, &key, &post_id).await;
    if let Err(error) = outcome {
        eprintln!("reaction on {post_id} failed: {error}");
    }
    shared
        .in_flight
        .lock()
        .expect("in-flight mutex")
        .remove(&key);
}

async fn open_session(
    shared: &Arc<Shared>,
    dm: &Arc<SelfDm>,
    key: &str,
    post_id: &str,
) -> Result<()> {
    let config = shared.config;
    let post = dm.post_by_id(post_id).await?;
    if !config.monitor_channel_ids.contains(&post.channel_id) {
        return Ok(());
    }
    if shared
        .runners
        .lock()
        .expect("runners mutex")
        .contains_key(key)
    {
        return Ok(());
    }
    let user = dm.user(dm.user_id()).await?;
    let Some(entry) = config.registry.get(&user.username) else {
        return Ok(());
    };
    let Some(spec) = config.agent(&entry.agent) else {
        return Err(anyhow!("no spawn config for agent \"{}\"", entry.agent));
    };

    let thread = dm
        .thread(post_id)
        .await
        .with_context(|| format!("could not build the prompt for post {post_id}"))?;

    let link = format!("{}/{}/pl/{}", config.mm_url, config.mm_team, post.id);
    let refresh = refresh_command(
        &std::env::current_exe()?,
        &config.config_home,
        &config.env_file,
        dm.owner_name(),
        post_id,
    )?;
    let mut scratch = ScratchDir::create()?;
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let prompt = PromptContext {
        language: &config.response_language,
        permalink: &link,
        thread: Some(&thread),
        question: INITIAL_QUESTION,
        refresh_command: Some(&refresh),
        scratch_path: &scratch_path,
    }
    .render();
    let cwd = entry.cwd.to_string_lossy().into_owned();
    let root_body = [
        "ACP session".to_string(),
        format!("agent: {}", entry.agent),
        format!("cwd: {cwd}"),
        format!("scratch: {scratch_path}"),
        format!("permalink: {link}"),
        format!("excerpt: {}", excerpt(&post.message, 200)),
        format!("instruction: {INITIAL_QUESTION}"),
    ]
    .join("\n");
    let root = dm.post(&root_body, None).await?;
    let record = SessionRecord {
        session_key: key.to_string(),
        post_id: post.id.clone(),
        status: SessionStatus::Starting,
        acp_session_id: None,
        dm_channel_id: dm.channel_id().to_string(),
        dm_root_post_id: root.id.clone(),
        agent: entry.agent.clone(),
        cwd: cwd.clone(),
        error: None,
        updated_at: 0,
    };
    shared.store.put(record)?;
    scratch.mark_in_use()?;

    let client = match AcpClient::spawn(&entry.agent, spec, &cwd).await {
        Ok(client) => client,
        Err(error) => {
            scratch.release_after_exit()?;
            shared.store.update(key, |record| {
                record.status = SessionStatus::Failed;
                record.error = Some(short_error(&error.to_string()));
            });
            let _ = dm.reply_error("ACP agent could not start.", &root.id).await;
            return Err(error);
        }
    };
    let runner = Arc::new(Runner {
        client,
        scratch: Mutex::new(scratch),
        permalink: link,
        refresh_command: refresh,
        queue: AsyncMutex::new(()),
        accepting: AtomicBool::new(true),
    });
    let _queue = runner.queue.try_lock().expect("new runner queue is empty");
    shared
        .runners
        .lock()
        .expect("runners mutex")
        .insert(key.to_string(), runner.clone());

    run_initial_prompt(shared, dm, key, &prompt, &runner).await;
    Ok(())
}

async fn run_initial_prompt(
    shared: &Arc<Shared>,
    dm: &SelfDm,
    key: &str,
    prompt: &str,
    runner: &Arc<Runner>,
) {
    let Some(record) = shared.store.get(key) else {
        return;
    };
    let outcome = async {
        tokio::time::timeout(Duration::from_secs(30), runner.client.initialize())
            .await
            .map_err(|_| anyhow!("ACP initialize timed out"))??;
        let session_id = tokio::time::timeout(
            Duration::from_secs(30),
            runner.client.new_session(&record.cwd),
        )
        .await
        .map_err(|_| anyhow!("ACP session/new timed out"))??;
        let mut active = record.clone();
        active.acp_session_id = Some(session_id.clone());
        active.status = SessionStatus::Active;
        active.error = None;
        shared.store.put(active)?;
        run_prompt(
            dm,
            runner,
            &session_id,
            prompt,
            &record.agent,
            &record.dm_root_post_id,
        )
        .await?;
        Ok(())
    }
    .await;
    if let Err(error) = outcome {
        fail_session(shared, dm, key, runner, &error).await;
    }
}

async fn handle_posted(shared: Arc<Shared>, dm: Arc<SelfDm>, event: MmEvent) {
    let Some(post) = event.data.get("post").and_then(json_field::<MmPost>) else {
        return;
    };
    if post.is_app_post() || post.user_id != dm.user_id() {
        return;
    }
    if post.root_id.is_empty() || post.channel_id != dm.channel_id() {
        return;
    }
    if shared
        .handled_posts
        .lock()
        .expect("handled mutex")
        .contains(&post.id)
    {
        return;
    }
    let Some(record) = shared.store.get_by_dm_root(&post.root_id) else {
        return;
    };
    if matches!(record.status, SessionStatus::Failed | SessionStatus::Closed) {
        let _ = dm.reply_error("This ACP session has ended. Remove and add the trigger reaction on the source post to start again.", &post.root_id).await;
        return;
    }
    shared
        .handled_posts
        .lock()
        .expect("handled mutex")
        .insert(post.id.clone());

    let key = record.session_key.clone();
    let runner = {
        shared
            .runners
            .lock()
            .expect("runners mutex")
            .get(&key)
            .cloned()
    };
    let Some(runner) = runner else {
        eprintln!("no live ACP session for DM thread {}", post.root_id);
        let _ = dm.reply_error("This ACP session is no longer running. Remove and add the trigger reaction on the source post to start again.", &post.root_id).await;
        return;
    };

    if !runner.accepts_turn() {
        let _ = dm
            .reply_error("This ACP session has ended.", &post.root_id)
            .await;
        return;
    }
    {
        let _queue = runner.queue.lock().await;
        continue_prompt(&shared, &dm, &key, &post.root_id, &post.message, &runner).await;
    }
}

async fn continue_prompt(
    shared: &Arc<Shared>,
    dm: &SelfDm,
    key: &str,
    dm_root_id: &str,
    message: &str,
    runner: &Arc<Runner>,
) {
    let Some(record) = shared.store.get(key) else {
        return;
    };
    if record.status != SessionStatus::Active || record.dm_root_post_id != dm_root_id {
        let _ = dm
            .reply_error("This ACP session has ended.", dm_root_id)
            .await;
        return;
    }
    let Some(session_id) = record.acp_session_id.clone() else {
        return;
    };
    let scratch_path = runner
        .scratch
        .lock()
        .expect("scratch mutex")
        .path()
        .to_string_lossy()
        .into_owned();
    let prompt = PromptContext {
        language: &shared.config.response_language,
        permalink: &runner.permalink,
        thread: None,
        question: message,
        refresh_command: Some(&runner.refresh_command),
        scratch_path: &scratch_path,
    }
    .render();
    let outcome = async {
        run_prompt(
            dm,
            runner,
            &session_id,
            &prompt,
            &record.agent,
            &record.dm_root_post_id,
        )
        .await?;
        Ok(())
    }
    .await;
    if let Err(error) = outcome {
        fail_session(shared, dm, key, runner, &error).await;
    }
}

async fn run_prompt(
    dm: &SelfDm,
    runner: &Runner,
    session_id: &str,
    prompt: &str,
    agent: &str,
    root_id: &str,
) -> Result<()> {
    let _ = dm.pending(agent, root_id).await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        runner.client.prompt(session_id, prompt),
    )
    .await
    .map_err(|_| anyhow!("ACP prompt timed out after 300s"))??;
    dm.reply(output_text(&answer.text), root_id, agent).await?;
    Ok(())
}

async fn fail_session(
    shared: &Arc<Shared>,
    dm: &SelfDm,
    key: &str,
    runner: &Runner,
    error: &anyhow::Error,
) {
    let detail = short_error(&error.to_string());
    let Some(record) = shared.store.get(key) else {
        return;
    };
    finish_session(shared, key, runner, SessionStatus::Failed, &detail).await;
    if record.dm_channel_id.is_empty() {
        return;
    }
    let root = if record.dm_root_post_id.is_empty() {
        None
    } else {
        Some(record.dm_root_post_id.as_str())
    };
    let outcome = if let Some(root) = root {
        dm.reply_error(&detail, root).await
    } else {
        dm.post(&format!("ACP session failed: {detail}"), None)
            .await
    };
    if let Err(error) = outcome {
        eprintln!("could not report failure in the self-DM: {error}");
    }
}

async fn finish_session(
    shared: &Shared,
    key: &str,
    runner: &Runner,
    status: SessionStatus,
    reason: &str,
) {
    {
        let runners = shared.runners.lock().expect("runners mutex");
        if !runners
            .get(key)
            .is_some_and(|current| std::ptr::eq(current.as_ref(), runner))
        {
            return;
        }
        runner.accepting.store(false, Ordering::SeqCst);
        if let Some(mut record) = shared.store.get(key) {
            record.status = status;
            record.error = Some(reason.to_string());
            if let Err(error) = shared.store.put(record) {
                eprintln!("could not persist session closure: {error}");
            }
        }
    }
    match runner.close().await {
        Ok(()) => {
            let mut runners = shared.runners.lock().expect("runners mutex");
            if runners
                .get(key)
                .is_some_and(|current| std::ptr::eq(current.as_ref(), runner))
            {
                runners.remove(key);
            }
        }
        Err(error) => eprintln!("session {key} cleanup failed; scratch retained: {error}"),
    }
}

async fn finish_exited_sessions(shared: &Shared, owner: &str) {
    let prefix = format!("{owner}:");
    let runners: Vec<(String, Arc<Runner>)> = shared
        .runners
        .lock()
        .expect("runners mutex")
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(key, runner)| (key.clone(), runner.clone()))
        .collect();
    for (key, runner) in runners {
        if runner.client.has_exited() {
            let _queue = runner.queue.lock().await;
            finish_session(
                shared,
                &key,
                &runner,
                SessionStatus::Failed,
                "ACP agent exited",
            )
            .await;
        }
    }
}

fn parse_as(args: &[String]) -> Result<Option<String>> {
    let mut owner = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--as" => {
                let name = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("--as needs an owner name"))?;
                owner = Some(name.clone());
                index += 2;
            }
            other => bail!("unexpected argument {other}"),
        }
    }
    Ok(owner)
}

fn output_text(answer: &str) -> &str {
    if answer.is_empty() {
        "(no output)"
    } else {
        answer
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    let flat = collapse_whitespace(text).trim().to_string();
    if flat.chars().count() > limit {
        format!("{}...", first_chars(&flat, limit))
    } else {
        flat
    }
}

fn short_error(message: &str) -> String {
    first_chars(&collapse_whitespace(message), 400)
}

fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_whitespace = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_whitespace {
                result.push(' ');
                in_whitespace = true;
            }
        } else {
            result.push(character);
            in_whitespace = false;
        }
    }
    result
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentSpec;

    #[tokio::test]
    async fn failed_spawn_retains_unattached_scratch() {
        let project = ScratchDir::create().unwrap();
        std::fs::write(project.path().join("keep"), "project data").unwrap();
        let scratch = ScratchDir::create().unwrap();
        let path = scratch.path().to_path_buf();
        let spec = AgentSpec {
            command: vec![project
                .path()
                .join("missing-agent")
                .to_string_lossy()
                .into_owned()],
            env: HashMap::new(),
            session_meta: None,
            model: None,
            mode: None,
        };
        assert!(
            AcpClient::spawn("missing", &spec, project.path().to_str().unwrap())
                .await
                .is_err()
        );
        drop(scratch);
        assert!(path.exists());
        assert!(project.path().join("keep").exists());
        std::fs::remove_dir_all(path).unwrap();
        std::fs::remove_dir_all(project.path()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_reaps_process_but_retains_scratch_and_is_idempotent() {
        let mut scratch = ScratchDir::create().unwrap();
        let path = scratch.path().to_path_buf();
        let spec = AgentSpec {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "trap '' TERM; printf ready > ready; while read line; do :; done".into(),
            ],
            env: HashMap::new(),
            session_meta: None,
            model: None,
            mode: None,
        };
        scratch.mark_in_use().unwrap();
        let client = AcpClient::spawn("test", &spec, path.to_str().unwrap())
            .await
            .unwrap();
        let runner = Runner {
            client,
            scratch: Mutex::new(scratch),
            permalink: "source".into(),
            refresh_command: "read-source".into(),
            queue: AsyncMutex::new(()),
            accepting: AtomicBool::new(true),
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !path.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(path.join("note"), "same session").unwrap();
        assert_eq!(runner.scratch.lock().unwrap().path(), path);
        assert_eq!(
            std::fs::read_to_string(path.join("note")).unwrap(),
            "same session"
        );
        runner.close().await.unwrap();
        assert!(runner.client.has_exited());
        assert!(path.join("note").exists());
        assert!(!runner.accepts_turn());
        runner.close().await.unwrap();
        drop(runner);
        assert!(path.join("note").exists());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn natural_exit_during_initialize_allows_safe_repeated_close() {
        let mut scratch = ScratchDir::create().unwrap();
        let path = scratch.path().to_path_buf();
        let spec = AgentSpec {
            command: vec!["/bin/sh".into(), "-c".into(), "exit 9".into()],
            env: HashMap::new(),
            session_meta: None,
            model: None,
            mode: None,
        };
        scratch.mark_in_use().unwrap();
        let client = AcpClient::spawn("test", &spec, path.to_str().unwrap())
            .await
            .unwrap();
        let runner = Runner {
            client,
            scratch: Mutex::new(scratch),
            permalink: "source".into(),
            refresh_command: "read-source".into(),
            queue: AsyncMutex::new(()),
            accepting: AtomicBool::new(true),
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(3), runner.client.initialize())
                .await
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runner.client.has_exited() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        runner.close().await.unwrap();
        runner.close().await.unwrap();
        assert!(path.exists());
        drop(runner);
        std::fs::remove_dir_all(path).unwrap();
    }
}

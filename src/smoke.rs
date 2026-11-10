use anyhow::{anyhow, bail, Result};
use std::future::Future;
use std::process::exit;

use crate::acp::AcpClient;
use crate::config::get_config;
use crate::prompt::PromptContext;
use crate::scratch::ScratchDir;

pub async fn run(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return web_probe(args).await;
    }
    let config = get_config()?;
    let cwd = config
        .config_home
        .join("workspaces")
        .join("owner1")
        .to_string_lossy()
        .into_owned();
    let prompt_text = "acp smoke prompt";
    let mut failed = false;

    for (name, spec) in &config.agents {
        let client = AcpClient::spawn(name, spec, &cwd).await?;
        let mut session_id = None;
        let mut initialized = false;

        match with_timeout(client.initialize(), 30_000, &format!("{name} initialize")).await {
            Ok(()) => {
                initialized = true;
                println!("{name} INIT OK");
            }
            Err(error) => println!("{name} INIT BLOCKED {error}"),
        }

        if initialized {
            match with_timeout(
                client.new_session(&cwd),
                30_000,
                &format!("{name} session/new"),
            )
            .await
            {
                Ok(id) => {
                    println!("{name} SESSION OK {id}");
                    session_id = Some(id);
                }
                Err(error) => println!("{name} SESSION BLOCKED {error}"),
            }
        } else {
            println!("{name} SESSION BLOCKED initialize did not complete");
        }

        if let Some(session_id) = session_id {
            match with_timeout(
                client.prompt(&session_id, prompt_text),
                60_000,
                &format!("{name} session/prompt"),
            )
            .await
            {
                Ok(reply) => {
                    let shown = serde_json::to_string(&first_chars(&reply.text, 160))?;
                    println!("{name} PROMPT OK {shown}");
                    let expected = format!("STUB:{}", first_chars(prompt_text, 2000));
                    if name == "stub" && reply.text != expected {
                        println!(
                            "{name} PROMPT BLOCKED unexpected reply {}",
                            serde_json::to_string(&reply.text)?
                        );
                        failed = true;
                    }
                }
                Err(error) => {
                    println!("{name} PROMPT BLOCKED {error}");
                    if name == "stub" {
                        failed = true;
                    }
                }
            }
        } else {
            println!("{name} PROMPT BLOCKED session did not start");
            if name == "stub" {
                failed = true;
            }
        }

        client.close().await?;
    }

    exit(if failed { 1 } else { 0 });
}

async fn web_probe(args: &[String]) -> Result<()> {
    let mut url = None;
    let mut profile = None;
    let mut question = None;
    let mut index = 0;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| anyhow!("{} requires a value", args[index]))?;
        match args[index].as_str() {
            "--web" => url = Some(value.as_str()),
            "--profile" => profile = Some(value.as_str()),
            "--question" => question = Some(value.as_str()),
            other => bail!("unexpected smoke argument {other}"),
        }
        index += 2;
    }
    let url = url.ok_or_else(|| {
        anyhow!("smoke --web <URL> --profile <Mattermost username> [--question <text>]")
    })?;
    let parsed = reqwest::Url::parse(url)?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("--web requires an HTTP or HTTPS page URL");
    }
    let profile = profile
        .ok_or_else(|| anyhow!("--profile selects a registry username, not an owner label"))?;
    let config = get_config()?;
    let entry = config
        .registry
        .get(profile)
        .ok_or_else(|| anyhow!("unknown registry profile {profile}"))?;
    let spec = config
        .agent(&entry.agent)
        .ok_or_else(|| anyhow!("unknown agent {}", entry.agent))?;
    let cwd = entry.cwd.to_string_lossy().into_owned();
    let mut scratch = ScratchDir::create()?;
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let question = format!(
        "{}\nRead {url} with your web retrieval tool now. Report concrete facts from the page and cite this URL. Do not answer from prior knowledge. If retrieval fails, explain the failure.",
        question.unwrap_or("Summarize the key facts on this public page.")
    );
    let prompt = PromptContext {
        language: &config.response_language,
        permalink: url,
        thread: None,
        question: &question,
        refresh_command: None,
        scratch_path: &scratch_path,
    }
    .render(config.prompt_template.as_ref());
    scratch.mark_in_use()?;
    let client = match AcpClient::spawn(&entry.agent, spec, &cwd).await {
        Ok(client) => client,
        Err(error) => {
            scratch.release_after_exit()?;
            return Err(error);
        }
    };
    let outcome = async {
        with_timeout(client.initialize(), 30_000, "web initialize").await?;
        let session_id = with_timeout(client.new_session(&cwd), 30_000, "web session/new").await?;
        let answer =
            with_timeout(client.prompt(&session_id, &prompt), 300_000, "web prompt").await?;
        println!("Requested source: {url}");
        println!("Response language: {}", config.response_language);
        println!("Answer:\n{}", answer.text);
        if !answer.fetched_url(url) {
            let states: Vec<String> = answer
                .tool_calls
                .values()
                .map(|call| format!("{}/{}", call.kind, call.status))
                .collect();
            bail!(
                "no completed ACP fetch for requested URL; observed tool states [{}]",
                states.join(", ")
            );
        }
        if !answer.text.contains(url) {
            bail!("answer does not cite the requested URL");
        }
        println!("PASS completed ACP fetch for requested URL and answer citation");
        Ok(())
    }
    .await;
    client.close().await?;
    scratch.release_after_exit()?;
    outcome
}

async fn with_timeout<F, T>(future: F, milliseconds: u64, label: &str) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(std::time::Duration::from_millis(milliseconds), future).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("{label} timed out after {milliseconds}ms")),
    }
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

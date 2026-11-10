use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::exit;
use std::time::Duration;

use crate::config::{get_config, Config, Owner};
use crate::listen::start_listener;
use crate::mm::{MattermostClient, MmPost};
use crate::store::{SessionRecord, SessionStatus};

pub async fn run() -> Result<()> {
    let root = crate::config::config_home()?;
    let state_dir = root.join("state");
    std::fs::create_dir_all(&state_dir)?;
    let probe_registry = state_dir.join("probe-registry.json");
    let registry = serde_json::json!({
        "owner1": { "agent": "stub", "cwd": "workspaces/owner1" },
    });
    std::fs::write(
        &probe_registry,
        format!("{}\n", serde_json::to_string_pretty(&registry)?),
    )?;
    std::env::set_var("ACP_REGISTRY", &probe_registry);

    let config = get_config()?;
    let owner = config
        .owner("owner1")
        .ok_or_else(|| anyhow!(".env is missing owner1 credentials"))?;
    let owner_mm = MattermostClient::new(&config.mm_url, &owner.token);
    let listener = start_listener("owner1").await?;
    let mut failed = false;
    let mut assertion = "owner1 posted in the monitor channel";

    let outcome = probe(config, owner, &owner_mm, &mut assertion).await;
    if let Err(error) = outcome {
        println!("FAIL {assertion}: {error}");
        failed = true;
    }

    listener.close().await;
    exit(if failed { 1 } else { 0 });
}

async fn probe(
    config: &Config,
    owner: &Owner,
    owner_mm: &MattermostClient,
    assertion: &mut &'static str,
) -> Result<()> {
    let channel_id = config
        .monitor_channel_ids
        .iter()
        .next()
        .ok_or_else(|| anyhow!("no monitor channels configured"))?;
    let marker = format!("acp probe {}", now_ms());
    let post = owner_mm
        .create_post(channel_id, &marker, None, None)
        .await?;
    *assertion = "owner1 reaction created an active session";
    let trigger = config
        .trigger_emojis
        .iter()
        .next()
        .ok_or_else(|| anyhow!("no trigger emoji configured"))?;
    owner_mm
        .add_reaction(&owner.user_id, &post.id, trigger)
        .await?;

    let link = format!("{}/{}/pl/{}", config.mm_url, config.mm_team, post.id);
    let post_id = post.id.clone();
    let state_file = config.state_file.clone();
    let session_key = format!("{}:{}", owner.name, post_id);
    let record = until(
        async || {
            let sessions = read_sessions(&state_file);
            Ok(sessions.get(&session_key).cloned().filter(|candidate| {
                candidate.status == SessionStatus::Active
                    && candidate
                        .acp_session_id
                        .as_deref()
                        .is_some_and(|session_id| !session_id.is_empty())
            }))
        },
        20_000,
    )
    .await?;
    println!(
        "PASS owner1 reaction created an active session for {}",
        post.id
    );

    *assertion = "self-DM for owner1 exists";
    let channel = owner_mm.get_channel(&record.dm_channel_id).await?;
    let members = owner_mm.get_channel_members(&record.dm_channel_id).await?;
    if channel.channel_type != "D" {
        return Err(anyhow!("channel type is {}, not D", channel.channel_type));
    }
    if members.len() != 1 || members[0].user_id != owner.user_id {
        let member_ids: Vec<&str> = members
            .iter()
            .map(|member| member.user_id.as_str())
            .collect();
        return Err(anyhow!(
            "self-DM members are [{}], expected only {}",
            member_ids.join(","),
            owner.name
        ));
    }
    println!("PASS self-DM for owner1 exists");

    *assertion = "dm root contains permalink for <post id>";
    let root = owner_mm.get_post(&record.dm_root_post_id).await?;
    if !root.message.contains(&link) {
        return Err(anyhow!("root message does not contain {link}"));
    }
    if !root.is_app_post() {
        return Err(anyhow!("root post is missing the acp_assist prop"));
    }
    println!("PASS dm root contains permalink for {}", post.id);

    *assertion = "thread reply contains the stub answer";
    let root_id = record.dm_root_post_id.clone();
    let first_reply = until(
        async || {
            let thread = owner_mm.get_thread(&root_id).await?;
            Ok(thread
                .posts
                .into_values()
                .find(|entry| entry.id != root_id && response_text(entry).starts_with("STUB:")))
        },
        30_000,
    )
    .await?;
    println!("PASS thread reply contains the stub answer");

    *assertion = "follow-up got a second reply from the same ACP session";
    let first_session_id = record.acp_session_id.clone();
    let follow_up = format!("{marker} follow-up");
    owner_mm
        .create_post(
            &record.dm_channel_id,
            &follow_up,
            Some(&record.dm_root_post_id),
            None,
        )
        .await?;
    let first_reply_id = first_reply.id.clone();
    let root_id = record.dm_root_post_id.clone();
    until(
        async || {
            let thread = owner_mm.get_thread(&root_id).await?;
            Ok(thread.posts.into_values().find(|entry| {
                entry.id != first_reply_id
                    && response_text(entry).starts_with("STUB:")
                    && response_text(entry).contains(&format!("User question:\n{follow_up}\n"))
            }))
        },
        30_000,
    )
    .await?;
    let updated = read_sessions(&config.state_file);
    let unchanged = updated
        .get(&session_key)
        .is_some_and(|current| current.acp_session_id == first_session_id);
    if !unchanged {
        return Err(anyhow!("ACP session id changed between prompts"));
    }
    println!("PASS follow-up got a second reply from the same ACP session");

    *assertion = "ACP prompt contains the post URL and the instruction";
    let instruction = "이거 해결 방법 제시해줘.";
    if !response_text(&first_reply).contains(&link) {
        return Err(anyhow!("stub answer does not contain {link}"));
    }
    if !response_text(&first_reply).contains(instruction) {
        return Err(anyhow!("stub answer does not contain the instruction"));
    }
    println!("PASS ACP prompt contains the post URL and the instruction");
    Ok(())
}

async fn until<T, F>(mut check: F, timeout_ms: u64) -> Result<T>
where
    F: AsyncFnMut() -> Result<Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("timed out after {timeout_ms}ms"));
        }
        if let Some(value) = check().await? {
            return Ok(value);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn read_sessions(file: &Path) -> std::collections::HashMap<String, SessionRecord> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn response_text(post: &MmPost) -> &str {
    post.props
        .as_ref()
        .and_then(|props| props.get("attachments"))
        .and_then(serde_json::Value::as_array)
        .and_then(|attachments| attachments.first())
        .and_then(|attachment| attachment.get("text"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&post.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_comes_from_attachment_not_message_label() {
        let post = MmPost {
            message: "ACP label".into(),
            props: Some(serde_json::json!({"attachments": [{"text": "STUB:answer"}]})),
            ..Default::default()
        };
        assert_eq!(response_text(&post), "STUB:answer");
        assert_eq!(
            response_text(&MmPost {
                message: "plain".into(),
                ..Default::default()
            }),
            "plain"
        );
    }
}

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::config::Owner;
use crate::mm::{MattermostClient, MmPost, MmUser, APP_PROP};
use crate::read::{fetch_thread, ThreadContext};

pub struct SelfDm {
    name: String,
    client: MattermostClient,
    user_id: String,
    channel_id: String,
}

impl SelfDm {
    pub async fn open(base_url: &str, owner: &Owner) -> Result<Self> {
        let client = MattermostClient::new(base_url, &owner.token);
        let channel = client
            .create_direct(&[owner.user_id.clone(), owner.user_id.clone()])
            .await
            .with_context(|| format!("could not open the self-DM for {}", owner.name))?;
        Ok(Self {
            name: owner.name.clone(),
            client,
            user_id: owner.user_id.clone(),
            channel_id: channel.id,
        })
    }

    pub fn owner_name(&self) -> &str {
        &self.name
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub async fn post(&self, message: &str, root_id: Option<&str>) -> Result<MmPost> {
        let props = Value::Object(Map::from_iter([(APP_PROP.to_string(), Value::Bool(true))]));
        self.client
            .create_post(&self.channel_id, message, root_id, Some(props))
            .await
    }

    pub async fn reply(&self, answer: &str, root_id: &str, agent: &str) -> Result<MmPost> {
        let props = json!({
            APP_PROP: true,
            "attachments": [{
                "fallback": answer,
                "color": "#4A90E2",
                "author_name": format!("ACP · {agent}"),
                "text": answer,
            }],
        });
        self.client
            .create_post(
                &self.channel_id,
                &format!("🤖 ACP · {agent}"),
                Some(root_id),
                Some(props),
            )
            .await
    }

    pub async fn pending(&self, agent: &str, root_id: &str) -> Result<MmPost> {
        let props = json!({
            APP_PROP: true,
            "attachments": [{
                "fallback": "working",
                "color": "#8E8E8E",
                "author_name": format!("ACP · {agent}"),
                "text": "작업 중...",
            }],
        });
        self.client
            .create_post(
                &self.channel_id,
                &format!("⏳ ACP · {agent}"),
                Some(root_id),
                Some(props),
            )
            .await
    }

    pub async fn reply_error(&self, detail: &str, root_id: &str) -> Result<MmPost> {
        let props = json!({
            APP_PROP: true,
            "attachments": [{
                "fallback": detail,
                "color": "#D24B4E",
                "author_name": "ACP error",
                "text": detail,
            }],
        });
        self.client
            .create_post(&self.channel_id, "🤖 ACP error", Some(root_id), Some(props))
            .await
    }

    pub async fn post_by_id(&self, post_id: &str) -> Result<MmPost> {
        self.client.get_post(post_id).await
    }

    pub async fn user(&self, user_id: &str) -> Result<MmUser> {
        self.client.get_user(user_id).await
    }

    pub async fn thread(&self, post_id: &str) -> Result<ThreadContext> {
        fetch_thread(&self.client, post_id).await
    }
}

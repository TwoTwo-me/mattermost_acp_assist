use anyhow::{anyhow, bail, Context, Result};
use reqwest::Url;
use std::collections::HashMap;

use crate::config::get_config;
use crate::mm::{MattermostClient, MmPost, MmThread};

pub struct ThreadPost {
    pub author: String,
    pub text: String,
    pub reacted: bool,
}

pub struct ThreadContext {
    pub channel_name: String,
    pub channel_type: String,
    pub reacted_post_id: String,
    pub posts: Vec<ThreadPost>,
}

pub fn parse_permalink(input: &str, mm_url: &str) -> Result<String> {
    let input = input.trim();
    if is_post_id(input) {
        return Ok(input.to_string());
    }
    let base =
        Url::parse(mm_url).with_context(|| format!("MM_URL is not a valid URL: {mm_url}"))?;
    let url = Url::parse(input)
        .with_context(|| format!("not a Mattermost permalink or post id: {input}"))?;
    if url.scheme() != base.scheme()
        || url.host_str() != base.host_str()
        || url.port() != base.port()
    {
        let host = url.host_str().unwrap_or_default();
        let origin = match url.port() {
            Some(port) => format!("{}://{host}:{port}", url.scheme()),
            None => format!("{}://{host}", url.scheme()),
        };
        bail!("refusing {input}: origin {origin} does not match MM_URL {mm_url}");
    }
    let base_path = base.path().trim_end_matches('/');
    let Some(tail) = url.path().strip_prefix(base_path) else {
        bail!("not a Mattermost permalink: {input}");
    };
    let segments: Vec<&str> = tail.trim_matches('/').split('/').collect();
    let post_id = match segments.as_slice() {
        ["_redirect", "pl", post_id] => *post_id,
        [_, "pl", post_id] => *post_id,
        _ => bail!("not a Mattermost permalink: {input}"),
    };
    if !is_post_id(post_id) {
        bail!("not a Mattermost post id in {input}");
    }
    Ok(post_id.to_string())
}

pub async fn fetch_thread(client: &MattermostClient, post_id: &str) -> Result<ThreadContext> {
    let reacted = client.get_post(post_id).await?;
    let root_id = source_root_id(&reacted);
    let thread = client
        .get_thread(root_id)
        .await
        .with_context(|| format!("could not read the thread for post {post_id}"))?;
    if !thread.posts.contains_key(root_id) || !thread.posts.contains_key(post_id) {
        bail!("thread response is missing the root or reacted post {post_id}");
    }
    let channel = client.get_channel(&reacted.channel_id).await?;
    let channel_name = [channel.name, channel.display_name, channel.id]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or_default();

    let ordered = ordered_posts(&thread);
    let mut author_ids: Vec<String> = Vec::new();
    for post in &ordered {
        if !author_ids.contains(&post.user_id) {
            author_ids.push(post.user_id.clone());
        }
    }
    let authors: HashMap<String, String> = client
        .get_users_by_ids(&author_ids)
        .await?
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect();

    let posts = ordered
        .into_iter()
        .map(|post| ThreadPost {
            author: authors
                .get(&post.user_id)
                .cloned()
                .unwrap_or_else(|| post.user_id.clone()),
            text: post.message.clone(),
            reacted: post.id == post_id,
        })
        .collect();

    Ok(ThreadContext {
        channel_name,
        channel_type: channel.channel_type,
        reacted_post_id: post_id.to_string(),
        posts,
    })
}

pub fn render(context: &ThreadContext) -> String {
    let mut lines = vec![
        format!(
            "channel: {} ({})",
            context.channel_name, context.channel_type
        ),
        format!("reacted post: {}", context.reacted_post_id),
        String::new(),
    ];
    for post in &context.posts {
        let marker = if post.reacted { "[reacted] " } else { "" };
        lines.push(format!("{marker}{}:", post.author));
        let text = if post.text.trim().is_empty() {
            "(empty post)"
        } else {
            post.text.as_str()
        };
        for line in text.lines() {
            lines.push(format!("  {line}"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

pub async fn run(args: &[String]) -> Result<()> {
    let (owner, input) = parse_args(args)?;
    let config = get_config()?;
    let token = match owner {
        Some(name) => config
            .owner(&name)
            .ok_or_else(|| anyhow!("unknown owner \"{name}\""))?
            .token
            .as_str(),
        None => config.reader_token.as_deref().ok_or_else(|| {
            anyhow!("read needs a token: pass --as <owner> or set MM_READER_TOKEN")
        })?,
    };
    let post_id = parse_permalink(&input, &config.mm_url)?;
    let client = MattermostClient::new(&config.mm_url, token);
    let context = fetch_thread(&client, &post_id).await?;
    println!("{}", render(&context));
    Ok(())
}

fn parse_args(args: &[String]) -> Result<(Option<String>, String)> {
    let mut owner = None;
    let mut input = None;
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
            value if value.starts_with("--") => bail!("unexpected argument {value}"),
            value => {
                if input.replace(value.to_string()).is_some() {
                    bail!("read takes one permalink or post id");
                }
                index += 1;
            }
        }
    }
    let input = input.ok_or_else(|| {
        anyhow!("usage: mattermost-acp-assist read [--as <owner>] <permalink|post-id>")
    })?;
    Ok((owner, input))
}

fn ordered_posts(thread: &MmThread) -> Vec<&MmPost> {
    let mut posts: Vec<&MmPost> = thread.posts.values().collect();
    posts.sort_by(|left, right| {
        (!left.root_id.is_empty(), left.create_at, &left.id).cmp(&(
            !right.root_id.is_empty(),
            right.create_at,
            &right.id,
        ))
    });
    posts
}

fn source_root_id(post: &MmPost) -> &str {
    if post.root_id.is_empty() {
        &post.id
    } else {
        &post.root_id
    }
}

fn is_post_id(text: &str) -> bool {
    text.len() == 26
        && text
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_resolves_to_root() {
        let root = MmPost {
            id: "root".into(),
            ..Default::default()
        };
        let reply = MmPost {
            id: "reply".into(),
            root_id: root.id.clone(),
            ..Default::default()
        };
        assert_eq!(source_root_id(&root), "root");
        assert_eq!(source_root_id(&reply), "root");
    }

    #[test]
    fn thread_is_complete_and_chronological_despite_api_order() {
        let root = MmPost {
            id: "root".into(),
            create_at: 1,
            ..Default::default()
        };
        let first = MmPost {
            id: "first".into(),
            root_id: "root".into(),
            create_at: 2,
            ..Default::default()
        };
        let last = MmPost {
            id: "last".into(),
            root_id: "root".into(),
            create_at: 3,
            ..Default::default()
        };
        let thread = MmThread {
            posts: [root, first, last]
                .into_iter()
                .map(|post| (post.id.clone(), post))
                .collect(),
        };
        let ids: Vec<&str> = ordered_posts(&thread)
            .into_iter()
            .map(|post| post.id.as_str())
            .collect();
        assert_eq!(ids, ["root", "first", "last"]);
    }
}

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Starting,
    Active,
    Failed,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    #[serde(default)]
    pub session_key: String,
    pub post_id: String,
    pub status: SessionStatus,
    pub acp_session_id: Option<String>,
    pub dm_channel_id: String,
    pub dm_root_post_id: String,
    pub agent: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at: i64,
}

struct Inner {
    records: HashMap<String, SessionRecord>,
    by_dm_root: HashMap<String, String>,
}

pub struct SessionStore {
    file: PathBuf,
    inner: Mutex<Inner>,
}

impl SessionStore {
    pub fn new(file: &Path) -> Result<Self> {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut records = HashMap::new();
        let mut by_dm_root = HashMap::new();
        if file.exists() {
            let text = std::fs::read_to_string(file)?;
            let stored: HashMap<String, SessionRecord> = serde_json::from_str(&text)?;
            for (key, mut record) in stored {
                record.session_key = key.clone();
                if !record.dm_root_post_id.is_empty() {
                    by_dm_root.insert(record.dm_root_post_id.clone(), key.clone());
                }
                records.insert(key, record);
            }
        }
        Ok(Self {
            file: file.to_path_buf(),
            inner: Mutex::new(Inner {
                records,
                by_dm_root,
            }),
        })
    }

    pub fn get(&self, post_id: &str) -> Option<SessionRecord> {
        self.inner
            .lock()
            .expect("store mutex")
            .records
            .get(post_id)
            .cloned()
    }

    pub fn get_by_dm_root(&self, dm_root_post_id: &str) -> Option<SessionRecord> {
        let inner = self.inner.lock().expect("store mutex");
        let key = inner.by_dm_root.get(dm_root_post_id)?;
        inner.records.get(key).cloned()
    }

    pub fn put(&self, mut record: SessionRecord) -> Result<SessionRecord> {
        record.updated_at = now_ms();
        if record.session_key.is_empty() {
            record.session_key = record.post_id.clone();
        }
        let key = record.session_key.clone();
        let mut inner = self.inner.lock().expect("store mutex");
        if let Some(previous) = inner.records.insert(key.clone(), record.clone()) {
            inner.by_dm_root.remove(&previous.dm_root_post_id);
        }
        if !record.dm_root_post_id.is_empty() {
            inner
                .by_dm_root
                .insert(record.dm_root_post_id.clone(), key.clone());
        }
        let snapshot = serde_json::to_string_pretty(&inner.records)?;
        let tmp = self
            .file
            .with_extension(format!("{}.{}.tmp", std::process::id(), now_ms()));
        std::fs::write(&tmp, format!("{snapshot}\n"))?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(record)
    }

    pub fn update<F: FnOnce(&mut SessionRecord)>(
        &self,
        post_id: &str,
        change: F,
    ) -> Option<SessionRecord> {
        let mut record = self.get(post_id)?;
        change(&mut record);
        self.put(record).ok()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::ScratchDir;

    #[test]
    fn retrigger_replaces_closed_record_without_routing_old_dm_to_new_session() {
        let directory = ScratchDir::create().unwrap();
        let store = SessionStore::new(&directory.path().join("sessions.json")).unwrap();
        let mut record = SessionRecord {
            session_key: "owner:source".into(),
            post_id: "source".into(),
            status: SessionStatus::Closed,
            acp_session_id: Some("old-session".into()),
            dm_channel_id: "dm".into(),
            dm_root_post_id: "old-root".into(),
            agent: "stub".into(),
            cwd: "/project".into(),
            error: None,
            updated_at: 0,
        };
        store.put(record.clone()).unwrap();
        record.status = SessionStatus::Starting;
        record.acp_session_id = None;
        record.dm_root_post_id = "new-root".into();
        store.put(record).unwrap();
        assert!(store.get_by_dm_root("old-root").is_none());
        assert_eq!(
            store.get_by_dm_root("new-root").unwrap().status,
            SessionStatus::Starting
        );
        let reopened = SessionStore::new(&directory.path().join("sessions.json")).unwrap();
        assert!(reopened.get_by_dm_root("old-root").is_none());
        assert_eq!(
            reopened.get_by_dm_root("new-root").unwrap().post_id,
            "source"
        );
        std::fs::remove_dir_all(directory.path()).unwrap();
    }
}

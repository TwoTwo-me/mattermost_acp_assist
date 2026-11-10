#![cfg(unix)]

use serde_json::json;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
const MARKER: &str = ".acp-assist-scratch.json";

struct Fixture {
    temporary: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temporary = std::env::temp_dir().join(format!(
            "acp-assist-cli-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&temporary)
            .unwrap();
        let root = temporary.join(format!("mattermost-acp-assist-{}", unsafe {
            libc::geteuid()
        }));
        Self { temporary, root }
    }

    fn create_root(&self) {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&self.root)
            .unwrap();
    }

    fn managed(&self, name: &str, state: &str) -> (PathBuf, File) {
        let path = self.root.join(name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = std::fs::metadata(&path).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join(MARKER))
            .unwrap();
        let marker = file.metadata().unwrap();
        serde_json::to_writer(
            &file,
            &json!({
                "format": "mattermost-acp-assist-scratch-v1",
                "name": name,
                "uid": unsafe { libc::geteuid() },
                "directory_device": directory.dev(),
                "directory_inode": directory.ino(),
                "marker_device": marker.dev(),
                "marker_inode": marker.ino(),
                "state": state,
            }),
        )
        .unwrap();
        std::fs::write(path.join("note"), "retained session data").unwrap();
        (path, file)
    }

    fn cleanup(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mattermost-acp-assist"))
            .arg("cleanup-scratch")
            .args(args)
            .env_clear()
            .env("TMPDIR", &self.temporary)
            .env("TMP", &self.temporary)
            .env("TEMP", &self.temporary)
            .current_dir(&self.temporary)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.temporary).unwrap();
    }
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cleanup_without_configuration_is_dry_run_safe_and_repeatable() {
    let fixture = Fixture::new();
    fixture.create_root();
    let (ready, ready_file) = fixture.managed("session-ready", "Ready");
    drop(ready_file);
    let (active, active_file) = fixture.managed("session-active", "Ready");
    assert_eq!(
        unsafe { libc::flock(active_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let (uncertain, uncertain_file) = fixture.managed("session-uncertain", "InUse");
    drop(uncertain_file);
    let unrelated = fixture.root.join("session-unmarked");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("keep"), "unrelated").unwrap();

    let dry_run = fixture.cleanup(&["--dry-run"]);
    assert_success(&dry_run);
    assert!(stdout(&dry_run)
        .contains("removed=0 would_remove=1 active_or_uncertain=2 unverified=1 errors=0"));
    assert!(ready.join("note").exists());

    let cleanup = fixture.cleanup(&[]);
    assert_success(&cleanup);
    assert!(stdout(&cleanup)
        .contains("removed=1 would_remove=0 active_or_uncertain=2 unverified=1 errors=0"));
    assert!(!ready.exists());
    assert!(active.join("note").exists());
    assert!(uncertain.join("note").exists());
    assert!(unrelated.join("keep").exists());

    drop(active_file);
    let repeated = fixture.cleanup(&[]);
    assert_success(&repeated);
    assert!(stdout(&repeated).contains("removed=1 would_remove=0 active_or_uncertain=1"));
    let empty = fixture.cleanup(&[]);
    assert_success(&empty);
    assert!(stdout(&empty).contains("removed=0"));
}

#[test]
fn cleanup_excludes_replaced_markers_traversal_and_symlink_targets() {
    let fixture = Fixture::new();
    fixture.create_root();
    let outside = fixture.temporary.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), "outside").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.root.join("session-link")).unwrap();
    let (replaced, old_file) = fixture.managed("session-replaced", "Ready");
    let replacement = replaced.join("replacement");
    std::fs::copy(replaced.join(MARKER), &replacement).unwrap();
    std::fs::rename(&replacement, replaced.join(MARKER)).unwrap();
    drop(old_file);
    let (traversal, file) = fixture.managed("session-traversal", "Ready");
    drop(file);
    let mut marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(traversal.join(MARKER)).unwrap()).unwrap();
    marker["name"] = json!("../../outside");
    std::fs::write(traversal.join(MARKER), serde_json::to_vec(&marker).unwrap()).unwrap();
    let (malformed, file) = fixture.managed("session-malformed", "Ready");
    drop(file);
    std::fs::write(malformed.join(MARKER), "not a marker").unwrap();

    let result = fixture.cleanup(&[]);
    assert_success(&result);
    assert!(stdout(&result)
        .contains("removed=0 would_remove=0 active_or_uncertain=0 unverified=4 errors=0"));
    for path in [&outside, &replaced, &traversal, &malformed] {
        assert!(path.exists());
    }
    assert!(outside.join("keep").exists());
}

#[test]
fn missing_root_is_noop_and_bad_arguments_fail_without_config() {
    let fixture = Fixture::new();
    let result = fixture.cleanup(&[]);
    assert_success(&result);
    assert!(stdout(&result).contains("removed=0"));
    assert!(!fixture.root.exists());
    let invalid = fixture.cleanup(&["--force"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("cleanup-scratch [--dry-run]"));
}

#[test]
fn symlinked_root_fails_without_touching_target() {
    let fixture = Fixture::new();
    let outside = fixture.temporary.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), "outside").unwrap();
    std::os::unix::fs::symlink(&outside, &fixture.root).unwrap();
    let result = fixture.cleanup(&[]);
    assert!(!result.status.success());
    assert!(outside.join("keep").exists());
}

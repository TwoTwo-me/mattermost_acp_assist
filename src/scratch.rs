use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
const MARKER: &str = ".acp-assist-scratch.json";
const FORMAT: &str = "mattermost-acp-assist-scratch-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format: String,
    name: String,
    uid: u32,
    directory_device: u64,
    directory_inode: u64,
    marker_device: u64,
    marker_inode: u64,
    state: LeaseState,
}

#[derive(Serialize, Deserialize, PartialEq)]
enum LeaseState {
    Ready,
    InUse,
}

pub struct ScratchDir {
    path: PathBuf,
    lease: Option<File>,
    marker: Marker,
}

impl ScratchDir {
    pub fn create() -> Result<Self> {
        Self::create_in(&scratch_root()?)
    }

    fn create_in(root: &Path) -> Result<Self> {
        let root_handle = open_root(root, true)?.context("scratch root disappeared")?;
        lock_root(&root_handle)?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let name = format!("session-{}-{nonce}-{sequence}", std::process::id());
            let path = root.join(&name);
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let directory = open_directory(&path)?;
                    let identity = directory.metadata()?;
                    let mut lease = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(path.join(MARKER))?;
                    if !try_lock(&lease)? {
                        bail!("new scratch marker is already locked");
                    }
                    let file_identity = lease.metadata()?;
                    let marker = Marker {
                        format: FORMAT.to_string(),
                        name,
                        uid: current_uid(),
                        directory_device: identity.dev(),
                        directory_inode: identity.ino(),
                        marker_device: file_identity.dev(),
                        marker_inode: file_identity.ino(),
                        state: LeaseState::Ready,
                    };
                    write_marker(&mut lease, &marker)?;
                    require_same_path(root, &root_handle.metadata()?)?;
                    return Ok(Self {
                        path,
                        lease: Some(lease),
                        marker,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("could not create session scratch directory")
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mark_in_use(&mut self) -> Result<()> {
        let lease = self
            .lease
            .as_mut()
            .context("scratch lease has been released")?;
        self.marker.state = LeaseState::InUse;
        write_marker(lease, &self.marker)
    }

    pub fn release_after_exit(&mut self) -> Result<()> {
        if let Some(lease) = self.lease.as_mut() {
            self.marker.state = LeaseState::Ready;
            write_marker(lease, &self.marker)?;
            self.lease.take();
        }
        Ok(())
    }
}

fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn scratch_root() -> Result<PathBuf> {
    let temporary = std::env::temp_dir()
        .canonicalize()
        .context("could not resolve OS temporary directory")?;
    Ok(temporary.join(format!("mattermost-acp-assist-{}", current_uid())))
}

fn open_directory(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?)
}

fn private_owned(metadata: &Metadata) -> bool {
    metadata.uid() == current_uid() && metadata.mode() & 0o077 == 0
}

fn open_root(root: &Path, create: bool) -> Result<Option<File>> {
    if create {
        match std::fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("could not create scratch root"),
        }
    } else if !root.try_exists()? {
        if std::fs::symlink_metadata(root).is_ok() {
            bail!("scratch root is a dangling symlink");
        }
        return Ok(None);
    }
    let handle = open_directory(root).context("scratch root must be a real directory")?;
    if !private_owned(&handle.metadata()?) {
        bail!(
            "scratch root must be owned by the current user and private: {}",
            root.display()
        );
    }
    Ok(Some(handle))
}

fn try_lock(file: &File) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error).context("could not lock scratch ownership marker")
    }
}

fn lock_root(root: &File) -> Result<()> {
    if unsafe { libc::flock(root.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not lock scratch root");
    }
    Ok(())
}

fn write_marker(file: &mut File, marker: &Marker) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    serde_json::to_writer(&mut *file, marker)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn require_same_path(path: &Path, expected: &Metadata) -> Result<()> {
    let current = std::fs::symlink_metadata(path)?;
    if current.file_type().is_symlink() || !same_file(&current, expected) {
        bail!("scratch path was replaced: {}", path.display());
    }
    Ok(())
}

#[derive(Default, Debug)]
struct CleanupReport {
    removed: usize,
    eligible: usize,
    active: usize,
    unverified: usize,
    errors: usize,
}

enum Candidate {
    Removed,
    Eligible,
    Active,
    Unverified,
}

pub fn run_cleanup(args: &[String]) -> Result<()> {
    let dry_run = match args {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        _ => bail!("usage: mattermost-acp-assist cleanup-scratch [--dry-run]"),
    };
    let root = scratch_root()?;
    let report = cleanup_in(&root, dry_run)?;
    println!("scratch root: {}", root.display());
    println!(
        "removed={} would_remove={} active_or_uncertain={} unverified={} errors={}",
        report.removed, report.eligible, report.active, report.unverified, report.errors
    );
    if report.errors != 0 {
        bail!(
            "scratch cleanup had {} error(s); affected directories were retained where possible",
            report.errors
        );
    }
    Ok(())
}

fn cleanup_in(root: &Path, dry_run: bool) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let Some(root_handle) = open_root(root, false)? else {
        return Ok(report);
    };
    if !try_lock(&root_handle)? {
        bail!("scratch root is busy; retry cleanup");
    }
    let root_identity = root_handle.metadata()?;
    require_same_path(root, &root_identity)?;
    for entry in std::fs::read_dir(root)? {
        let outcome = entry.map_err(anyhow::Error::from).and_then(|entry| {
            let path = entry.path();
            clean_candidate(root, &root_identity, &path, dry_run)
                .with_context(|| format!("could not clean {}", path.display()))
        });
        match outcome {
            Ok(Candidate::Removed) => report.removed += 1,
            Ok(Candidate::Eligible) => report.eligible += 1,
            Ok(Candidate::Active) => report.active += 1,
            Ok(Candidate::Unverified) => report.unverified += 1,
            Err(error) => {
                report.errors += 1;
                eprintln!("scratch cleanup: {error:#}");
            }
        }
    }
    Ok(report)
}

fn clean_candidate(
    root: &Path,
    root_identity: &Metadata,
    path: &Path,
    dry_run: bool,
) -> Result<Candidate> {
    let identity = std::fs::symlink_metadata(path)?;
    if !identity.is_dir() || !private_owned(&identity) {
        return Ok(Candidate::Unverified);
    }
    let directory = open_directory(path)?;
    if !same_file(&identity, &directory.metadata()?) {
        return Ok(Candidate::Unverified);
    }
    let marker_path = path.join(MARKER);
    let marker_identity = match std::fs::symlink_metadata(&marker_path) {
        Ok(metadata) if metadata.is_file() && private_owned(&metadata) && metadata.nlink() == 1 => {
            metadata
        }
        _ => return Ok(Candidate::Unverified),
    };
    let mut lease = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&marker_path)?;
    if !same_file(&marker_identity, &lease.metadata()?) {
        return Ok(Candidate::Unverified);
    }
    if !try_lock(&lease)? {
        return Ok(Candidate::Active);
    }
    let mut text = String::new();
    (&mut lease).take(4097).read_to_string(&mut text)?;
    let marker = match serde_json::from_str::<Marker>(&text) {
        Ok(marker) if text.len() <= 4096 => marker,
        _ => return Ok(Candidate::Unverified),
    };
    if marker.format != FORMAT
        || marker.uid != current_uid()
        || path.file_name().and_then(|name| name.to_str()) != Some(marker.name.as_str())
        || marker.directory_device != identity.dev()
        || marker.directory_inode != identity.ino()
        || marker.marker_device != marker_identity.dev()
        || marker.marker_inode != marker_identity.ino()
    {
        return Ok(Candidate::Unverified);
    }
    if marker.state == LeaseState::InUse {
        return Ok(Candidate::Active);
    }
    require_same_path(root, root_identity)?;
    require_same_path(path, &identity)?;
    require_same_path(&marker_path, &marker_identity)?;
    if dry_run {
        return Ok(Candidate::Eligible);
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == MARKER {
            continue;
        }
        require_same_path(root, root_identity)?;
        require_same_path(path, &identity)?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    require_same_path(root, root_identity)?;
    require_same_path(path, &identity)?;
    require_same_path(&marker_path, &marker_identity)?;
    std::fs::remove_file(marker_path)?;
    std::fs::remove_dir(path).with_context(|| format!("could not remove {}", path.display()))?;
    Ok(Candidate::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "acp-assist-cleanup-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn drop_retains_files_and_manual_cleanup_is_repeatable() {
        let root = TestRoot::new();
        let first = ScratchDir::create_in(&root.0).unwrap();
        let second = ScratchDir::create_in(&root.0).unwrap();
        assert_ne!(first.path(), second.path());
        let path = first.path().to_path_buf();
        std::fs::write(path.join("note"), "retained").unwrap();
        drop(first);
        assert!(path.join("note").exists());
        let dry = cleanup_in(&root.0, true).unwrap();
        assert_eq!(dry.eligible, 1);
        assert_eq!(dry.active, 1);
        assert!(path.join("note").exists());
        assert_eq!(cleanup_in(&root.0, false).unwrap().removed, 1);
        assert!(second.path().exists());
        drop(second);
        assert_eq!(cleanup_in(&root.0, false).unwrap().removed, 1);
        assert_eq!(cleanup_in(&root.0, false).unwrap().removed, 0);
    }

    #[test]
    fn uncertain_exit_stays_protected_after_lock_release() {
        let root = TestRoot::new();
        let mut scratch = ScratchDir::create_in(&root.0).unwrap();
        let path = scratch.path().to_path_buf();
        scratch.mark_in_use().unwrap();
        assert_eq!(cleanup_in(&root.0, false).unwrap().active, 1);
        drop(scratch);
        assert_eq!(cleanup_in(&root.0, false).unwrap().active, 1);
        assert!(path.exists());
    }

    #[test]
    fn confirmed_exit_retains_files_but_allows_cleanup() {
        let root = TestRoot::new();
        let mut scratch = ScratchDir::create_in(&root.0).unwrap();
        let path = scratch.path().to_path_buf();
        scratch.mark_in_use().unwrap();
        scratch.release_after_exit().unwrap();
        scratch.release_after_exit().unwrap();
        assert!(path.exists());
        assert_eq!(cleanup_in(&root.0, false).unwrap().removed, 1);
    }

    #[test]
    fn excludes_unmarked_replaced_and_symlinked_directories() {
        let root = TestRoot::new();
        let scratch = ScratchDir::create_in(&root.0).unwrap();
        let original = scratch.path().to_path_buf();
        drop(scratch);
        let moved = root.0.join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&original)
            .unwrap();
        std::fs::copy(moved.join(MARKER), original.join(MARKER)).unwrap();
        std::fs::write(original.join("keep"), "replacement").unwrap();
        let foreign = root.0.join("session-foreign");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&foreign)
            .unwrap();
        std::os::unix::fs::symlink(&foreign, root.0.join("session-link")).unwrap();
        let report = cleanup_in(&root.0, false).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.unverified, 4);
        assert!(original.join("keep").exists());
    }

    #[test]
    fn does_not_follow_nested_symlinks_or_marker_symlinks() {
        let root = TestRoot::new();
        let foreign = TestRoot::new();
        std::fs::write(foreign.0.join("keep"), "foreign").unwrap();
        let scratch = ScratchDir::create_in(&root.0).unwrap();
        std::os::unix::fs::symlink(&foreign.0, scratch.path().join("link")).unwrap();
        drop(scratch);
        assert_eq!(cleanup_in(&root.0, false).unwrap().removed, 1);
        assert!(foreign.0.join("keep").exists());
        let scratch = ScratchDir::create_in(&root.0).unwrap();
        let marker = scratch.path().join(MARKER);
        drop(scratch);
        std::fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink(foreign.0.join("keep"), marker).unwrap();
        assert_eq!(cleanup_in(&root.0, false).unwrap().unverified, 1);
        assert!(foreign.0.join("keep").exists());
    }

    #[test]
    fn partial_cleanup_failure_keeps_marker_for_retry() {
        if current_uid() == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = TestRoot::new();
        let scratch = ScratchDir::create_in(&root.0).unwrap();
        let path = scratch.path().to_path_buf();
        let protected = path.join("protected");
        std::fs::create_dir(&protected).unwrap();
        std::fs::write(protected.join("note"), "retained").unwrap();
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o500)).unwrap();
        drop(scratch);
        let result = cleanup_in(&root.0, false).unwrap();
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.errors, 1);
        assert!(path.join(MARKER).exists());
        assert_eq!(cleanup_in(&root.0, false).unwrap().removed, 1);
    }

    #[test]
    fn refuses_symlinked_or_public_root() {
        let parent = TestRoot::new();
        let target = TestRoot::new();
        let link = parent.0.join("root");
        std::os::unix::fs::symlink(&target.0, &link).unwrap();
        assert!(cleanup_in(&link, false).is_err());
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(cleanup_in(&target.0, false).is_err());
    }
}

use crate::{SnapshotError, capture_git_snapshot};
use needle_core::RepositorySnapshot;
use std::borrow::Cow;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("snapshot capture failed: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("sandbox I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("sandbox Git command failed: {0}")]
    Git(String),
    #[error("sandbox source path is unsafe: {0}")]
    UnsafePath(String),
    #[error("sandbox snapshot is not reproducible: {0}")]
    SnapshotMismatch(String),
}

#[derive(Debug)]
pub struct IsolatedCheckout {
    source_root: PathBuf,
    run_root: PathBuf,
    checkout_root: PathBuf,
    target_root: PathBuf,
    temp_root: PathBuf,
    snapshot: RepositorySnapshot,
    cleaned: bool,
}

impl IsolatedCheckout {
    pub fn materialize(source: &Path, runs_directory: &Path) -> Result<Self, SandboxError> {
        let (source_root, snapshot) = capture_git_snapshot(source)?;
        fs::create_dir_all(runs_directory)?;
        let run_root = runs_directory.join(unique_run_id());
        let checkout_root = run_root.join("checkout");
        let target_root = run_root.join("target");
        let temp_root = run_root.join("tmp");
        let source_index = {
            let path = git_path(&source_root, "index")?;
            path.is_file().then(|| fs::read(path)).transpose()?
        };
        fs::create_dir_all(&run_root)?;
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&temp_root)?;

        let checkout_git_path = git_cli_path(&checkout_root)?;
        git(
            &source_root,
            &[
                "worktree",
                "add",
                "--detach",
                "--force",
                checkout_git_path.as_ref(),
                &snapshot.head_sha,
            ],
        )?;

        let materialized = (|| -> Result<(), SandboxError> {
            copy_index(source_index.as_deref(), &checkout_root)?;
            overlay_source_files(&source_root, &checkout_root)?;
            let (_, observed) = capture_git_snapshot(&checkout_root)?;
            if observed.source_digest != snapshot.source_digest {
                return Err(SandboxError::SnapshotMismatch(format!(
                    "expected source={} repository={} tracked={} untracked={}; \
                     observed source={} repository={} tracked={} untracked={}",
                    snapshot.source_digest,
                    snapshot.repository_id,
                    snapshot.tracked_changes_digest,
                    snapshot.untracked_content_digest,
                    observed.source_digest,
                    observed.repository_id,
                    observed.tracked_changes_digest,
                    observed.untracked_content_digest,
                )));
            }
            protect_git_marker(&checkout_root)?;
            Ok(())
        })();
        if let Err(error) = materialized {
            let _ = remove_worktree(&source_root, &checkout_root);
            let _ = fs::remove_dir_all(&run_root);
            return Err(error);
        }

        Ok(Self {
            source_root,
            run_root,
            checkout_root,
            target_root,
            temp_root,
            snapshot,
            cleaned: false,
        })
    }

    pub fn checkout_root(&self) -> &Path {
        &self.checkout_root
    }

    pub fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub fn temp_root(&self) -> &Path {
        &self.temp_root
    }

    pub fn run_root(&self) -> &Path {
        &self.run_root
    }

    pub fn snapshot(&self) -> &RepositorySnapshot {
        &self.snapshot
    }

    pub fn cleanup(mut self) -> Result<(), SandboxError> {
        self.cleanup_inner()
    }

    fn cleanup_inner(&mut self) -> Result<(), SandboxError> {
        if self.cleaned {
            return Ok(());
        }
        let unprotect_error = unprotect_git_marker(&self.checkout_root).err();
        let worktree_error = remove_worktree(&self.source_root, &self.checkout_root).err();
        let run_root_error = remove_directory_with_retry(&self.run_root).err();
        if worktree_error.is_none() && run_root_error.is_none() {
            self.cleaned = true;
            return Ok(());
        }
        Err(worktree_error
            .or(run_root_error)
            .or(unprotect_error)
            .expect("cleanup error must be present"))
    }
}

impl Drop for IsolatedCheckout {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup_inner();
        }
    }
}

fn copy_index(source_index: Option<&[u8]>, checkout_root: &Path) -> Result<(), SandboxError> {
    let checkout_git = git_path(checkout_root, "index")?;
    if let Some(source_index) = source_index {
        fs::write(checkout_git, source_index)?;
    }
    Ok(())
}

fn overlay_source_files(source_root: &Path, checkout_root: &Path) -> Result<(), SandboxError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source_root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(SandboxError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    for raw in output.stdout.split(|byte| *byte == 0).filter(|value| !value.is_empty()) {
        let relative = std::str::from_utf8(raw)
            .map_err(|_| SandboxError::UnsafePath("non-UTF-8".to_owned()))?;
        let relative_path = safe_relative(relative)?;
        let source = source_root.join(&relative_path);
        let destination = checkout_root.join(&relative_path);
        if source.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &destination)?;
        } else if destination.exists() {
            if destination.is_dir() {
                return Err(SandboxError::UnsafePath(relative.to_owned()));
            }
            fs::remove_file(destination)?;
        }
    }
    Ok(())
}

fn safe_relative(value: &str) -> Result<PathBuf, SandboxError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(SandboxError::UnsafePath(value.to_owned()));
    }
    Ok(path.to_path_buf())
}

fn git_path(root: &Path, name: &str) -> Result<PathBuf, SandboxError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-path", name])
        .output()?;
    if !output.status.success() {
        return Err(SandboxError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|_| SandboxError::UnsafePath("non-UTF-8 Git path".to_owned()))?;
    Ok(PathBuf::from(value.trim()))
}

fn protect_git_marker(checkout_root: &Path) -> Result<(), SandboxError> {
    let marker = checkout_root.join(".git");
    let mut permissions = fs::metadata(&marker)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(marker, permissions)?;
    Ok(())
}

fn unprotect_git_marker(checkout_root: &Path) -> Result<(), SandboxError> {
    let marker = checkout_root.join(".git");
    if marker.exists() {
        let mut permissions = fs::metadata(&marker)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(marker, permissions)?;
    }
    Ok(())
}

fn remove_worktree(source_root: &Path, checkout_root: &Path) -> Result<(), SandboxError> {
    let checkout_git_path = git_cli_path(checkout_root)?;
    let first = git(source_root, &["worktree", "remove", "--force", checkout_git_path.as_ref()]);
    if first.is_ok() {
        return Ok(());
    }
    let second = git(source_root, &["worktree", "remove", "--force", checkout_git_path.as_ref()]);
    match second {
        Ok(()) => Ok(()),
        Err(remove_error) => match worktree_registered(source_root, checkout_root) {
            Ok(false) => Ok(()),
            Ok(true) => Err(remove_error),
            Err(verification_error) => Err(SandboxError::Git(format!(
                "{remove_error}; worktree registration verification failed: {verification_error}"
            ))),
        },
    }
}

fn worktree_registered(source_root: &Path, checkout_root: &Path) -> Result<bool, SandboxError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source_root)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(SandboxError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    let expected = normalized_path(checkout_root)?;
    for field in output.stdout.split(|byte| *byte == 0) {
        let Some(path) = field.strip_prefix(b"worktree ") else {
            continue;
        };
        let path = std::str::from_utf8(path)
            .map_err(|_| SandboxError::UnsafePath("non-UTF-8 Git worktree path".to_owned()))?;
        if normalized_path(Path::new(path))? == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalized_path(path: &Path) -> Result<String, SandboxError> {
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(SandboxError::Io(error)),
    };
    let value = git_cli_path(&canonical)?;
    #[cfg(windows)]
    let value = value.replace('\\', "/").to_ascii_lowercase();
    #[cfg(not(windows))]
    let value = value.into_owned();
    Ok(value.trim_end_matches('/').to_owned())
}

fn remove_directory_with_retry(path: &Path) -> Result<(), SandboxError> {
    #[cfg(windows)]
    const RETRY_DELAYS: [Duration; 8] = [
        Duration::from_millis(25),
        Duration::from_millis(50),
        Duration::from_millis(100),
        Duration::from_millis(200),
        Duration::from_millis(400),
        Duration::from_millis(800),
        Duration::from_millis(1_600),
        Duration::from_millis(2_000),
    ];
    #[cfg(not(windows))]
    const RETRY_DELAYS: [Duration; 3] =
        [Duration::from_millis(10), Duration::from_millis(25), Duration::from_millis(50)];
    if !path.exists() {
        return Ok(());
    }
    let mut result = fs::remove_dir_all(path);
    for delay in RETRY_DELAYS {
        if result.is_ok() || !path.exists() {
            return Ok(());
        }
        thread::sleep(delay);
        result = fs::remove_dir_all(path);
    }
    if !path.exists() { Ok(()) } else { result.map_err(SandboxError::Io) }
}

fn git(root: &Path, arguments: &[&str]) -> Result<(), SandboxError> {
    let output = Command::new("git").arg("-C").arg(root).args(arguments).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(SandboxError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()))
    }
}

fn git_cli_path(path: &Path) -> Result<Cow<'_, str>, SandboxError> {
    let value =
        path.to_str().ok_or_else(|| SandboxError::UnsafePath(path.display().to_string()))?;
    #[cfg(windows)]
    {
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(Cow::Owned(format!(r"\\{unc}")));
        }
        if let Some(local) = value.strip_prefix(r"\\?\") {
            return Ok(Cow::Borrowed(local));
        }
    }
    Ok(Cow::Borrowed(value))
}

fn unique_run_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
    format!("sandbox-{}-{nanos}-{sequence}", std::process::id())
}

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successive_run_ids_are_distinct() {
        assert_ne!(unique_run_id(), unique_run_id());
    }

    #[test]
    fn normalized_path_canonicalizes_existing_lexical_aliases() {
        let root = std::env::temp_dir().join(unique_run_id());
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        let alias = root.join("child").join("..").join("child");

        assert_eq!(normalized_path(&child).unwrap(), normalized_path(&alias).unwrap());

        let _ = fs::remove_dir_all(root);
    }

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(unique_run_id());
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub fn answer() -> u32 { 41 }\n").unwrap();
        for arguments in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "needle@example.invalid"],
            vec!["config", "user.name", "Needle Test"],
            vec!["add", "src/lib.rs"],
            vec!["commit", "--quiet", "-m", "fixture"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(arguments)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        root
    }

    #[test]
    fn clean_snapshot_materializes_exactly_and_cleans_up() {
        let source = fixture();
        let runs = source.parent().unwrap().join(unique_run_id());
        let expected = capture_git_snapshot(&source).unwrap().1;
        let sandbox = IsolatedCheckout::materialize(&source, &runs).unwrap();
        assert_ne!(sandbox.checkout_root(), source);
        assert_eq!(sandbox.snapshot().source_digest, expected.source_digest);
        let run_root = sandbox.run_root().to_path_buf();
        sandbox.cleanup().unwrap();
        assert!(!run_root.exists());
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(runs);
    }

    #[test]
    fn dirty_binary_and_untracked_snapshot_materialize_exactly() {
        let source = fixture();
        fs::write(source.join("src/lib.rs"), b"pub fn answer() -> u32 { 42 }\n\0").unwrap();
        fs::write(source.join("untracked.bin"), [0_u8, 255, 1, 2]).unwrap();
        let expected = capture_git_snapshot(&source).unwrap().1;
        let runs = source.parent().unwrap().join(unique_run_id());
        let sandbox = IsolatedCheckout::materialize(&source, &runs).unwrap();
        let observed = capture_git_snapshot(sandbox.checkout_root()).unwrap().1;
        assert_eq!(observed.source_digest, expected.source_digest);
        assert_eq!(
            fs::read(sandbox.checkout_root().join("untracked.bin")).unwrap(),
            [0_u8, 255, 1, 2]
        );
        drop(sandbox);
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(runs);
    }

    #[test]
    fn checkout_mutations_cannot_modify_the_active_worktree() {
        let source = fixture();
        let original = fs::read(source.join("src/lib.rs")).unwrap();
        let runs = source.parent().unwrap().join(unique_run_id());
        let sandbox = IsolatedCheckout::materialize(&source, &runs).unwrap();
        fs::write(sandbox.checkout_root().join("src/lib.rs"), b"sandbox mutation\n").unwrap();
        assert_eq!(fs::read(source.join("src/lib.rs")).unwrap(), original);
        drop(sandbox);
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(runs);
    }

    #[test]
    fn cleanup_accepts_an_already_unregistered_worktree_only_after_verification() {
        let source = fixture();
        let runs = source.parent().unwrap().join(unique_run_id());
        let sandbox = IsolatedCheckout::materialize(&source, &runs).unwrap();
        let checkout = sandbox.checkout_root().to_path_buf();
        let run_root = sandbox.run_root().to_path_buf();
        assert!(worktree_registered(&source, &checkout).unwrap());
        unprotect_git_marker(&checkout).unwrap();
        let checkout_git_path = git_cli_path(&checkout).unwrap();
        git(&source, &["worktree", "remove", "--force", checkout_git_path.as_ref()]).unwrap();
        assert!(!worktree_registered(&source, &checkout).unwrap());
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("orphaned-temp-file"), b"temporary").unwrap();

        sandbox.cleanup().unwrap();

        assert!(!run_root.exists());
        assert!(!worktree_registered(&source, &checkout).unwrap());
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(runs);
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_waits_for_a_transient_windows_file_lock() {
        use std::os::windows::fs::OpenOptionsExt;

        let source = fixture();
        let runs = source.parent().unwrap().join(unique_run_id());
        let sandbox = IsolatedCheckout::materialize(&source, &runs).unwrap();
        let run_root = sandbox.run_root().to_path_buf();
        let locked_path = sandbox.temp_root().join("transient-lock.txt");
        fs::write(&locked_path, b"temporary").unwrap();
        let locked_file =
            fs::OpenOptions::new().read(true).share_mode(0).open(&locked_path).unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            drop(locked_file);
        });

        sandbox.cleanup().unwrap();
        releaser.join().unwrap();

        assert!(!run_root.exists());
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(runs);
    }

    #[cfg(windows)]
    #[test]
    fn git_cli_path_removes_windows_extended_length_prefixes() {
        assert_eq!(git_cli_path(Path::new(r"\\?\C:\repo\checkout")).unwrap(), r"C:\repo\checkout");
        assert_eq!(
            git_cli_path(Path::new(r"\\?\UNC\server\share\checkout")).unwrap(),
            r"\\server\share\checkout"
        );
    }
}

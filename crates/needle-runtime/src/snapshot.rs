use needle_core::{
    CanonicalHasher, Digest, NeedResult, REPOSITORY_SNAPSHOT_IDENTITY_REVISION, RepositorySnapshot,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use thiserror::Error;

const MAX_LINEAGE_CACHE_ENTRIES: usize = 64;

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("repository path is not a directory")]
    NotDirectory,
    #[error("Git command failed: {0}")]
    Git(String),
    #[error("repository data is not UTF-8")]
    Utf8,
    #[error("source I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("evidence path is unsafe or outside the cacheable source set: {0}")]
    UnsafeEvidence(String),
    #[error("evidence digest mismatch for {0}")]
    EvidenceDigest(String),
    #[error("claim `{0}` has no evidence")]
    ClaimWithoutEvidence(String),
    #[error("worker result is incomplete: {0}")]
    IncompleteResult(&'static str),
    #[error("worker result contains a duplicate identifier: {0}")]
    DuplicateIdentifier(String),
    #[error("worker result contains a nested protocol marker")]
    NestedMarker,
    #[error("evidence byte range is invalid for {0}")]
    EvidenceRange(String),
    #[error("suggested read is not a source file: {0}")]
    UnsafeSuggestedRead(String),
}

pub fn capture_git_snapshot(root: &Path) -> Result<(PathBuf, RepositorySnapshot), SnapshotError> {
    if !root.is_dir() {
        return Err(SnapshotError::NotDirectory);
    }
    let (top, git_directory, common_path) = discover_git_layout(root)?;
    let head_sha = resolve_head(&git_directory, &common_path)?;
    if head_sha.len() != 40 || !head_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SnapshotError::Git("HEAD is not a 40-character SHA".to_owned()));
    }
    let status = git_bytes(&top, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut tracked_hasher = blake3::Hasher::new();
    tracked_hasher.update(b"needle-tracked-changes\n");
    // Hash semantic index entries rather than the raw index file. Git's index
    // contains worktree-specific stat data, so byte hashing would make an
    // otherwise identical detached worktree impossible to reproduce.
    tracked_hasher.update(&git_bytes(&top, &["ls-files", "--stage", "-z"])?);
    let mut untracked_hasher = blake3::Hasher::new();
    untracked_hasher.update(b"needle-untracked\n");
    let mut records = status.split(|byte| *byte == 0).filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            return Err(SnapshotError::Git("invalid porcelain status record".to_owned()));
        }
        let state = &record[..2];
        let relative =
            std::str::from_utf8(&record[3..]).map_err(|_| SnapshotError::Utf8)?.to_owned();
        let rename_or_copy = state.iter().any(|byte| matches!(byte, b'R' | b'C'));
        let original = rename_or_copy.then(|| records.next()).flatten();
        let hasher = if state == b"??" { &mut untracked_hasher } else { &mut tracked_hasher };
        hasher.update(record);
        hasher.update(b"\0");
        if let Some(original) = original {
            hasher.update(original);
            hasher.update(b"\0");
        }
        let relative_path = safe_relative_path(&relative)?;
        let absolute = top.join(&relative_path);
        if absolute.exists() {
            let canonical = fs::canonicalize(&absolute)?;
            if !canonical.starts_with(&top) || !canonical.is_file() {
                return Err(SnapshotError::UnsafeEvidence(relative));
            }
            hasher.update(&fs::read(canonical)?);
        }
        hasher.update(b"\0");
    }
    let tracked_changes_digest = Digest(*tracked_hasher.finalize().as_bytes());
    let untracked_content_digest = Digest(*untracked_hasher.finalize().as_bytes());
    let repository_id = repository_lineage_id(&top, &common_path, &head_sha)?;
    let mut source_hasher = CanonicalHasher::new(b"source-snapshot");
    source_hasher.field_u16(REPOSITORY_SNAPSHOT_IDENTITY_REVISION);
    source_hasher.field_digest(repository_id);
    source_hasher.field_str(&head_sha);
    source_hasher.field_digest(tracked_changes_digest);
    source_hasher.field_digest(untracked_content_digest);
    let source_digest = source_hasher.finish();
    Ok((
        top,
        RepositorySnapshot {
            identity_revision: REPOSITORY_SNAPSHOT_IDENTITY_REVISION,
            repository_id,
            head_sha,
            tracked_changes_digest,
            untracked_content_digest,
            source_digest,
        },
    ))
}

fn repository_lineage_id(
    root: &Path,
    common_path: &Path,
    head_sha: &str,
) -> Result<Digest, SnapshotError> {
    type CacheKey = (PathBuf, String);
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Digest>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (common_path.to_path_buf(), head_sha.to_owned());
    if let Ok(guard) = cache.lock()
        && let Some(identity) = guard.get(&key)
    {
        return Ok(*identity);
    }

    let output = git_bytes(root, &["rev-list", "--max-parents=0", "HEAD"])?;
    let roots_text = std::str::from_utf8(&output).map_err(|_| SnapshotError::Utf8)?;
    let mut roots = roots_text.split_whitespace().collect::<Vec<_>>();
    if roots.is_empty()
        || roots
            .iter()
            .any(|root| root.len() != 40 || !root.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(SnapshotError::Git(
            "repository lineage has no valid 40-character root commit".to_owned(),
        ));
    }
    roots.sort_unstable();
    roots.dedup();

    let mut hasher = CanonicalHasher::new(b"repository-lineage");
    hasher.field_u16(REPOSITORY_SNAPSHOT_IDENTITY_REVISION);
    for root_commit in roots {
        hasher.field_str(root_commit);
    }
    let identity = hasher.finish();

    if let Ok(mut guard) = cache.lock()
        && guard.len() < MAX_LINEAGE_CACHE_ENTRIES
    {
        guard.insert(key, identity);
    }
    Ok(identity)
}

pub fn validate_need_result(root: &Path, result: &NeedResult) -> Result<(), SnapshotError> {
    validate_need_result_inner(root, result, true)
}

/// Replace worker-supplied evidence digests with digests computed by the
/// trusted parent runtime. The worker is intentionally unable to execute a
/// hashing helper in its read-only sandbox.
pub fn bind_evidence_digests(root: &Path, result: &mut NeedResult) -> Result<(), SnapshotError> {
    let canonical_root = fs::canonicalize(root)?;
    for evidence in &mut result.evidence {
        let relative = safe_relative_path(&evidence.path)?;
        if evidence.path.contains('\\') || !is_source_path(&canonical_root, &evidence.path)? {
            return Err(SnapshotError::UnsafeEvidence(evidence.path.clone()));
        }
        let absolute = canonical_root.join(relative);
        let canonical = fs::canonicalize(&absolute)
            .map_err(|_| SnapshotError::UnsafeEvidence(evidence.path.clone()))?;
        if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
            return Err(SnapshotError::UnsafeEvidence(evidence.path.clone()));
        }
        evidence.content_digest = Digest::blake3(fs::read(canonical)?);
    }
    Ok(())
}

pub fn validate_cached_need_result(root: &Path, result: &NeedResult) -> Result<(), SnapshotError> {
    validate_need_result_inner(root, result, false)
}

fn validate_need_result_inner(
    root: &Path,
    result: &NeedResult,
    require_source_membership: bool,
) -> Result<(), SnapshotError> {
    if !result.complete {
        return Err(SnapshotError::IncompleteResult("worker declared incomplete"));
    }
    if !result.suggested_reads.is_empty() {
        return Err(SnapshotError::IncompleteResult(
            "complete results cannot delegate repository reads to the main model",
        ));
    }
    if !result.uncertainty.is_empty() {
        return Err(SnapshotError::IncompleteResult(
            "complete results cannot contain unresolved claims",
        ));
    }
    if result.summary.trim().is_empty() {
        return Err(SnapshotError::ClaimWithoutEvidence("summary".to_owned()));
    }
    if result.claims.is_empty() {
        return Err(SnapshotError::IncompleteResult("claims"));
    }
    if result.evidence.is_empty() {
        return Err(SnapshotError::IncompleteResult("evidence"));
    }
    let mut claim_ids = BTreeSet::new();
    let mut evidence_ids = BTreeSet::new();
    for claim in &result.claims {
        if [
            claim.id.as_str(),
            claim.kind.as_str(),
            claim.subject.as_str(),
            claim.statement.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty() || has_disallowed_control(value))
        {
            return Err(SnapshotError::IncompleteResult("claim fields"));
        }
        reject_nested_marker([
            claim.id.as_str(),
            claim.kind.as_str(),
            claim.subject.as_str(),
            claim.statement.as_str(),
        ])?;
        if !claim_ids.insert(claim.id.as_str()) {
            return Err(SnapshotError::DuplicateIdentifier(claim.id.clone()));
        }
    }
    for evidence in &result.evidence {
        if evidence.id.trim().is_empty()
            || evidence.path.trim().is_empty()
            || has_disallowed_control(&evidence.id)
            || has_disallowed_control(&evidence.path)
            || evidence.symbol.as_deref().is_some_and(has_disallowed_control)
        {
            return Err(SnapshotError::IncompleteResult("evidence fields"));
        }
        reject_nested_marker([
            evidence.id.as_str(),
            evidence.path.as_str(),
            evidence.symbol.as_deref().unwrap_or_default(),
        ])?;
        if !evidence_ids.insert(evidence.id.as_str()) {
            return Err(SnapshotError::DuplicateIdentifier(evidence.id.clone()));
        }
    }
    reject_nested_marker(std::iter::once(result.summary.as_str()))?;
    reject_nested_marker(result.suggested_reads.iter().map(String::as_str))?;
    reject_nested_marker(result.suggested_commands.iter().map(String::as_str))?;
    reject_nested_marker(result.uncertainty.iter().map(|item| item.statement.as_str()))?;
    if result
        .suggested_reads
        .iter()
        .chain(result.suggested_commands.iter())
        .any(|value| value.trim().is_empty() || has_disallowed_control(value))
        || result
            .uncertainty
            .iter()
            .any(|item| item.statement.trim().is_empty() || has_disallowed_control(&item.statement))
    {
        return Err(SnapshotError::IncompleteResult("suggestions or uncertainty"));
    }
    for claim in &result.claims {
        if claim.evidence_ids.is_empty() {
            return Err(SnapshotError::ClaimWithoutEvidence(claim.id.clone()));
        }
        if claim.evidence_ids.iter().any(|id| !result.evidence.iter().any(|item| &item.id == id)) {
            return Err(SnapshotError::ClaimWithoutEvidence(claim.id.clone()));
        }
    }
    let canonical_root = fs::canonicalize(root)?;
    for evidence in &result.evidence {
        let relative = safe_relative_path(&evidence.path)?;
        if evidence.path.contains('\\')
            || (require_source_membership && !is_source_path(&canonical_root, &evidence.path)?)
        {
            return Err(SnapshotError::UnsafeEvidence(evidence.path.clone()));
        }
        let absolute = canonical_root.join(relative);
        let canonical = fs::canonicalize(&absolute)
            .map_err(|_| SnapshotError::UnsafeEvidence(evidence.path.clone()))?;
        if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
            return Err(SnapshotError::UnsafeEvidence(evidence.path.clone()));
        }
        let bytes = fs::read(canonical)?;
        if Digest::blake3(&bytes) != evidence.content_digest {
            return Err(SnapshotError::EvidenceDigest(evidence.path.clone()));
        }
        match (evidence.byte_start, evidence.byte_end) {
            (Some(start), Some(end))
                if start < end && end <= u64::try_from(bytes.len()).unwrap_or(u64::MAX) => {}
            (None, None) => {}
            _ => return Err(SnapshotError::EvidenceRange(evidence.path.clone())),
        }
    }
    for read in &result.suggested_reads {
        let relative = safe_relative_path(read)?;
        if read.contains('\\')
            || !canonical_root.join(relative).is_file()
            || (require_source_membership && !is_source_path(&canonical_root, read)?)
        {
            return Err(SnapshotError::UnsafeSuggestedRead(read.clone()));
        }
    }
    Ok(())
}

fn reject_nested_marker<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), SnapshotError> {
    if values.into_iter().any(|value| {
        ["@@need", "@@end", "[NEEDLE_CONTEXT]", "[/NEEDLE_CONTEXT]"]
            .iter()
            .any(|marker| value.contains(marker))
    }) {
        Err(SnapshotError::NestedMarker)
    } else {
        Ok(())
    }
}

fn has_disallowed_control(value: &str) -> bool {
    value.chars().any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
}

fn is_source_path(root: &Path, relative: &str) -> Result<bool, SnapshotError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", relative])
        .output()
        .map_err(SnapshotError::Io)?;
    if !output.status.success() {
        return Err(SnapshotError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    let normalized = relative.replace('\\', "/");
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .filter_map(|value| std::str::from_utf8(value).ok())
        .any(|value| value.replace('\\', "/") == normalized))
}

fn safe_relative_path(value: &str) -> Result<PathBuf, SnapshotError> {
    let path = Path::new(value);
    if path.is_absolute()
        || value.is_empty()
        || value.chars().any(char::is_control)
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(SnapshotError::UnsafeEvidence(value.to_owned()));
    }
    Ok(path.to_path_buf())
}

fn discover_git_layout(root: &Path) -> Result<(PathBuf, PathBuf, PathBuf), SnapshotError> {
    let canonical = fs::canonicalize(root)?;
    let mut candidates = canonical.ancestors();
    let top = candidates
        .find(|candidate| candidate.join(".git").exists())
        .ok_or_else(|| SnapshotError::Git("not a Git worktree".to_owned()))?
        .to_path_buf();
    let marker = top.join(".git");
    let git_directory = if marker.is_dir() {
        fs::canonicalize(marker)?
    } else {
        let marker_text = fs::read_to_string(marker)?;
        let path = marker_text
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .ok_or_else(|| SnapshotError::Git("invalid .git worktree marker".to_owned()))?;
        let candidate = PathBuf::from(path);
        fs::canonicalize(if candidate.is_absolute() { candidate } else { top.join(candidate) })?
    };
    let common_marker = git_directory.join("commondir");
    let common_directory = if common_marker.is_file() {
        let value = fs::read_to_string(common_marker)?;
        let candidate = PathBuf::from(value.trim());
        fs::canonicalize(if candidate.is_absolute() {
            candidate
        } else {
            git_directory.join(candidate)
        })?
    } else {
        git_directory.clone()
    };
    Ok((top, git_directory, common_directory))
}

fn resolve_head(git_directory: &Path, common_directory: &Path) -> Result<String, SnapshotError> {
    let head = fs::read_to_string(git_directory.join("HEAD"))?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ").map(str::trim) {
        for base in [git_directory, common_directory] {
            let path = base.join(reference);
            if path.is_file() {
                return Ok(fs::read_to_string(path)?.trim().to_owned());
            }
        }
        let packed = common_directory.join("packed-refs");
        if packed.is_file() {
            for line in fs::read_to_string(packed)?.lines() {
                let mut fields = line.split_whitespace();
                if let (Some(digest), Some(name)) = (fields.next(), fields.next())
                    && name == reference
                {
                    return Ok(digest.to_owned());
                }
            }
        }
        return Err(SnapshotError::Git(format!("cannot resolve HEAD reference {reference}")));
    }
    Ok(head.to_owned())
}

fn git_bytes(root: &Path, arguments: &[&str]) -> Result<Vec<u8>, SnapshotError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(SnapshotError::Io)?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(SnapshotError::Git(diagnostic));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        Claim, EvidenceReference, LEGACY_REPOSITORY_SNAPSHOT_IDENTITY_REVISION, Uncertainty,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(label: &str) -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("needle-{label}-{}-{suffix}", std::process::id()))
    }

    fn committed_repository() -> PathBuf {
        let root = unique_root("snapshot");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub fn answer() -> u32 { 42 }\n").unwrap();
        for arguments in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "needle@example.invalid"],
            vec!["config", "user.name", "Needle Test"],
            vec!["add", "src/lib.rs"],
            vec!["commit", "--quiet", "-m", "fixture"],
        ] {
            let status = Command::new("git").arg("-C").arg(&root).args(arguments).status().unwrap();
            assert!(status.success());
        }
        root
    }

    #[test]
    fn snapshot_identity_is_stable_across_clones_and_dirty_overlays() {
        let source = committed_repository();
        let clone = unique_root("snapshot-clone");
        let status = Command::new("git")
            .args(["clone", "--quiet", "--no-local"])
            .arg(&source)
            .arg(&clone)
            .status()
            .unwrap();
        assert!(status.success());

        let source_clean = capture_git_snapshot(&source).unwrap().1;
        let clone_clean = capture_git_snapshot(&clone).unwrap().1;
        assert_eq!(source_clean.identity_revision, REPOSITORY_SNAPSHOT_IDENTITY_REVISION);
        assert_eq!(clone_clean.identity_revision, REPOSITORY_SNAPSHOT_IDENTITY_REVISION);
        assert_eq!(source_clean.repository_id, clone_clean.repository_id);
        assert_eq!(source_clean.source_digest, clone_clean.source_digest);

        for root in [&source, &clone] {
            fs::write(root.join("src/lib.rs"), b"pub fn answer() -> u32 { 43 }\n").unwrap();
            fs::write(root.join("notes.bin"), [0_u8, 255, 1, 2]).unwrap();
        }
        let source_dirty = capture_git_snapshot(&source).unwrap().1;
        let clone_dirty = capture_git_snapshot(&clone).unwrap().1;
        assert_eq!(source_dirty.repository_id, clone_dirty.repository_id);
        assert_eq!(source_dirty.source_digest, clone_dirty.source_digest);
        assert_ne!(source_clean.source_digest, source_dirty.source_digest);

        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(clone);
    }

    #[test]
    fn legacy_snapshot_json_defaults_to_path_scoped_revision() {
        let value = serde_json::json!({
            "repository_id": Digest::blake3("repository"),
            "head_sha": "0".repeat(40),
            "tracked_changes_digest": Digest::blake3("tracked"),
            "untracked_content_digest": Digest::blake3("untracked"),
            "source_digest": Digest::blake3("source"),
        });
        let snapshot: RepositorySnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(snapshot.identity_revision, LEGACY_REPOSITORY_SNAPSHOT_IDENTITY_REVISION);
    }

    #[test]
    fn parent_runtime_replaces_untrusted_worker_digest() {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string();
        let root = std::env::temp_dir().join(format!("needle-bind-evidence-{suffix}"));
        fs::create_dir_all(root.join("src")).unwrap();
        let source = b"pub fn answer() -> u32 { 42 }\n";
        fs::write(root.join("src/lib.rs"), source).unwrap();
        for arguments in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "needle@example.invalid"],
            vec!["config", "user.name", "Needle Test"],
            vec!["add", "src/lib.rs"],
        ] {
            let status = Command::new("git").arg("-C").arg(&root).args(arguments).status().unwrap();
            assert!(status.success());
        }
        let mut result = NeedResult {
            complete: true,
            summary: "Verified answer implementation.".to_owned(),
            claims: vec![Claim {
                id: "claim-1".to_owned(),
                kind: "implementation".to_owned(),
                subject: "answer".to_owned(),
                statement: "The implementation returns 42.".to_owned(),
                evidence_ids: vec!["evidence-1".to_owned()],
            }],
            evidence: vec![EvidenceReference {
                id: "evidence-1".to_owned(),
                path: "src/lib.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                content_digest: Digest([0; 32]),
                byte_start: None,
                byte_end: None,
            }],
            suggested_reads: Vec::new(),
            suggested_commands: Vec::new(),
            uncertainty: Vec::<Uncertainty>::new(),
        };

        bind_evidence_digests(&root, &mut result).unwrap();

        assert_eq!(result.evidence[0].content_digest, Digest::blake3(source));
        validate_need_result(&root, &result).unwrap();
        let brief = result.render_evidence_brief(
            &needle_core::NeedKey::new("trace.state-flow").unwrap(),
            "generated",
        );
        assert!(!brief.contains("Suggested spot-checks"));
        assert!(!brief.contains("- read:"));
        assert!(brief.contains("Usa solo questo brief; non ispezionare il repository."));

        let mut delegated_read = result.clone();
        delegated_read.suggested_reads = vec!["src/lib.rs".to_owned()];
        assert!(matches!(
            validate_need_result(&root, &delegated_read),
            Err(SnapshotError::IncompleteResult(_))
        ));

        let mut incomplete = result;
        incomplete.complete = false;
        assert!(matches!(
            validate_need_result(&root, &incomplete),
            Err(SnapshotError::IncompleteResult(_))
        ));
        let _ = fs::remove_dir_all(root);
    }
}

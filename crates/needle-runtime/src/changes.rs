use crate::{PatchFileBlob, RuntimeStore, StoreError, capture_git_snapshot};
use needle_core::{
    CanonicalHasher, ChangeApplyId, ChangeApplyRecord, ChangeApplyStatus, ChangeId, Digest,
    PatchArtifact, PatchOperation, VerificationStatus,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChangeMaterializationError {
    #[error("patch artifact is not canonical")]
    NonCanonical,
    #[error("patch path is unsafe: {0}")]
    UnsafePath(String),
    #[error("patch path contains a symlink: {0}")]
    Symlink(String),
    #[error("patch base does not match: {0}")]
    BaseMismatch(String),
    #[error("patch blob is invalid: {0}")]
    InvalidBlob(String),
    #[error("patch I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ChangeApplyError {
    #[error("change does not exist")]
    NotFound,
    #[error("change is not verified for its latest patch")]
    NotVerified,
    #[error("change digest does not match If-Match")]
    DigestMismatch,
    #[error("active source snapshot drifted from the prepared base")]
    SnapshotDrift,
    #[error("pending apply recovery failed: {0}")]
    Recovery(String),
    #[error(transparent)]
    Materialization(#[from] ChangeMaterializationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("snapshot capture failed: {0}")]
    Snapshot(#[from] crate::SnapshotError),
    #[error("apply I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

pub fn materialize_patch_artifact(
    root: &Path,
    patch: &PatchArtifact,
    blobs: &[PatchFileBlob],
) -> Result<(), ChangeMaterializationError> {
    validate_patch_artifact_base(root, patch, blobs)?;
    let blob_by_path =
        blobs.iter().map(|blob| (blob.path.as_str(), blob)).collect::<BTreeMap<_, _>>();
    let targets = patch
        .files
        .iter()
        .map(|file| {
            let relative = safe_relative(&file.path)?;
            let blob = blob_by_path
                .get(file.path.as_str())
                .ok_or_else(|| ChangeMaterializationError::InvalidBlob(file.path.clone()))?;
            Ok((file, *blob, root.join(relative)))
        })
        .collect::<Result<Vec<_>, ChangeMaterializationError>>()?;

    for (file, blob, target) in &targets {
        match file.operation {
            PatchOperation::Create => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                    reject_symlink_ancestors(root, parent.strip_prefix(root).unwrap_or(parent))?;
                }
                fs::write(target, blob.after.as_deref().expect("create blob was validated"))?;
            }
            PatchOperation::Update => {
                fs::write(target, blob.after.as_deref().expect("update blob was validated"))?;
            }
            PatchOperation::Delete => fs::remove_file(target)?,
        }
    }
    for (file, _, target) in targets {
        match file.after_digest {
            Some(expected) => {
                let bytes = fs::read(&target)?;
                if Digest::blake3(&bytes) != expected {
                    return Err(ChangeMaterializationError::InvalidBlob(file.path.clone()));
                }
            }
            None if target.exists() => {
                return Err(ChangeMaterializationError::InvalidBlob(file.path.clone()));
            }
            None => {}
        }
    }
    Ok(())
}

pub fn validate_patch_artifact_base(
    root: &Path,
    patch: &PatchArtifact,
    blobs: &[PatchFileBlob],
) -> Result<(), ChangeMaterializationError> {
    if patch.id != PatchArtifact::compute_id(patch.source_snapshot, &patch.files)
        || patch.files.len() != blobs.len()
    {
        return Err(ChangeMaterializationError::NonCanonical);
    }
    let blob_by_path =
        blobs.iter().map(|blob| (blob.path.as_str(), blob)).collect::<BTreeMap<_, _>>();
    for file in &patch.files {
        let relative = safe_relative(&file.path)?;
        reject_symlink_ancestors(root, &relative)?;
        let target = root.join(&relative);
        let blob = blob_by_path
            .get(file.path.as_str())
            .ok_or_else(|| ChangeMaterializationError::InvalidBlob(file.path.clone()))?;
        validate_blob(file.before_digest, blob.before.as_deref(), &file.path)?;
        validate_blob(file.after_digest, blob.after.as_deref(), &file.path)?;
        validate_base(&target, file.operation, file.before_digest, &file.path)?;
    }
    Ok(())
}

pub fn apply_verified_change(
    store: &RuntimeStore,
    repository_root: &Path,
    change_id: &ChangeId,
    expected_change_digest: Digest,
) -> Result<ChangeApplyRecord, ChangeApplyError> {
    let repository_root = fs::canonicalize(repository_root)?;
    let repository_text = repository_root.to_string_lossy().into_owned();
    recover_pending_change_applies(store, &repository_root)?;
    let prepared = store.prepared_change(change_id)?.ok_or(ChangeApplyError::NotFound)?;
    store
        .latest_verification_artifact(change_id)?
        .filter(|artifact| {
            artifact.patch_id == prepared.patch.id
                && artifact.verdict == VerificationStatus::Verified
                && artifact.is_canonical()
        })
        .ok_or(ChangeApplyError::NotVerified)?;
    if prepared.state != "verified" {
        return Err(ChangeApplyError::NotVerified);
    }
    if store.change_digest(change_id)? != Some(expected_change_digest) {
        return Err(ChangeApplyError::DigestMismatch);
    }
    let (_, active_snapshot) = capture_git_snapshot(&repository_root)?;
    if active_snapshot.source_digest != prepared.source_snapshot {
        return Err(ChangeApplyError::SnapshotDrift);
    }
    let blobs = store.patch_file_blobs(prepared.patch.id)?;
    validate_patch_artifact_base(&repository_root, &prepared.patch, &blobs)?;
    let created = now_ms();
    let mut hasher = CanonicalHasher::new(b"needle-change-apply");
    hasher.field_str(change_id.as_str());
    hasher.field_digest(prepared.patch.id.0);
    hasher.field_digest(active_snapshot.source_digest);
    hasher.field_bytes(&created.to_le_bytes());
    let apply_id = ChangeApplyId(hasher.finish());
    let mut record = ChangeApplyRecord {
        id: apply_id,
        change_id: change_id.clone(),
        patch_id: prepared.patch.id,
        repository_root: repository_text,
        pre_snapshot: active_snapshot.source_digest,
        post_snapshot: None,
        status: ChangeApplyStatus::Applying,
        created_unix_ms: created,
        completed_unix_ms: None,
    };
    let journal = serde_json::json!({
        "patch_id": prepared.patch.id,
        "paths": prepared.patch.files.iter().map(|file| &file.path).collect::<Vec<_>>()
    });
    store.begin_change_apply(&record, &journal, expected_change_digest)?;
    let applied = materialize_patch_artifact(&repository_root, &prepared.patch, &blobs)
        .map_err(ChangeApplyError::from)
        .and_then(|_| capture_git_snapshot(&repository_root).map_err(ChangeApplyError::from));
    match applied {
        Ok((_, post_snapshot)) => {
            let completed = now_ms();
            store.finish_change_apply(
                apply_id,
                ChangeApplyStatus::Applied,
                Some(post_snapshot.source_digest),
                completed,
            )?;
            record.status = ChangeApplyStatus::Applied;
            record.post_snapshot = Some(post_snapshot.source_digest);
            record.completed_unix_ms = Some(completed);
            Ok(record)
        }
        Err(error) => {
            let rollback = rollback_patch_and_verify(
                &repository_root,
                &prepared.patch,
                &blobs,
                active_snapshot.source_digest,
            );
            let completed = now_ms();
            let status = if rollback.is_ok() {
                ChangeApplyStatus::RolledBack
            } else {
                ChangeApplyStatus::RollbackFailed
            };
            store.finish_change_apply(apply_id, status, None, completed)?;
            match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(ChangeApplyError::Recovery(format!(
                    "{error}; rollback failed: {rollback_error}"
                ))),
            }
        }
    }
}

pub fn recover_pending_change_applies(
    store: &RuntimeStore,
    repository_root: &Path,
) -> Result<(), ChangeApplyError> {
    let repository_root = fs::canonicalize(repository_root)?;
    let repository_text = repository_root.to_string_lossy().into_owned();
    for pending in store.pending_change_applies(&repository_text)? {
        let prepared =
            store.prepared_change(&pending.change_id)?.ok_or(ChangeApplyError::NotFound)?;
        if prepared.patch.id != pending.patch_id {
            store.finish_change_apply(
                pending.id,
                ChangeApplyStatus::RecoveryConflict,
                None,
                now_ms(),
            )?;
            return Err(ChangeApplyError::Recovery(
                "pending apply references a different latest patch".to_owned(),
            ));
        }
        let blobs = store.patch_file_blobs(pending.patch_id)?;
        match rollback_patch_and_verify(
            &repository_root,
            &prepared.patch,
            &blobs,
            pending.pre_snapshot,
        ) {
            Ok(()) => store.finish_change_apply(
                pending.id,
                ChangeApplyStatus::RolledBack,
                None,
                now_ms(),
            )?,
            Err(error) => {
                store.finish_change_apply(
                    pending.id,
                    ChangeApplyStatus::RollbackFailed,
                    None,
                    now_ms(),
                )?;
                return Err(ChangeApplyError::Recovery(error.to_string()));
            }
        }
    }
    Ok(())
}

fn rollback_patch_and_verify(
    root: &Path,
    patch: &PatchArtifact,
    blobs: &[PatchFileBlob],
    expected_snapshot: Digest,
) -> Result<(), ChangeApplyError> {
    rollback_patch(root, patch, blobs)?;
    let (_, restored) = capture_git_snapshot(root)?;
    if restored.source_digest != expected_snapshot {
        return Err(ChangeApplyError::Recovery(
            "rollback did not restore the pre-apply source snapshot".to_owned(),
        ));
    }
    Ok(())
}

fn rollback_patch(
    root: &Path,
    patch: &PatchArtifact,
    blobs: &[PatchFileBlob],
) -> Result<(), ChangeMaterializationError> {
    let by_path = blobs.iter().map(|blob| (blob.path.as_str(), blob)).collect::<BTreeMap<_, _>>();
    for file in patch.files.iter().rev() {
        let relative = safe_relative(&file.path)?;
        reject_symlink_ancestors(root, &relative)?;
        let target = root.join(relative);
        let blob = by_path
            .get(file.path.as_str())
            .ok_or_else(|| ChangeMaterializationError::InvalidBlob(file.path.clone()))?;
        match file.operation {
            PatchOperation::Create => {
                if target.exists() {
                    if fs::symlink_metadata(&target)?.file_type().is_symlink() {
                        return Err(ChangeMaterializationError::Symlink(file.path.clone()));
                    }
                    fs::remove_file(&target)?;
                }
            }
            PatchOperation::Update | PatchOperation::Delete => {
                let before = blob
                    .before
                    .as_deref()
                    .ok_or_else(|| ChangeMaterializationError::InvalidBlob(file.path.clone()))?;
                validate_blob(file.before_digest, Some(before), &file.path)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, before)?;
            }
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn validate_blob(
    expected: Option<Digest>,
    bytes: Option<&[u8]>,
    path: &str,
) -> Result<(), ChangeMaterializationError> {
    match (expected, bytes) {
        (None, None) => Ok(()),
        (Some(expected), Some(bytes))
            if Digest::blake3(bytes) == expected
                && !bytes.contains(&0)
                && std::str::from_utf8(bytes).is_ok() =>
        {
            Ok(())
        }
        _ => Err(ChangeMaterializationError::InvalidBlob(path.to_owned())),
    }
}

fn validate_base(
    target: &Path,
    operation: PatchOperation,
    expected: Option<Digest>,
    display: &str,
) -> Result<(), ChangeMaterializationError> {
    match operation {
        PatchOperation::Create if !target.exists() && expected.is_none() => Ok(()),
        PatchOperation::Update | PatchOperation::Delete => {
            let metadata = fs::symlink_metadata(target)
                .map_err(|_| ChangeMaterializationError::BaseMismatch(display.to_owned()))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(ChangeMaterializationError::BaseMismatch(display.to_owned()));
            }
            let bytes = fs::read(target)?;
            if expected == Some(Digest::blake3(bytes)) {
                Ok(())
            } else {
                Err(ChangeMaterializationError::BaseMismatch(display.to_owned()))
            }
        }
        _ => Err(ChangeMaterializationError::BaseMismatch(display.to_owned())),
    }
}

fn safe_relative(value: &str) -> Result<PathBuf, ChangeMaterializationError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(ChangeMaterializationError::UnsafePath(value.to_owned()));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(ChangeMaterializationError::UnsafePath(value.to_owned()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| ChangeMaterializationError::UnsafePath(value.to_owned()))?;
        if matches!(
            component.to_ascii_lowercase().as_str(),
            ".git"
                | ".needle"
                | ".codegraph"
                | ".cache"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
        ) {
            return Err(ChangeMaterializationError::UnsafePath(value.to_owned()));
        }
        output.push(component);
    }
    Ok(output)
}

fn reject_symlink_ancestors(
    root: &Path,
    relative: &Path,
) -> Result<(), ChangeMaterializationError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(ChangeMaterializationError::UnsafePath(relative.display().to_string()));
        };
        current.push(component);
        if current.exists() && fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(ChangeMaterializationError::Symlink(relative.display().to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{ChangeId, PatchFile};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn materializes_exact_text_patch_and_rejects_replay() {
        let root = temporary_root();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "before\n").unwrap();
        let file = PatchFile {
            path: "src/lib.rs".to_owned(),
            operation: PatchOperation::Update,
            before_digest: Some(Digest::blake3(b"before\n")),
            after_digest: Some(Digest::blake3(b"after\n")),
            before_bytes: 7,
            after_bytes: 6,
        };
        let source = Digest::blake3(b"source");
        let patch = PatchArtifact {
            id: PatchArtifact::compute_id(source, std::slice::from_ref(&file)),
            change_id: ChangeId::from_digest(Digest::blake3(b"change")),
            revision: 1,
            source_snapshot: source,
            files: vec![file],
            summary: "summary".to_owned(),
            acceptance_coverage: Vec::new(),
            residual_risks: Vec::new(),
            declared_output_digest: Digest::blake3(b"output"),
            discrepancies: Vec::new(),
        };
        let blobs = vec![PatchFileBlob {
            path: "src/lib.rs".to_owned(),
            before: Some(b"before\n".to_vec()),
            after: Some(b"after\n".to_vec()),
        }];
        materialize_patch_artifact(&root, &patch, &blobs).unwrap();
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), "after\n");
        assert!(matches!(
            materialize_patch_artifact(&root, &patch, &blobs),
            Err(ChangeMaterializationError::BaseMismatch(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_root() -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("needle-materialize-{}-{suffix}", std::process::id()))
    }
}

use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const MANAGED_MARKER: &str =
    "<!-- Managed by Needle. Run `needle enable` to refresh this skill. -->";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SkillInstallation {
    pub path: PathBuf,
    pub installed: bool,
    pub managed: bool,
    pub current: bool,
    pub changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SkillRemoval {
    pub path: PathBuf,
    pub removed: bool,
    pub unmanaged_preserved: bool,
}

pub(crate) fn inspect() -> Result<SkillInstallation, String> {
    let root = personal_skills_root()?;
    let executable = current_executable()?;
    inspect_at(&root, &executable)
}

pub(crate) fn ensure_installed() -> Result<SkillInstallation, String> {
    let root = personal_skills_root()?;
    let executable = current_executable()?;
    ensure_installed_at(&root, &executable)
}

pub(crate) fn remove_managed() -> Result<SkillRemoval, String> {
    let root = personal_skills_root()?;
    let executable = current_executable()?;
    remove_managed_at(&root, &executable)
}

fn personal_skills_root() -> Result<PathBuf, String> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(".agents").join("skills"))
        .ok_or_else(|| "the personal Codex skills directory is unavailable".to_owned())
}

fn current_executable() -> Result<PathBuf, String> {
    env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("cannot resolve the Needle executable: {error}"))
}

fn inspect_at(root: &Path, executable: &Path) -> Result<SkillInstallation, String> {
    let path = root.join("needle").join("SKILL.md");
    if !path.is_file() {
        return Ok(SkillInstallation {
            path,
            installed: false,
            managed: false,
            current: false,
            changed: false,
        });
    }
    let existing = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let managed = existing.contains(MANAGED_MARKER);
    let current = managed && existing == render_skill(executable);
    Ok(SkillInstallation { path, installed: true, managed, current, changed: false })
}

fn ensure_installed_at(root: &Path, executable: &Path) -> Result<SkillInstallation, String> {
    let mut status = inspect_at(root, executable)?;
    if status.installed && !status.managed {
        return Err(format!(
            "{} already exists and is not managed by Needle; move or rename it before enabling the Codex Desktop integration",
            status.path.display()
        ));
    }
    if status.current {
        return Ok(status);
    }
    let parent = status
        .path
        .parent()
        .ok_or_else(|| "the Needle skill path has no parent directory".to_owned())?;
    if parent.exists() && !status.installed {
        let mut entries = fs::read_dir(parent)
            .map_err(|error| format!("cannot inspect {}: {error}", parent.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "{} already exists and contains files not managed by Needle",
                parent.display()
            ));
        }
    }
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    fs::write(&status.path, render_skill(executable))
        .map_err(|error| format!("cannot write {}: {error}", status.path.display()))?;
    status.installed = true;
    status.managed = true;
    status.current = true;
    status.changed = true;
    Ok(status)
}

fn remove_managed_at(root: &Path, executable: &Path) -> Result<SkillRemoval, String> {
    let status = inspect_at(root, executable)?;
    if !status.installed {
        return Ok(SkillRemoval { path: status.path, removed: false, unmanaged_preserved: false });
    }
    if !status.managed {
        return Ok(SkillRemoval { path: status.path, removed: false, unmanaged_preserved: true });
    }

    fs::remove_file(&status.path)
        .map_err(|error| format!("cannot remove {}: {error}", status.path.display()))?;
    if let Some(parent) = status.path.parent() {
        let empty = fs::read_dir(parent)
            .map_err(|error| format!("cannot inspect {}: {error}", parent.display()))?
            .next()
            .is_none();
        if empty {
            fs::remove_dir(parent)
                .map_err(|error| format!("cannot remove empty {}: {error}", parent.display()))?;
        }
    }
    Ok(SkillRemoval { path: status.path, removed: true, unmanaged_preserved: false })
}

fn render_skill(executable: &Path) -> String {
    let executable = executable.display();
    format!(
        r#"---
name: needle
description: In Codex Desktop, use Needle for repository tasks that require locating an implementation, tracing runtime or state flow, or identifying relevant tests.
---

# Needle repository exploration

{MANAGED_MARKER}

Use this skill only in Codex Desktop. Codex CLI uses Needle lifecycle hooks instead and must not also invoke this skill. In Desktop, invoke Needle before native repository exploration whenever the current task requires locating an implementation, tracing runtime behavior or state transitions, or identifying relevant tests. Do not skip Needle merely because native exploration could answer the same question. Do not invoke it for general questions, trivial file reads, or work outside a Git repository.

Needle is opt-in and repository-aware. The exploration command checks activation itself. Make one Needle invocation for the selected route. Continue natively only if Needle reports that it is disabled, unavailable, or unsuccessful.

Choose exactly one route:

- `locate.implementation` to find the primary implementation and its important callers or dependencies.
- `trace.state-flow` to trace runtime behavior, control flow, or state transitions.
- `tests.relevant` to identify the smallest relevant tests and what they cover.

Identify one exact exploration subject and classify it as `symbol`, `cli-option`, `configuration-key`, `test`, `file`, `module`, or `behavior`. Use a stable canonical name that appears literally in the target source, not the full user prompt or a descriptive phrase. Prefer an exact symbol when one is known. For a state or control-flow question without a narrower symbol, use `behavior` with one short source token, such as `activation`, rather than a phrase such as `activation precedence`.

Invoke the executable at `{executable}` with this argument shape:

`explore --route <route> --subject-kind <subject-kind> --subject <canonical-subject> --repository <absolute-repository-path>`

Do not add `--query`. Needle derives a deterministic route-specific exploration request from the structured route and subject so equivalent invocations can safely reuse an exact local result. The original user prompt remains with you and is not replaced by Needle's canonical exploration request.

Quote every argument for the active shell. Set the shell execution timeout to at least 360 seconds and wait until the Needle process exits; a first uncached exploration can take more than one minute. If the shell tool yields a running process or cell, keep waiting for that same process instead of starting another invocation. Needle writes progress to stderr and emits the bounded context on stdout only after resolution completes.

Never ask the user to write an internal marker and never expose Needle's internal marker protocol. Treat stdout as bounded repository context, verify critical claims against source when necessary, and then continue the original task. If Needle exits unsuccessfully, report only the actionable failure and continue with native exploration when safe.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        env::temp_dir().join(format!("needle-skill-{name}-{nonce}"))
    }

    #[test]
    fn installation_is_idempotent_and_tracks_the_executable() {
        let root = temporary_root("idempotent");
        let first_executable = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        let second_executable =
            root.join("new").join(if cfg!(windows) { "needle.exe" } else { "needle" });

        let first = ensure_installed_at(&root, &first_executable).unwrap();
        let unchanged = ensure_installed_at(&root, &first_executable).unwrap();
        let updated = ensure_installed_at(&root, &second_executable).unwrap();

        assert!(first.changed);
        assert!(!unchanged.changed);
        assert!(updated.changed);
        let contents = fs::read_to_string(updated.path).unwrap();
        assert!(contents.contains(MANAGED_MARKER));
        assert!(contents.contains(&second_executable.display().to_string()));
        assert!(contents.contains("--subject-kind <subject-kind>"));
        assert!(contents.contains("Do not add `--query`"));
        assert!(contents.contains("one short source token"));
        assert!(contents.contains("timeout to at least 360 seconds"));
        assert!(contents.contains("keep waiting for that same process"));
        assert!(contents.contains("invoke Needle before native repository exploration"));
        assert!(contents.contains("Do not skip Needle merely because native exploration"));
        assert!(!contents.contains("@@need"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_refuses_to_overwrite_an_unmanaged_skill() {
        let root = temporary_root("collision");
        let skill = root.join("needle");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: needle\n---\nUser content.\n").unwrap();

        let error = ensure_installed_at(&root, Path::new("needle")).unwrap_err();
        assert!(error.contains("not managed by Needle"));
        assert!(fs::read_to_string(skill.join("SKILL.md")).unwrap().contains("User content"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_skill_removal_is_safe_and_idempotent() {
        let root = temporary_root("remove-managed");
        let executable = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        let installation = ensure_installed_at(&root, &executable).unwrap();

        let removed = remove_managed_at(&root, &executable).unwrap();
        let repeated = remove_managed_at(&root, &executable).unwrap();

        assert!(removed.removed);
        assert!(!removed.unmanaged_preserved);
        assert!(!installation.path.exists());
        assert!(!repeated.removed);
        assert!(!repeated.unmanaged_preserved);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removal_preserves_unmanaged_skills_and_sibling_files() {
        let root = temporary_root("remove-preserves-user-content");
        let executable = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        let skill = root.join("needle");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: needle\n---\nUser content.\n").unwrap();

        let unmanaged = remove_managed_at(&root, &executable).unwrap();
        assert!(!unmanaged.removed);
        assert!(unmanaged.unmanaged_preserved);
        assert!(skill.join("SKILL.md").is_file());

        fs::remove_file(skill.join("SKILL.md")).unwrap();
        ensure_installed_at(&root, &executable).unwrap();
        fs::write(skill.join("notes.txt"), "preserve me").unwrap();
        let managed = remove_managed_at(&root, &executable).unwrap();
        assert!(managed.removed);
        assert!(!skill.join("SKILL.md").exists());
        assert_eq!(fs::read_to_string(skill.join("notes.txt")).unwrap(), "preserve me");
        fs::remove_dir_all(root).unwrap();
    }
}

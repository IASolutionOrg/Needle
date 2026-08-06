use serde::Serialize;
use serde_json::{Map, Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const EVENTS: [(&str, &str, u64); 6] = [
    ("SessionStart", "session-start", 10),
    ("UserPromptSubmit", "user-prompt-submit", 10),
    ("Stop", "stop", 240),
    ("SessionEnd", "session-end", 10),
    ("PreCompact", "pre-compact", 5),
    ("PostCompact", "post-compact", 5),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct HookRegistration {
    pub path: PathBuf,
    pub registered: bool,
    pub changed: bool,
    pub removed: bool,
    pub trust_review_required: bool,
}

pub(crate) fn ensure_registered() -> Result<HookRegistration, String> {
    let codex_home = codex_home()?;
    let executable = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("cannot resolve the Needle executable: {error}"))?;
    ensure_registered_at(&codex_home, &executable)
}

pub(crate) fn inspect() -> Result<HookRegistration, String> {
    let codex_home = codex_home()?;
    let executable = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("cannot resolve the Needle executable: {error}"))?;
    inspect_at(&codex_home, &executable)
}

pub(crate) fn remove_registered() -> Result<HookRegistration, String> {
    let codex_home = codex_home()?;
    remove_registered_at(&codex_home)
}

fn codex_home() -> Result<PathBuf, String> {
    if let Some(value) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(value));
    }
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(|value| PathBuf::from(value).join(".codex"))
        .ok_or_else(|| "Codex home is unavailable; set CODEX_HOME".to_owned())
}

fn ensure_registered_at(codex_home: &Path, executable: &Path) -> Result<HookRegistration, String> {
    let path = codex_home.join("hooks.json");
    let mut document = if path.is_file() {
        serde_json::from_slice::<Value>(
            &fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?,
        )
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?
    } else {
        json!({"hooks": {}})
    };
    let root = document
        .as_object_mut()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| format!("{}.hooks must contain a JSON object", path.display()))?;
    let executable = executable.to_string_lossy();
    let mut changed = false;
    for (event, action, timeout) in EVENTS {
        let command = format!("\"{executable}\" hook {action}");
        let groups = hooks.entry(event).or_insert_with(|| Value::Array(Vec::new()));
        let groups = groups
            .as_array_mut()
            .ok_or_else(|| format!("{}.hooks.{event} must contain an array", path.display()))?;
        if !contains_command(groups, &command) {
            groups.retain(|group| !is_needle_group(group, action));
            groups.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "commandWindows": command,
                    "timeout": timeout
                }]
            }));
            changed = true;
        }
    }
    if changed {
        fs::create_dir_all(codex_home)
            .map_err(|error| format!("cannot create {}: {error}", codex_home.display()))?;
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("cannot encode Codex hooks: {error}"))?;
        fs::write(&path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(HookRegistration {
        path,
        registered: true,
        changed,
        removed: false,
        trust_review_required: changed,
    })
}

fn inspect_at(codex_home: &Path, executable: &Path) -> Result<HookRegistration, String> {
    let path = codex_home.join("hooks.json");
    if !path.is_file() {
        return Ok(HookRegistration {
            path,
            registered: false,
            changed: false,
            removed: false,
            trust_review_required: false,
        });
    }
    let document: Value = serde_json::from_slice(
        &fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    let executable = executable.to_string_lossy();
    let registered = EVENTS.iter().all(|(event, action, _)| {
        let expected = format!("\"{executable}\" hook {action}");
        document
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array)
            .is_some_and(|groups| contains_command(groups, &expected))
    });
    Ok(HookRegistration {
        path,
        registered,
        changed: false,
        removed: false,
        trust_review_required: false,
    })
}

fn remove_registered_at(codex_home: &Path) -> Result<HookRegistration, String> {
    let path = codex_home.join("hooks.json");
    if !path.is_file() {
        return Ok(HookRegistration {
            path,
            registered: false,
            changed: false,
            removed: false,
            trust_review_required: false,
        });
    }
    let mut document: Value = serde_json::from_slice(
        &fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    let Some(hooks_value) = document.get_mut("hooks") else {
        return Ok(HookRegistration {
            path,
            registered: false,
            changed: false,
            removed: false,
            trust_review_required: false,
        });
    };
    let hooks = hooks_value
        .as_object_mut()
        .ok_or_else(|| format!("{}.hooks must contain a JSON object", path.display()))?;
    let mut changed = false;
    for (event, action, _) in EVENTS {
        let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        let previous = groups.len();
        groups.retain(|group| !is_needle_group(group, action));
        changed |= groups.len() != previous;
    }
    hooks.retain(|_, groups| !groups.as_array().is_some_and(Vec::is_empty));
    if changed {
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("cannot encode Codex hooks: {error}"))?;
        fs::write(&path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(HookRegistration {
        path,
        registered: false,
        changed,
        removed: changed,
        trust_review_required: false,
    })
}

fn is_needle_group(group: &Value, action: &str) -> bool {
    let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    if handlers.len() != 1 {
        return false;
    }
    let Some(handler) = handlers.first() else {
        return false;
    };
    ["command", "commandWindows"].iter().any(|field| {
        handler
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|command| is_needle_command(command, action))
    })
}

fn is_needle_command(command: &str, action: &str) -> bool {
    let suffix = format!(" hook {action}");
    let Some(executable) = command.strip_suffix(&suffix) else {
        return false;
    };
    let executable = executable.trim().trim_matches('"');
    Path::new(executable).file_name().and_then(|name| name.to_str()).is_some_and(|name| {
        name.eq_ignore_ascii_case("needle") || name.eq_ignore_ascii_case("needle.exe")
    })
}

fn contains_command(groups: &[Value], expected: &str) -> bool {
    groups.iter().any(|group| {
        group.get("hooks").and_then(Value::as_array).is_some_and(|handlers| {
            handlers.iter().any(|handler| {
                handler.get("command").and_then(Value::as_str) == Some(expected)
                    || handler.get("commandWindows").and_then(Value::as_str) == Some(expected)
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn registration_preserves_existing_hooks_and_is_idempotent() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let root = env::temp_dir().join(format!("needle-hook-registration-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("hooks.json"),
            serde_json::to_vec(&json!({
                "description": "user hooks",
                "hooks": {"Stop": [
                    {"hooks": [{"type": "command", "command": "user-tool"}]},
                    {"hooks": [{"type": "command", "command": "\"C:\\\\old\\\\needle.exe\" hook stop"}]}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        let executable = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        let first = ensure_registered_at(&root, &executable).unwrap();
        let second = ensure_registered_at(&root, &executable).unwrap();
        assert!(first.changed);
        assert!(first.registered);
        assert!(first.trust_review_required);
        assert!(!second.changed);
        assert!(!second.trust_review_required);
        let document: Value =
            serde_json::from_slice(&fs::read(root.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(document["description"], "user hooks");
        let stop_groups = document["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_groups.len(), 2);
        let commands = stop_groups
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter_map(|handler| handler["command"].as_str())
            .collect::<Vec<_>>();
        assert!(commands.iter().any(|command| command.contains(executable.to_str().unwrap())));
        assert!(commands.iter().all(|command| !command.contains("old")));
        for (event, _, _) in EVENTS {
            assert!(document["hooks"][event].is_array());
        }
        assert!(inspect_at(&root, &executable).unwrap().registered);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removal_preserves_unrelated_hooks() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let root = env::temp_dir().join(format!("needle-hook-removal-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("hooks.json"),
            serde_json::to_vec(&json!({
                "description": "user hooks",
                "hooks": {
                    "Stop": [
                        {"hooks": [{"type": "command", "command": "user-tool"}]},
                        {"hooks": [{"type": "command", "command": "\"C:\\\\old\\\\needle.exe\" hook stop"}]}
                    ],
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": "\"C:\\\\old\\\\needle.exe\" hook session-start"}]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let removal = remove_registered_at(&root).unwrap();
        assert!(removal.changed);
        assert!(removal.removed);
        let document: Value =
            serde_json::from_slice(&fs::read(root.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(document["description"], "user hooks");
        assert_eq!(document["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(document["hooks"]["Stop"][0]["hooks"][0]["command"], "user-tool");
        assert!(document["hooks"].get("SessionStart").is_none());
        fs::remove_dir_all(root).unwrap();
    }
}

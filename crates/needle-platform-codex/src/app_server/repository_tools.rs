use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

const DEFAULT_READ_LINES: usize = 160;
const MAX_READ_LINES: usize = 240;
const MAX_SEARCH_FILES: usize = 2_000;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub(super) struct RepositoryToolOutput {
    pub(super) text: String,
    pub(super) observed_files: Vec<String>,
}

pub(super) fn specs() -> Value {
    json!([
        {
            "type": "function",
            "name": "needle_search",
            "description": "Search for one literal source token inside UTF-8 repository files. Start with the exact subject, then narrow later calls to returned source directories or files. Paths must be repository-relative. Generated files and lockfiles are skipped for directory searches.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {"type": "string", "minLength": 1, "maxLength": 256},
                    "path": {"type": "string", "description": "Repository-relative file or directory; defaults to ."},
                    "case_sensitive": {"type": "boolean"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS}
                }
            }
        },
        {
            "type": "function",
            "name": "needle_read",
            "description": "Read a bounded range of lines from one UTF-8 repository file. The path must be repository-relative.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "start_line": {"type": "integer", "minimum": 1},
                    "line_count": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES}
                }
            }
        },
        {
            "type": "function",
            "name": "needle_list",
            "description": "List the direct children of one repository-relative directory. Output is bounded and sorted.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string", "description": "Repository-relative directory; defaults to ."}
                }
            }
        }
    ])
}

pub(super) fn execute(
    tool: &str,
    arguments: &Value,
    repository_root: &Path,
) -> Result<RepositoryToolOutput, String> {
    let arguments =
        arguments.as_object().ok_or_else(|| "tool arguments must be a JSON object".to_owned())?;
    match tool {
        "needle_search" => search(arguments, repository_root),
        "needle_read" => read(arguments, repository_root),
        "needle_list" => list(arguments, repository_root),
        _ => Err(format!("unknown Needle repository tool: {tool}")),
    }
}

fn search(
    arguments: &serde_json::Map<String, Value>,
    root: &Path,
) -> Result<RepositoryToolOutput, String> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| "query must contain 1 to 256 bytes".to_owned())?;
    let target = resolve_path(root, string_argument(arguments, "path").unwrap_or("."))?;
    let case_sensitive = arguments.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false);
    let max_results = usize_argument(arguments, "max_results", 20).clamp(1, MAX_SEARCH_RESULTS);
    let needle = (!case_sensitive).then(|| query.to_lowercase());
    let mut files = Vec::new();
    collect_files(&target, &mut files)?;
    files.sort_by_key(|path| (search_priority(path), path.clone()));

    let mut output = String::new();
    let mut observed = BTreeSet::new();
    let mut matches = 0usize;
    let mut visited = 0usize;
    for file in files.into_iter().take(MAX_SEARCH_FILES) {
        visited += 1;
        if fs::metadata(&file).map(|metadata| metadata.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&file) else {
            continue;
        };
        let Some(relative) = relative_file(root, &file) else {
            continue;
        };
        for (index, line) in contents.lines().enumerate() {
            let found = match needle.as_ref() {
                Some(needle) => line.to_lowercase().contains(needle),
                None => line.contains(query),
            };
            if !found {
                continue;
            }
            let line = bound_line(line, 500);
            let entry = format!("{relative}:{}:{line}\n", index + 1);
            if output.len().saturating_add(entry.len()) > MAX_OUTPUT_BYTES {
                output.push_str("[output truncated]\n");
                return Ok(RepositoryToolOutput {
                    text: output,
                    observed_files: observed.into_iter().collect(),
                });
            }
            output.push_str(&entry);
            observed.insert(relative.clone());
            matches += 1;
            if matches >= max_results {
                output.push_str("[result limit reached]\n");
                return Ok(RepositoryToolOutput {
                    text: output,
                    observed_files: observed.into_iter().collect(),
                });
            }
        }
    }
    if output.is_empty() {
        output = format!("No matches in {visited} inspected files.\n");
    }
    Ok(RepositoryToolOutput { text: output, observed_files: observed.into_iter().collect() })
}

fn read(
    arguments: &serde_json::Map<String, Value>,
    root: &Path,
) -> Result<RepositoryToolOutput, String> {
    let raw_path = string_argument(arguments, "path")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "path is required".to_owned())?;
    let target = resolve_path(root, raw_path)?;
    if !target.is_file() {
        return Err("path is not a file".to_owned());
    }
    if fs::metadata(&target).map_err(|error| error.to_string())?.len() > MAX_FILE_BYTES {
        return Err("file exceeds the 1 MiB inspection limit".to_owned());
    }
    let contents = fs::read_to_string(&target)
        .map_err(|error| format!("cannot read UTF-8 repository file: {error}"))?;
    let start = usize_argument(arguments, "start_line", 1).max(1);
    let count =
        usize_argument(arguments, "line_count", DEFAULT_READ_LINES).clamp(1, MAX_READ_LINES);
    let relative = relative_file(root, &target)
        .ok_or_else(|| "resolved file is outside the isolated repository".to_owned())?;
    let mut output = format!("path={relative}\n");
    for (index, line) in contents.lines().enumerate().skip(start - 1).take(count) {
        let entry = format!("{}: {}\n", index + 1, bound_line(line, 2_000));
        if output.len().saturating_add(entry.len()) > MAX_OUTPUT_BYTES {
            output.push_str("[output truncated]\n");
            break;
        }
        output.push_str(&entry);
    }
    Ok(RepositoryToolOutput { text: output, observed_files: vec![relative] })
}

fn list(
    arguments: &serde_json::Map<String, Value>,
    root: &Path,
) -> Result<RepositoryToolOutput, String> {
    let target = resolve_path(root, string_argument(arguments, "path").unwrap_or("."))?;
    if !target.is_dir() {
        return Err("path is not a directory".to_owned());
    }
    let mut entries = fs::read_dir(&target)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != ".git")
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut output = String::new();
    for entry in entries.into_iter().take(200) {
        let path = entry.path();
        let relative = relative_path(root, &path)
            .ok_or_else(|| "directory entry escaped the isolated repository".to_owned())?;
        output.push_str(&relative);
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            output.push('/');
        }
        output.push('\n');
    }
    if output.is_empty() {
        output.push_str("[empty directory]\n");
    }
    Ok(RepositoryToolOutput { text: output, observed_files: Vec::new() })
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    if files.len() >= MAX_SEARCH_FILES {
        return Ok(());
    }
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    let mut entries = fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if files.len() >= MAX_SEARCH_FILES {
            break;
        }
        if entry.file_name() == ".git" {
            continue;
        }
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if ignored_directory(&entry.file_name().to_string_lossy()) {
                continue;
            }
            collect_files(&entry.path(), files)?;
        } else if kind.is_file() && searchable_file(&entry.path()) {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn resolve_path(root: &Path, value: &str) -> Result<PathBuf, String> {
    let relative = Path::new(value);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err("path must stay repository-relative".to_owned());
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let target = fs::canonicalize(canonical_root.join(relative))
        .map_err(|error| format!("repository path is unavailable: {error}"))?;
    if !target.starts_with(&canonical_root) {
        return Err("path escapes the isolated repository".to_owned());
    }
    Ok(target)
}

fn ignored_directory(name: &str) -> bool {
    [".git", ".next", "build", "coverage", "dist", "node_modules", "target"]
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
}

fn searchable_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default();
    !["Cargo.lock", "package-lock.json", "pnpm-lock.yaml", "poetry.lock", "yarn.lock"]
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
        && !name.ends_with(".min.js")
        && !name.ends_with(".map")
}

fn search_priority(path: &Path) -> u8 {
    match path.extension().and_then(|value| value.to_str()).unwrap_or_default() {
        "c" | "cc" | "cpp" | "cs" | "go" | "h" | "hpp" | "java" | "js" | "jsx" | "kt" | "php"
        | "py" | "rb" | "rs" | "swift" | "ts" | "tsx" => 0,
        "css" | "graphql" | "html" | "json" | "md" | "sql" | "toml" | "xml" | "yaml" | "yml" => 1,
        _ => 2,
    }
}

fn relative_file(root: &Path, path: &Path) -> Option<String> {
    path.is_file().then(|| relative_path(root, path)).flatten()
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let root = fs::canonicalize(root).ok()?;
    let path = fs::canonicalize(path).ok()?;
    let relative = path.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn string_argument<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

fn usize_argument(arguments: &serde_json::Map<String, Value>, key: &str, default: usize) -> usize {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
}

fn bound_line(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("needle-repository-tools-{suffix}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join(".git"), "gitdir: elsewhere\n").unwrap();
        fs::write(root.join("package-lock.json"), "activation\n").unwrap();
        fs::write(root.join("src/lib.rs"), "fn resolve_activation() {}\n").unwrap();
        fs::write(root.join("target/generated.rs"), "fn activation() {}\n").unwrap();
        root
    }

    #[test]
    fn search_and_read_report_observed_repository_files() {
        let root = fixture();
        let search = execute("needle_search", &json!({"query": "activation"}), &root).unwrap();
        assert!(search.text.contains("src/lib.rs:1"));
        assert_eq!(search.observed_files, ["src/lib.rs"]);

        let read = execute(
            "needle_read",
            &json!({"path": "src/lib.rs", "start_line": 1, "line_count": 10}),
            &root,
        )
        .unwrap();
        assert!(read.text.contains("1: fn resolve_activation"));
        assert_eq!(read.observed_files, ["src/lib.rs"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_tool_rejects_path_traversal() {
        let root = fixture();
        let error = execute("needle_read", &json!({"path": "../secret"}), &root).unwrap_err();
        assert!(error.contains("repository-relative"));
        fs::remove_dir_all(root).unwrap();
    }
}

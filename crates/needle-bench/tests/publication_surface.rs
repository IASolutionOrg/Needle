use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    fs::read_to_string(workspace().join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
}

#[test]
fn publication_entrypoints_use_current_names_and_human_ownership() {
    let root = workspace();
    assert!(root.join("PROJECT_STATUS.md").is_file());
    assert!(!root.join("IMPLEMENTATION_STATUS.md").exists());
    assert!(root.join("docs/DEVELOPER_SETUP.md").is_file());
    assert!(!root.join("docs/GETTING_STARTED.md").exists());
    assert!(root.join("LICENSE.md").is_file());
    assert!(!root.join("LICENSE-MIT").exists());
    assert!(!root.join("LICENSE-APACHE").exists());

    let readme = read("README.md");
    assert!(readme.contains("PROJECT_STATUS.md"));
    assert!(readme.contains("assets/brand/needle-banner.png"));
    assert!(readme.contains("[Apache License 2.0](LICENSE.md)"));

    let workspace_manifest = read("Cargo.toml");
    assert!(workspace_manifest.contains("license = \"Apache-2.0\""));
    assert!(!workspace_manifest.contains("MIT OR Apache-2.0"));
    for package in
        ["needle-core", "needle-runtime", "needle-platform-codex", "needle-bench", "needle-app"]
    {
        let manifest = read(&format!("crates/{package}/Cargo.toml"));
        assert!(manifest.contains("license.workspace = true"), "{package} must inherit license");
    }

    let template = read(".github/PULL_REQUEST_TEMPLATE.md");
    assert!(
        template.contains("AI assistance: none | investigation | code | tests | documentation")
    );
    assert!(template.contains("I read and understand the complete diff"));
    assert!(template.contains("I finalized and personally published the commits"));
    assert!(template.contains("PROJECT_STATUS.md"));

    let app = read("crates/needle-app/src/main.rs");
    assert!(app.contains("\"artifact-cache-main-replay\""));
    assert!(!app.contains("\"r35-cache-main-replay\""));
    assert!(root.join("crates/needle-bench/src/bin/artifact-cache-replay.rs").is_file());
    assert!(!root.join("crates/needle-bench/src/bin/r35-cache-replay.rs").exists());
}

#[test]
fn curated_evidence_archive_and_sources_are_complete() {
    let expected = [
        "historical/cache-hit-calibration.md",
        "historical/cache-mutation-calibration.md",
        "historical/routing-location-calibration.md",
        "historical/routing-trace-calibration.md",
        "live/partial-and-cross-route-reuse.md",
        "live/routing-and-cache-calibration.md",
        "live/structured-mcp-cache-hit.md",
        "offline/claim-reuse-performance.md",
        "offline/end-to-end-proof-replay.md",
        "offline/multi-task-quality-replay.md",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();

    let results = workspace().join("benchmarks/results");
    let observed = ["historical", "live", "offline"]
        .into_iter()
        .flat_map(|directory| {
            fs::read_dir(results.join(directory))
                .unwrap_or_else(|error| panic!("cannot read {directory} reports: {error}"))
                .map(move |entry| {
                    let entry = entry.expect("report entry");
                    format!("{directory}/{}", entry.file_name().to_string_lossy())
                })
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(observed, expected);
    for relative in &observed {
        let report = read(&format!("benchmarks/results/{relative}"));
        for field in [
            "| Evidence level |",
            "| Date |",
            "| Repository / commit |",
            "| Task |",
            "| Route |",
            "| Models |",
            "| Codex / tier |",
            "| Pricing digest |",
            "| Provider calls |",
            "| Automatic retries |",
            "## Result",
            "## Limits and non-claims",
        ] {
            assert!(report.contains(field), "{relative} is missing `{field}`");
        }
    }

    for relative in [
        "benchmarks/corpus/router-cache/cost-model.json",
        "benchmarks/corpus/router-cache/campaign.json",
        "benchmarks/corpus/router-cache/minimal-live-pilot.json",
    ] {
        let value: Value = serde_json::from_str(&read(relative)).expect("valid evidence JSON");
        assert_evidence_paths_exist(&value, &workspace());
    }
}

fn assert_evidence_paths_exist(value: &Value, root: &Path) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "evidence" {
                    let entries = child.as_array().expect("evidence must be an array");
                    for entry in entries {
                        let relative = entry.as_str().expect("evidence path must be a string");
                        let path = Path::new(relative);
                        assert!(!path.is_absolute(), "evidence path must be relative: {relative}");
                        assert!(
                            path.components().all(|component| matches!(
                                component,
                                std::path::Component::Normal(_) | std::path::Component::CurDir
                            )),
                            "evidence path must not escape the workspace: {relative}"
                        );
                        assert!(root.join(path).is_file(), "missing evidence source: {relative}");
                    }
                } else {
                    assert_evidence_paths_exist(child, root);
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                assert_evidence_paths_exist(child, root);
            }
        }
        _ => {}
    }
}

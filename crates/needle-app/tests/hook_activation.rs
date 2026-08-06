use needle_core::{
    CodexHost, CodexRole, CommandPolicy, Digest, EvidenceFailurePolicy, FallbackPolicy,
    FilesystemPolicy, NetworkPolicy, ReasoningLevel, RepairPolicy, RoleProfileBudget,
    RoleProfileDefinition, RoleProfileDefinitionInput, RoleProfileId, ServiceTier, TestPolicy,
    ToolPolicy,
};
use needle_runtime::{RuntimeSettings, RuntimeStore};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_data(name: &str) -> PathBuf {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    std::env::temp_dir().join(format!("needle-{name}-{nonce}"))
}

fn repository_root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn run_session_start(data: &Path, session_id: &str) -> Value {
    let binary = env!("CARGO_BIN_EXE_needle");
    let input = serde_json::to_vec(&json!({
        "session_id": session_id,
        "cwd": repository_root(),
        "model": "gpt-test"
    }))
    .unwrap();
    let mut child = Command::new(binary)
        .args(["hook", "session-start"])
        .env("NEEDLE_DATA_DIR", data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_stop(data: &Path, session_id: &str) -> Value {
    let binary = env!("CARGO_BIN_EXE_needle");
    let input = serde_json::to_vec(&json!({
        "session_id": session_id,
        "turn_id": "turn-1",
        "cwd": repository_root(),
        "model": "gpt-test",
        "last_assistant_message": "@@need:trace.state-flow\nTrace the request.\n@@end"
    }))
    .unwrap();
    let mut child = Command::new(binary)
        .args(["hook", "stop"])
        .env("NEEDLE_DATA_DIR", data)
        .env("NEEDLE_RESOLVE_CACHE_ONLY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn explorer_profile(profile_id: RoleProfileId) -> RoleProfileDefinition {
    RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id,
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "gpt-test".to_owned(),
        reasoning: ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 120,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1000,
        },
        prompt_profile_digest: Digest::blake3(b"hook-activation-prompt"),
        output_contract_digest: Digest::blake3(b"hook-activation-output"),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy: RepairPolicy::None,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: Vec::new(),
    })
    .unwrap()
}

#[test]
fn disabled_hook_is_invisible_and_does_not_create_product_state() {
    let data = temporary_data("disabled-hook");
    let output = run_session_start(&data, "disabled-session");
    assert_eq!(output, json!({}));
    assert!(!data.join("needle.sqlite3").exists());
}

#[test]
fn enabled_hook_injects_context_and_freezes_explorer_provenance() {
    let data = temporary_data("enabled-hook");
    fs::create_dir_all(&data).unwrap();
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "gpt-test".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 120,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .unwrap();
    let profile_id = RoleProfileId::new("explorer.default").unwrap();
    let revision = store.create_role_profile(explorer_profile(profile_id.clone())).unwrap();
    let state = store.role_profile_state(&profile_id).unwrap();
    store.activate_role_profile(&profile_id, revision.revision, state.state_digest).unwrap();
    store.set_repository_activation(&repository_root(), true, Some(&profile_id)).unwrap();

    let output = run_session_start(&data, "enabled-session");
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(output["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("@@need"));
    let session = store.session("enabled-session").unwrap().unwrap();
    assert_eq!(session.role_profile_provenance.unwrap().profile_id, profile_id);
    let stop = run_stop(&data, "enabled-session");
    assert_eq!(stop["decision"], "block");
    assert!(data.join("hook-state").is_dir());
    drop(store);
    fs::remove_dir_all(data).unwrap();
}

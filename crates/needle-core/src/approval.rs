use crate::Digest;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionSource {
    AutoPolicy,
    WebUser,
    Timeout,
    Runtime,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClassification {
    AutoApprovedTest { policy_id: String },
    AutoApprovedReadOnly { policy_id: String },
    PendingUser,
    RejectedFileChange,
    RejectedNetwork,
    RejectedUnparseable,
    RejectedPolicyMismatch,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedPermissions {
    pub write_paths: Vec<String>,
    pub read_paths: Vec<String>,
    pub network: bool,
    pub raw: serde_json::Value,
}

impl Default for RequestedPermissions {
    fn default() -> Self {
        Self {
            write_paths: Vec::new(),
            read_paths: Vec::new(),
            network: false,
            raw: serde_json::Value::Null,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: String,
    pub protocol_request_id: serde_json::Value,
    pub protocol_approval_id: Option<String>,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub argv: Vec<String>,
    pub command_display: Option<String>,
    pub cwd: String,
    pub reason: Option<String>,
    pub requested_permissions: RequestedPermissions,
    pub route: String,
    pub repository_id: Digest,
    pub repository_root: String,
    pub expires_unix_ms: u64,
    pub classification: CommandClassification,
    pub payload_digest: Digest,
    pub decision: Option<ApprovalDecision>,
    pub decision_source: Option<ApprovalDecisionSource>,
    pub decided_unix_ms: Option<u64>,
}

impl ApprovalRequest {
    pub fn compute_payload_digest(
        argv: &[String],
        cwd: &str,
        requested_permissions: &RequestedPermissions,
    ) -> Result<Digest, serde_json::Error> {
        serde_json::to_vec(&(argv, cwd, requested_permissions)).map(Digest::blake3)
    }

    pub fn can_apply_decision(&self, expected_payload_digest: Digest, now_unix_ms: u64) -> bool {
        self.decision.is_none()
            && self.payload_digest == expected_payload_digest
            && now_unix_ms < self.expires_unix_ms
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandExecutionEvidence {
    pub id: String,
    pub approval_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub source_snapshot_digest: Digest,
    pub runner: String,
    pub runner_version: Option<String>,
    pub exit_status: Option<i32>,
    pub duration_ms: u64,
    pub output_digest: Digest,
    pub output_preview: String,
    pub test_identifier: Option<String>,
    pub tests_executed: Option<u32>,
    pub infrastructure_failure: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCommandPolicy {
    pub id: String,
    pub repository_id: Digest,
    pub trusted: bool,
    pub executable: String,
    pub argv_prefix: Vec<String>,
    pub maximum_executions_per_worker: u32,
}

impl TestCommandPolicy {
    pub fn cargo_test(repository_id: Digest) -> Self {
        Self {
            id: "cargo-test-direct-v1".to_owned(),
            repository_id,
            trusted: true,
            executable: "cargo".to_owned(),
            argv_prefix: vec!["cargo".to_owned(), "test".to_owned()],
            maximum_executions_per_worker: 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOnlyCommandPolicy {
    pub id: String,
    pub repository_id: Digest,
    pub trusted: bool,
    pub maximum_executions_per_worker: u32,
}

impl ReadOnlyCommandPolicy {
    pub fn repository_inspection(repository_id: Digest) -> Self {
        Self {
            id: "repository-read-only-v1".to_owned(),
            repository_id,
            trusted: true,
            maximum_executions_per_worker: 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_is_bound_to_payload_and_expiry() {
        let permissions = RequestedPermissions::default();
        let digest = ApprovalRequest::compute_payload_digest(
            &["cargo".into(), "test".into()],
            "C:/run",
            &permissions,
        )
        .unwrap();
        let request = ApprovalRequest {
            id: "approval".to_owned(),
            protocol_request_id: serde_json::json!(7),
            protocol_approval_id: None,
            thread_id: "thread".to_owned(),
            turn_id: "turn".to_owned(),
            item_id: "item".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned()],
            command_display: None,
            cwd: "C:/run".to_owned(),
            reason: None,
            requested_permissions: permissions,
            route: "tests.relevant".to_owned(),
            repository_id: Digest::blake3("repo"),
            repository_root: "C:/run".to_owned(),
            expires_unix_ms: 20,
            classification: CommandClassification::PendingUser,
            payload_digest: digest,
            decision: None,
            decision_source: None,
            decided_unix_ms: None,
        };
        assert!(request.can_apply_decision(digest, 19));
        assert!(!request.can_apply_decision(Digest::blake3("changed"), 19));
        assert!(!request.can_apply_decision(digest, 20));
    }

    #[test]
    fn protocol_decisions_exclude_session_and_policy_amendments() {
        for decision in
            [ApprovalDecision::Accept, ApprovalDecision::Decline, ApprovalDecision::Cancel]
        {
            let json = serde_json::to_string(&decision).unwrap();
            assert!(!json.contains("Session"));
            assert!(!json.contains("Amendment"));
        }
    }
}

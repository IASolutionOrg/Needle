use crate::store::now_ms;
use crate::{RuntimeStore, StoreError};
use needle_core::{
    Artifact, ArtifactRequest, CacheResolution, CacheScope, DependencyManifest, Digest, RoutePlan,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_LEASE_MS: u64 = 30_000;
const DEFAULT_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ArtifactCache {
    store: RuntimeStore,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactCacheError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("artifact computation failed: {0}")]
    Compute(String),
    #[error("single-flight timed out for request {0}")]
    SingleFlightTimeout(Digest),
    #[error("computed artifact does not belong to request")]
    RequestMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanCacheResult {
    pub resolution: CacheResolution,
    pub artifacts: BTreeMap<String, Artifact>,
}

impl ArtifactCache {
    pub fn new(store: RuntimeStore) -> Self {
        Self { store }
    }

    /// Resolves an exact snapshot artifact first. A semantic reuse is only
    /// possible when every claim-bearing dependency is complete and still
    /// hashes to the observed digest.
    pub fn resolve(
        &self,
        request: &ArtifactRequest,
        repository_root: &Path,
    ) -> Result<(CacheResolution, Option<Artifact>), ArtifactCacheError> {
        let (exact, artifact) = self.store.resolve_artifact(request)?;
        if artifact.is_some() {
            return Ok((exact, artifact));
        }
        let Some(candidate) = self.store.latest_logical_artifact(request)? else {
            return Ok((CacheResolution::Miss, None));
        };
        match validate_manifest(&candidate.dependency_manifest, repository_root) {
            ManifestValidity::Reusable => Ok((
                CacheResolution::CompositeHit {
                    artifact_ids: vec![candidate.id],
                    sufficiency_certificate_id: None,
                    selected_plan_id: None,
                    resolution_format_revision: None,
                },
                Some(candidate),
            )),
            ManifestValidity::Bypass(reason) => Ok((CacheResolution::Bypass { reason }, None)),
            ManifestValidity::Stale(reason) => {
                Ok((CacheResolution::Stale { artifact_id: candidate.id, reason }, None))
            }
        }
    }

    /// Executes `compute` only for the lease owner. Followers wait for the
    /// published artifact and never start a duplicate worker.
    pub fn resolve_or_compute<F>(
        &self,
        request: &ArtifactRequest,
        repository_root: &Path,
        owner: &str,
        compute: F,
    ) -> Result<(CacheResolution, Artifact), ArtifactCacheError>
    where
        F: FnOnce() -> Result<Artifact, ArtifactCacheError>,
    {
        let (resolution, artifact) = self.resolve(request, repository_root)?;
        if let Some(artifact) = artifact {
            return Ok((resolution, artifact));
        }
        let request_id = request.id();
        if self.store.acquire_artifact_lease(
            request_id,
            owner,
            now_ms().saturating_add(DEFAULT_LEASE_MS),
        )? {
            let result = (|| {
                let artifact = compute()?;
                if artifact.request_id != request_id {
                    return Err(ArtifactCacheError::RequestMismatch);
                }
                self.store.publish_artifact(request, &artifact)?;
                Ok((CacheResolution::Miss, artifact))
            })();
            self.store.release_artifact_lease(request_id, owner)?;
            return result;
        }

        let deadline = Instant::now() + DEFAULT_WAIT;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            let (resolution, artifact) = self.resolve(request, repository_root)?;
            if let Some(artifact) = artifact {
                return Ok((resolution, artifact));
            }
        }
        Err(ArtifactCacheError::SingleFlightTimeout(request_id))
    }

    /// Resolves plan nodes in topological order. A stale/missing node
    /// invalidates only itself and its descendants; independent nodes remain
    /// reusable.
    pub fn resolve_plan(
        &self,
        plan: &RoutePlan,
        requests: &BTreeMap<String, ArtifactRequest>,
        repository_root: &Path,
    ) -> Result<PlanCacheResult, ArtifactCacheError> {
        let mut artifacts = BTreeMap::new();
        let mut invalidated = BTreeSet::new();
        let mut bypass_reason = None;

        for node in &plan.nodes {
            if node.depends_on.iter().any(|dependency| invalidated.contains(dependency)) {
                invalidated.insert(node.id.clone());
                continue;
            }
            let Some(request) = requests.get(&node.id) else {
                invalidated.insert(node.id.clone());
                continue;
            };
            let (resolution, artifact) = self.resolve(request, repository_root)?;
            match (resolution, artifact) {
                (
                    CacheResolution::ExactHit { .. } | CacheResolution::CompositeHit { .. },
                    Some(value),
                ) => {
                    artifacts.insert(node.id.clone(), value);
                }
                (CacheResolution::Bypass { reason }, _) => {
                    bypass_reason.get_or_insert(reason);
                    invalidated.insert(node.id.clone());
                }
                _ => {
                    invalidated.insert(node.id.clone());
                }
            }
        }

        Ok(plan_cache_result(plan, artifacts, invalidated, bypass_reason))
    }

    /// Builds each node request only after its parents have resolved, so the
    /// semantic identity contains the actual input artifact ids.
    pub fn resolve_route_plan(
        &self,
        plan: &RoutePlan,
        normalized_request: &str,
        repository_id: Digest,
        source_snapshot_digest: Digest,
        repository_root: &Path,
    ) -> Result<PlanCacheResult, ArtifactCacheError> {
        let mut artifacts = BTreeMap::new();
        let mut invalidated = BTreeSet::new();
        let mut bypass_reason = None;

        for node in &plan.nodes {
            if node.depends_on.iter().any(|dependency| invalidated.contains(dependency)) {
                invalidated.insert(node.id.clone());
                continue;
            }
            let input_artifact_ids = node
                .depends_on
                .iter()
                .filter_map(|dependency| artifacts.get(dependency))
                .map(|artifact: &Artifact| artifact.id)
                .collect::<Vec<_>>();
            if input_artifact_ids.len() != node.depends_on.len() {
                invalidated.insert(node.id.clone());
                continue;
            }
            let request = ArtifactRequest {
                contract_id: format!("needle.{}", node.operator_id),
                contract_revision: 1,
                repository_id,
                source_snapshot_digest,
                route_key: plan.route_key.clone(),
                normalized_request: normalized_request.to_owned(),
                semantic_fragment_id: None,
                input_artifact_ids,
            };
            let (resolution, artifact) = self.resolve(&request, repository_root)?;
            match (resolution, artifact) {
                (
                    CacheResolution::ExactHit { .. } | CacheResolution::CompositeHit { .. },
                    Some(value),
                ) => {
                    artifacts.insert(node.id.clone(), value);
                }
                (CacheResolution::Bypass { reason }, _) => {
                    bypass_reason.get_or_insert(reason);
                    invalidated.insert(node.id.clone());
                }
                _ => {
                    invalidated.insert(node.id.clone());
                }
            }
        }

        Ok(plan_cache_result(plan, artifacts, invalidated, bypass_reason))
    }
}

fn plan_cache_result(
    plan: &RoutePlan,
    artifacts: BTreeMap<String, Artifact>,
    invalidated: BTreeSet<String>,
    bypass_reason: Option<String>,
) -> PlanCacheResult {
    let reused = artifacts.values().map(|artifact| artifact.id).collect::<Vec<_>>();
    let invalidated_nodes = plan
        .nodes
        .iter()
        .filter(|node| invalidated.contains(&node.id))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let resolution = if invalidated_nodes.is_empty() {
        CacheResolution::CompositeHit {
            artifact_ids: reused,
            sufficiency_certificate_id: None,
            selected_plan_id: None,
            resolution_format_revision: None,
        }
    } else if !reused.is_empty() {
        CacheResolution::PartialHit {
            reused,
            reused_claim_ids: Vec::new(),
            invalidated_nodes,
            selected_plan_id: None,
            resolution_format_revision: None,
        }
    } else if let Some(reason) = bypass_reason {
        CacheResolution::Bypass { reason }
    } else {
        CacheResolution::Miss
    };
    PlanCacheResult { resolution, artifacts }
}

enum ManifestValidity {
    Reusable,
    Stale(String),
    Bypass(String),
}

fn validate_manifest(manifest: &DependencyManifest, root: &Path) -> ManifestValidity {
    if manifest.scope != CacheScope::WorktreeSemantic {
        return ManifestValidity::Stale(
            "snapshot-exact artifact cannot cross snapshots".to_owned(),
        );
    }
    if !manifest.observed_files_complete || !manifest.gaps.is_empty() {
        return ManifestValidity::Bypass(
            "dependency closure is incomplete or contains unrepresentable search gaps".to_owned(),
        );
    }
    if manifest.dependencies.is_empty()
        || manifest.dependencies.iter().any(|dependency| dependency.claims.is_empty())
    {
        return ManifestValidity::Bypass("claim-to-dependency closure is not provable".to_owned());
    }
    let Ok(canonical_root) = fs::canonicalize(root) else {
        return ManifestValidity::Bypass("repository root cannot be canonicalized".to_owned());
    };
    for dependency in &manifest.dependencies {
        let Some(candidate) = safe_dependency_path(root, &dependency.path) else {
            return ManifestValidity::Bypass(format!(
                "unsafe dependency path `{}`",
                dependency.path
            ));
        };
        let Ok(path) = fs::canonicalize(candidate) else {
            return ManifestValidity::Stale(format!("dependency `{}` is missing", dependency.path));
        };
        if !path.starts_with(&canonical_root) || !path.is_file() {
            return ManifestValidity::Bypass(format!(
                "dependency `{}` escapes the repository",
                dependency.path
            ));
        }
        let Ok(bytes) = fs::read(&path) else {
            return ManifestValidity::Stale(format!("dependency `{}` is missing", dependency.path));
        };
        if Digest::blake3(bytes) != dependency.content_digest {
            return ManifestValidity::Stale(format!("dependency `{}` changed", dependency.path));
        }
    }
    ManifestValidity::Reusable
}

fn safe_dependency_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        ArtifactContract, ArtifactKind, CacheScope, Dependency, DependencyManifest, NeedKey,
        PlanNode, ValidationRecord,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "needle-artifact-cache-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn request(snapshot: &str, key: &str) -> ArtifactRequest {
        ArtifactRequest {
            contract_id: format!("{key}.contract"),
            contract_revision: 1,
            repository_id: Digest::blake3("repository"),
            source_snapshot_digest: Digest::blake3(snapshot),
            route_key: NeedKey::new("trace.state-flow").unwrap(),
            normalized_request: key.to_owned(),
            semantic_fragment_id: None,
            input_artifact_ids: Vec::new(),
        }
    }

    fn artifact(request: &ArtifactRequest, dependency: &str, bytes: &[u8]) -> Artifact {
        let contract = ArtifactContract::new(
            request.contract_id.clone(),
            1,
            ArtifactKind::code_location(),
            CacheScope::WorktreeSemantic,
        );
        let payload = json!({"key": request.normalized_request});
        let request_id = request.id();
        Artifact {
            id: Artifact::compute_id(request_id, &contract, &payload).unwrap(),
            request_id,
            contract,
            payload,
            dependency_manifest: DependencyManifest {
                scope: CacheScope::WorktreeSemantic,
                observed_files_complete: true,
                dependencies: vec![Dependency {
                    path: dependency.to_owned(),
                    content_digest: Digest::blake3(bytes),
                    byte_start: None,
                    byte_end: None,
                    claims: vec!["claim".to_owned()],
                }],
                gaps: Vec::new(),
            },
            validations: vec![ValidationRecord {
                validator: "fixture".to_owned(),
                validator_revision: 1,
                status: "passed".to_owned(),
                evidence_digest: Digest::blake3("validated"),
                validated_unix_ms: now_ms(),
            }],
            created_unix_ms: now_ms(),
        }
    }

    #[test]
    fn irrelevant_mutation_reuses_and_relevant_mutation_never_hits() {
        let root = root("mutations");
        fs::write(root.join("dependent.rs"), b"stable").unwrap();
        fs::write(root.join("irrelevant.rs"), b"before").unwrap();
        let store = RuntimeStore::new(root.join("cache.sqlite3"));
        store.initialize().unwrap();
        let cache = ArtifactCache::new(store.clone());
        let original = request("snapshot-a", "location");
        store.publish_artifact(&original, &artifact(&original, "dependent.rs", b"stable")).unwrap();

        fs::write(root.join("irrelevant.rs"), b"after").unwrap();
        let changed_snapshot = request("snapshot-b", "location");
        let (resolution, value) = cache.resolve(&changed_snapshot, &root).unwrap();
        assert!(matches!(resolution, CacheResolution::CompositeHit { .. }));
        assert!(value.is_some());

        fs::write(root.join("dependent.rs"), b"changed").unwrap();
        let (resolution, value) = cache.resolve(&changed_snapshot, &root).unwrap();
        assert!(matches!(resolution, CacheResolution::Stale { .. }));
        assert!(value.is_none(), "a stale artifact must never be returned");
    }

    #[test]
    fn partial_plan_invalidates_only_changed_node_and_descendants() {
        let root = root("partial");
        fs::write(root.join("location.rs"), b"location").unwrap();
        fs::write(root.join("test.rs"), b"test").unwrap();
        let store = RuntimeStore::new(root.join("cache.sqlite3"));
        store.initialize().unwrap();
        let cache = ArtifactCache::new(store.clone());
        let plan = RoutePlan::new(
            "fixture",
            1,
            NeedKey::new("trace.state-flow").unwrap(),
            vec![
                PlanNode {
                    id: "location".to_owned(),
                    operator_id: "location".to_owned(),
                    depends_on: vec![],
                },
                PlanNode {
                    id: "test".to_owned(),
                    operator_id: "test".to_owned(),
                    depends_on: vec![],
                },
                PlanNode {
                    id: "brief".to_owned(),
                    operator_id: "brief".to_owned(),
                    depends_on: vec!["location".to_owned(), "test".to_owned()],
                },
            ],
        )
        .unwrap();
        for (node, file, bytes) in [
            ("location", "location.rs", b"location".as_slice()),
            ("test", "test.rs", b"test".as_slice()),
            ("brief", "location.rs", b"location".as_slice()),
        ] {
            let old = request("snapshot-a", node);
            store.publish_artifact(&old, &artifact(&old, file, bytes)).unwrap();
        }
        fs::write(root.join("location.rs"), b"changed").unwrap();
        let requests = ["location", "test", "brief"]
            .into_iter()
            .map(|node| (node.to_owned(), request("snapshot-b", node)))
            .collect();
        let started = Instant::now();
        let result = cache.resolve_plan(&plan, &requests, &root).unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(result.artifacts.keys().cloned().collect::<Vec<_>>(), vec!["test"]);
        assert_eq!(
            result.resolution,
            CacheResolution::PartialHit {
                reused: vec![result.artifacts["test"].id],
                reused_claim_ids: Vec::new(),
                invalidated_nodes: vec!["location".to_owned(), "brief".to_owned()],
                selected_plan_id: None,
                resolution_format_revision: None,
            }
        );
    }

    #[test]
    fn exact_hit_skips_compute_and_single_flight_computes_once() {
        let root = root("single-flight");
        fs::write(root.join("dependent.rs"), b"stable").unwrap();
        let store = RuntimeStore::new(root.join("cache.sqlite3"));
        store.initialize().unwrap();
        let cache = ArtifactCache::new(store);
        let request = request("snapshot-a", "location");
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let mut joins = Vec::new();
        for owner in ["one", "two"] {
            let cache = cache.clone();
            let request = request.clone();
            let root = root.clone();
            let calls = calls.clone();
            let barrier = barrier.clone();
            joins.push(thread::spawn(move || {
                barrier.wait();
                cache
                    .resolve_or_compute(&request, &root, owner, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(50));
                        Ok(artifact(&request, "dependent.rs", b"stable"))
                    })
                    .unwrap()
            }));
        }
        let results = joins.into_iter().map(|join| join.join().unwrap()).collect::<Vec<_>>();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(results[0].1.id, results[1].1.id);

        let guard_calls = AtomicUsize::new(0);
        let (resolution, _) = cache
            .resolve_or_compute(&request, &root, "three", || {
                guard_calls.fetch_add(1, Ordering::SeqCst);
                Err(ArtifactCacheError::Compute("must not run".to_owned()))
            })
            .unwrap();
        assert!(matches!(resolution, CacheResolution::ExactHit { .. }));
        assert_eq!(guard_calls.load(Ordering::SeqCst), 0);
    }
}

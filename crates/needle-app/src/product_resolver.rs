use crate::runtime_instance;
use needle_platform_codex::CodexWorker;
use needle_runtime::{ResolveOutcome, ResolveRequest, RuntimeEngine, RuntimeStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerPolicy {
    Allow,
    CacheOnly,
}

pub(crate) struct ProductResolver {
    data_directory: PathBuf,
    store: RuntimeStore,
    engine: RuntimeEngine<CodexWorker>,
    worker_policy: WorkerPolicy,
}

impl ProductResolver {
    pub(crate) fn new(
        data_directory: impl Into<PathBuf>,
        worker_policy: WorkerPolicy,
    ) -> Result<Self, String> {
        Self::new_with_cancellation(data_directory, worker_policy, None)
    }

    pub(crate) fn new_cancellable(
        data_directory: impl Into<PathBuf>,
        worker_policy: WorkerPolicy,
        cancellation: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Self::new_with_cancellation(data_directory, worker_policy, Some(cancellation))
    }

    fn new_with_cancellation(
        data_directory: impl Into<PathBuf>,
        worker_policy: WorkerPolicy,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<Self, String> {
        let data_directory = data_directory.into();
        let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
        store.initialize().map_err(|error| error.to_string())?;
        // A migrated database without product settings is not runnable. Fail at
        // startup instead of returning a misleading tool-level cache miss.
        store.settings().map_err(|error| error.to_string())?;
        let worker = CodexWorker::new(data_directory.clone());
        let worker = if let Some(cancellation) = cancellation {
            worker.with_cancellation(cancellation)
        } else {
            worker
        };
        let engine = RuntimeEngine::new(store.clone(), worker);
        Ok(Self { data_directory, store, engine, worker_policy })
    }

    pub(crate) fn store(&self) -> &RuntimeStore {
        &self.store
    }

    pub(crate) fn resolve(&self, request: &ResolveRequest) -> Result<ResolveOutcome, String> {
        if self.worker_policy == WorkerPolicy::CacheOnly {
            return self.engine.resolve_cache_only(request).map_err(|error| error.to_string());
        }
        if !runtime_instance::is_published(&self.data_directory) {
            return self.engine.resolve(request).map_err(|error| error.to_string());
        }
        match runtime_instance::resolve_resident(&self.data_directory, request) {
            Ok(outcome) => Ok(outcome),
            Err(runtime_instance::ResidentResolveError::Unavailable(error)) => {
                eprintln!("needle: resident runtime unavailable ({error}); using local resolver");
                self.engine.resolve(request).map_err(|error| error.to_string())
            }
            Err(runtime_instance::ResidentResolveError::Remote(error)) => Err(error),
        }
    }

    pub(crate) fn resolve_direct_explore(
        &self,
        request: &ResolveRequest,
    ) -> Result<ResolveOutcome, String> {
        if self.worker_policy == WorkerPolicy::CacheOnly {
            return self
                .engine
                .resolve_direct_explore_cache_only(request)
                .map_err(|error| error.to_string());
        }
        self.engine.resolve_direct_explore(request).map_err(|error| error.to_string())
    }

    pub(crate) fn resolve_semantic_required(
        &self,
        request: &ResolveRequest,
    ) -> Result<ResolveOutcome, String> {
        if self.worker_policy == WorkerPolicy::CacheOnly {
            return self
                .engine
                .resolve_semantic_required_cache_only(request)
                .map_err(|error| error.to_string());
        }
        self.engine.resolve_semantic_required(request).map_err(|error| error.to_string())
    }

    pub(crate) fn resolve_semantic_required_calibration(
        &self,
        request: &ResolveRequest,
    ) -> Result<ResolveOutcome, String> {
        if self.worker_policy == WorkerPolicy::CacheOnly {
            return self
                .engine
                .resolve_semantic_required_cache_only(request)
                .map_err(|error| error.to_string());
        }
        self.engine
            .resolve_semantic_required_calibration(request)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn resolve_semantic_required_cache_only(
        &self,
        request: &ResolveRequest,
    ) -> Result<ResolveOutcome, String> {
        self.engine.resolve_semantic_required_cache_only(request).map_err(|error| error.to_string())
    }

    pub(crate) fn data_directory(&self) -> &Path {
        &self.data_directory
    }
}

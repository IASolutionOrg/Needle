use fs2::FileExt;
use needle_core::Digest;
use needle_platform_codex::CodexWorker;
use needle_runtime::{ResolveOutcome, ResolveRequest, RuntimeEngine, RuntimeStore};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const DESCRIPTOR_SCHEMA: &str = "needle.runtime-instance/2";
const IPC_SCHEMA: &str = "needle.runtime-ipc/1";
const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;
const READY_ATTEMPTS: usize = 40;
const READY_RETRY: Duration = Duration::from_millis(50);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(210);

#[derive(Debug)]
pub(crate) enum ResidentResolveError {
    Unavailable(String),
    Remote(String),
}

impl std::fmt::Display for ResidentResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Remote(message) => formatter.write_str(message),
        }
    }
}

pub(crate) struct InstanceGuard {
    lock: File,
    descriptor: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDescriptor {
    schema: String,
    pid: u32,
    http_authority: String,
    ipc_endpoint: String,
    ipc_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum IpcRequest {
    Health { schema: String, request_id: String, token: String },
    Resolve { schema: String, request_id: String, token: String, request: Box<ResolveRequest> },
}

impl IpcRequest {
    fn request_id(&self) -> &str {
        match self {
            Self::Health { request_id, .. } | Self::Resolve { request_id, .. } => request_id,
        }
    }

    fn schema(&self) -> &str {
        match self {
            Self::Health { schema, .. } | Self::Resolve { schema, .. } => schema,
        }
    }

    fn token(&self) -> &str {
        match self {
            Self::Health { token, .. } | Self::Resolve { token, .. } => token,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum IpcResponse {
    Ok { schema: String, request_id: String, outcome: Option<Box<ResolveOutcome>> },
    Error { schema: String, request_id: String, code: String, message: String },
}

impl InstanceGuard {
    pub(crate) fn acquire(data_directory: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_directory).map_err(|error| error.to_string())?;
        let lock_path = data_directory.join("runtime.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|error| error.to_string())?;
        lock.try_lock_exclusive()
            .map_err(|_| "another Needle runtime is already active for this profile".to_owned())?;
        Ok(Self { lock, descriptor: data_directory.join("runtime.json") })
    }

    pub(crate) fn publish(
        &self,
        authority: &str,
        ipc_endpoint: &str,
        ipc_token: &str,
    ) -> Result<(), String> {
        let descriptor = RuntimeDescriptor {
            schema: DESCRIPTOR_SCHEMA.to_owned(),
            pid: std::process::id(),
            http_authority: authority.to_owned(),
            ipc_endpoint: ipc_endpoint.to_owned(),
            ipc_token: ipc_token.to_owned(),
        };
        let bytes = serde_json::to_vec_pretty(&descriptor).map_err(|error| error.to_string())?;
        let temporary = self.descriptor.with_extension("json.tmp");
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        fs::rename(temporary, &self.descriptor).map_err(|error| error.to_string())
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.descriptor);
        let _ = FileExt::unlock(&self.lock);
    }
}

pub(crate) fn endpoint(data_directory: &Path) -> String {
    let digest = Digest::blake3(data_directory.to_string_lossy().as_bytes()).to_string();
    let suffix = digest.trim_start_matches("b3:").chars().take(20).collect::<String>();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\needle-{suffix}")
    }
    #[cfg(unix)]
    {
        let candidate = data_directory.join(format!("needle-{suffix}.sock"));
        if std::os::unix::net::SocketAddr::from_pathname(&candidate).is_ok() {
            candidate.to_string_lossy().into_owned()
        } else {
            Path::new("/tmp").join(format!("needle-{suffix}.sock")).to_string_lossy().into_owned()
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        data_directory.join(format!("needle-{suffix}.sock")).to_string_lossy().into_owned()
    }
}

pub(crate) fn is_published(data_directory: &Path) -> bool {
    data_directory.join("runtime.json").is_file()
}

#[cfg(windows)]
pub(crate) async fn serve_ipc(
    endpoint: String,
    token: String,
    store: RuntimeStore,
    data_directory: PathBuf,
) -> Result<(), String> {
    use tokio::net::windows::named_pipe::ServerOptions;

    loop {
        let server = ServerOptions::new()
            .first_pipe_instance(false)
            .reject_remote_clients(true)
            .create(&endpoint)
            .map_err(|error| error.to_string())?;
        server.connect().await.map_err(|error| error.to_string())?;
        let token = token.clone();
        let store = store.clone();
        let data_directory = data_directory.clone();
        tokio::spawn(async move {
            let _ = serve_connection(server, &token, store, data_directory).await;
        });
    }
}

#[cfg(unix)]
pub(crate) async fn serve_ipc(
    endpoint: String,
    token: String,
    store: RuntimeStore,
    data_directory: PathBuf,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = Path::new(&endpoint);
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    let listener = UnixListener::bind(path).map_err(|error| error.to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    loop {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let token = token.clone();
        let store = store.clone();
        let data_directory = data_directory.clone();
        tokio::spawn(async move {
            let _ = serve_connection(stream, &token, store, data_directory).await;
        });
    }
}

#[cfg(not(any(windows, unix)))]
pub(crate) async fn serve_ipc(
    _endpoint: String,
    _token: String,
    _store: RuntimeStore,
    _data_directory: PathBuf,
) -> Result<(), String> {
    std::future::pending().await
}

async fn serve_connection<S>(
    mut stream: S,
    expected_token: &str,
    store: RuntimeStore,
    data_directory: PathBuf,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = read_frame(&mut stream).await?;
    let response = match serde_json::from_slice::<IpcRequest>(&frame) {
        Ok(request) => handle_request(request, expected_token, store, data_directory).await,
        Err(error) => IpcResponse::Error {
            schema: IPC_SCHEMA.to_owned(),
            request_id: "unknown".to_owned(),
            code: "invalid_request".to_owned(),
            message: error.to_string(),
        },
    };
    let mut bytes = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())
}

async fn handle_request(
    request: IpcRequest,
    expected_token: &str,
    store: RuntimeStore,
    data_directory: PathBuf,
) -> IpcResponse {
    let request_id = request.request_id().to_owned();
    if request.schema() != IPC_SCHEMA {
        return ipc_error(request_id, "incompatible_schema", "runtime IPC schema mismatch");
    }
    if request.token().as_bytes() != expected_token.as_bytes() {
        return ipc_error(request_id, "unauthorized", "runtime IPC capability token is invalid");
    }
    match request {
        IpcRequest::Health { .. } => {
            IpcResponse::Ok { schema: IPC_SCHEMA.to_owned(), request_id, outcome: None }
        }
        IpcRequest::Resolve { request, .. } => {
            let result = tokio::task::spawn_blocking(move || {
                RuntimeEngine::new(store, CodexWorker::new(data_directory)).resolve(&request)
            })
            .await;
            match result {
                Ok(Ok(outcome)) => IpcResponse::Ok {
                    schema: IPC_SCHEMA.to_owned(),
                    request_id,
                    outcome: Some(Box::new(outcome)),
                },
                Ok(Err(error)) => ipc_error(request_id, "resolve_failed", &error.to_string()),
                Err(error) => ipc_error(request_id, "runtime_join_failed", &error.to_string()),
            }
        }
    }
}

fn ipc_error(request_id: String, code: &str, message: &str) -> IpcResponse {
    IpcResponse::Error {
        schema: IPC_SCHEMA.to_owned(),
        request_id,
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

async fn read_frame<S>(stream: &mut S) -> Result<Vec<u8>, String>
where
    S: AsyncRead + Unpin,
{
    let mut frame = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("runtime IPC stream closed before a complete frame".to_owned());
        }
        if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            frame.extend_from_slice(&chunk[..newline]);
            break;
        }
        frame.extend_from_slice(&chunk[..read]);
        if frame.len() > MAX_IPC_FRAME_BYTES {
            return Err("runtime IPC request exceeds the frame cap".to_owned());
        }
    }
    if frame.is_empty() || frame.len() > MAX_IPC_FRAME_BYTES {
        return Err("runtime IPC request is empty or oversized".to_owned());
    }
    Ok(frame)
}

pub(crate) async fn wait_until_ready(endpoint: &str, token: &str) -> Result<(), String> {
    let request_id = unique_request_id("health");
    let request = IpcRequest::Health {
        schema: IPC_SCHEMA.to_owned(),
        request_id: request_id.clone(),
        token: token.to_owned(),
    };
    let mut last_error = "runtime IPC did not start".to_owned();
    for _ in 0..READY_ATTEMPTS {
        match tokio::time::timeout(HEALTH_TIMEOUT, call_endpoint(endpoint, &request)).await {
            Ok(Ok(IpcResponse::Ok { request_id: response_id, outcome: None, .. }))
                if response_id == request_id =>
            {
                return Ok(());
            }
            Ok(Ok(IpcResponse::Error { message, .. })) | Ok(Err(message)) => last_error = message,
            Ok(Ok(_)) => last_error = "runtime IPC returned an invalid health response".to_owned(),
            Err(_) => last_error = "runtime IPC health check timed out".to_owned(),
        }
        tokio::time::sleep(READY_RETRY).await;
    }
    Err(last_error)
}

pub(crate) fn resolve_resident(
    data_directory: &Path,
    request: &ResolveRequest,
) -> Result<ResolveOutcome, ResidentResolveError> {
    let descriptor_path = data_directory.join("runtime.json");
    let descriptor: RuntimeDescriptor = serde_json::from_slice(
        &fs::read(&descriptor_path)
            .map_err(|error| ResidentResolveError::Unavailable(error.to_string()))?,
    )
    .map_err(|error| ResidentResolveError::Unavailable(error.to_string()))?;
    if descriptor.schema != DESCRIPTOR_SCHEMA
        || descriptor.pid == 0
        || descriptor.ipc_token.len() != 64
    {
        return Err(ResidentResolveError::Unavailable(
            "resident runtime descriptor is invalid or incompatible".to_owned(),
        ));
    }
    let health_id = unique_request_id("health");
    let health_request = IpcRequest::Health {
        schema: IPC_SCHEMA.to_owned(),
        request_id: health_id.clone(),
        token: descriptor.ipc_token.clone(),
    };
    let request_id = unique_request_id("resolve");
    let ipc_request = IpcRequest::Resolve {
        schema: IPC_SCHEMA.to_owned(),
        request_id: request_id.clone(),
        token: descriptor.ipc_token,
        request: Box::new(request.clone()),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ResidentResolveError::Unavailable(error.to_string()))?;
    runtime
        .block_on(async {
            tokio::time::timeout(
                HEALTH_TIMEOUT,
                call_endpoint(&descriptor.ipc_endpoint, &health_request),
            )
            .await
            .map_err(|_| "resident runtime health check timed out".to_owned())?
        })
        .map_err(ResidentResolveError::Unavailable)
        .and_then(|response| match response {
            IpcResponse::Ok { request_id: response_id, outcome: None, .. }
                if response_id == health_id =>
            {
                Ok(())
            }
            IpcResponse::Error { code, message, .. } => {
                Err(ResidentResolveError::Unavailable(format!("{code}: {message}")))
            }
            _ => Err(ResidentResolveError::Unavailable(
                "resident runtime returned an invalid health response".to_owned(),
            )),
        })?;
    let response = runtime.block_on(async {
        tokio::time::timeout(RESOLVE_TIMEOUT, call_endpoint(&descriptor.ipc_endpoint, &ipc_request))
            .await
            .map_err(|_| "resident runtime resolve timed out".to_owned())?
    });
    let response = response.map_err(ResidentResolveError::Remote)?;
    match response {
        IpcResponse::Ok { request_id: response_id, outcome: Some(outcome), .. }
            if response_id == request_id =>
        {
            Ok(*outcome)
        }
        IpcResponse::Error { code, message, .. } => {
            Err(ResidentResolveError::Remote(format!("{code}: {message}")))
        }
        _ => Err(ResidentResolveError::Remote(
            "resident runtime returned an invalid resolve response".to_owned(),
        )),
    }
}

#[cfg(windows)]
async fn call_endpoint(endpoint: &str, request: &IpcRequest) -> Result<IpcResponse, String> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut stream = ClientOptions::new().open(endpoint).map_err(|error| error.to_string())?;
    exchange(&mut stream, request).await
}

#[cfg(unix)]
async fn call_endpoint(endpoint: &str, request: &IpcRequest) -> Result<IpcResponse, String> {
    let mut stream =
        tokio::net::UnixStream::connect(endpoint).await.map_err(|error| error.to_string())?;
    exchange(&mut stream, request).await
}

#[cfg(not(any(windows, unix)))]
async fn call_endpoint(_endpoint: &str, _request: &IpcRequest) -> Result<IpcResponse, String> {
    Err("runtime IPC is unsupported on this platform".to_owned())
}

async fn exchange<S>(stream: &mut S, request: &IpcRequest) -> Result<IpcResponse, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_IPC_FRAME_BYTES {
        return Err("runtime IPC request exceeds the frame cap".to_owned());
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())?;
    let response = read_frame(stream).await?;
    serde_json::from_slice(&response).map_err(|error| error.to_string())
}

fn unique_request_id(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    Digest::blake3(format!("{prefix}\n{}\n{nanos}\n", std::process::id())).to_hex()
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{NeedKey, NeedRequest};

    #[test]
    fn profile_lock_allows_only_one_runtime() {
        let root = std::env::temp_dir().join(format!(
            "needle-instance-lock-{}-{}",
            std::process::id(),
            crate::server::test_nonce()
        ));
        let first = InstanceGuard::acquire(&root).unwrap();
        assert!(InstanceGuard::acquire(&root).is_err());
        drop(first);
        assert!(InstanceGuard::acquire(&root).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unpublished_runtime_is_a_normal_on_demand_state() {
        let root = std::env::temp_dir().join(format!(
            "needle-instance-unpublished-{}-{}",
            std::process::id(),
            crate::server::test_nonce()
        ));
        fs::create_dir_all(&root).unwrap();
        assert!(!is_published(&root));
        fs::write(root.join("runtime.json"), "{}").unwrap();
        assert!(is_published(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn descriptor_requires_current_schema_and_capability() {
        let descriptor = RuntimeDescriptor {
            schema: DESCRIPTOR_SCHEMA.to_owned(),
            pid: 1,
            http_authority: "127.0.0.1:1".to_owned(),
            ipc_endpoint: "endpoint".to_owned(),
            ipc_token: "a".repeat(64),
        };
        let round_trip: RuntimeDescriptor =
            serde_json::from_slice(&serde_json::to_vec(&descriptor).unwrap()).unwrap();
        assert_eq!(round_trip.schema, DESCRIPTOR_SCHEMA);
        assert_eq!(round_trip.ipc_token.len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn short_unix_endpoint_stays_under_data_directory() {
        let data_directory = PathBuf::from("/tmp/needle-instance-short");
        let endpoint_path = PathBuf::from(endpoint(&data_directory));

        assert_eq!(endpoint_path.parent(), Some(data_directory.as_path()));
        assert!(std::os::unix::net::SocketAddr::from_pathname(&endpoint_path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn overlong_unix_endpoint_uses_valid_tmp_fallback() {
        let data_directory = PathBuf::from("/tmp").join("needle-instance-overlong-".repeat(5));
        let candidate = data_directory.join("needle-placeholder.sock");
        assert!(std::os::unix::net::SocketAddr::from_pathname(&candidate).is_err());

        let endpoint_path = PathBuf::from(endpoint(&data_directory));
        assert!(endpoint_path.starts_with("/tmp"));
        assert!(!endpoint_path.starts_with(&data_directory));
        assert!(std::os::unix::net::SocketAddr::from_pathname(&endpoint_path).is_ok());

        let digest = Digest::blake3(data_directory.to_string_lossy().as_bytes()).to_string();
        let suffix = digest.trim_start_matches("b3:").chars().take(20).collect::<String>();
        assert_eq!(endpoint_path, PathBuf::from(format!("/tmp/needle-{suffix}.sock")));
    }

    #[tokio::test]
    async fn invalid_capability_fails_closed_without_resolving() {
        let request = IpcRequest::Health {
            schema: IPC_SCHEMA.to_owned(),
            request_id: "request".to_owned(),
            token: "wrong".to_owned(),
        };
        let root = std::env::temp_dir().join(format!(
            "needle-instance-auth-{}-{}",
            std::process::id(),
            crate::server::test_nonce()
        ));
        let response = handle_request(
            request,
            "correct",
            RuntimeStore::new(root.join("needle.sqlite3")),
            root.clone(),
        )
        .await;
        assert!(matches!(
            response,
            IpcResponse::Error { code, .. } if code == "unauthorized"
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn real_ipc_health_and_resolve_error_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "needle-instance-ipc-{}-{}",
            std::process::id(),
            crate::server::test_nonce()
        ));
        fs::create_dir_all(&root).unwrap();
        let store = RuntimeStore::new(root.join("needle.sqlite3"));
        store.initialize().unwrap();
        let endpoint = endpoint(&root);
        let token = "a".repeat(64);
        let task = tokio::spawn(serve_ipc(endpoint.clone(), token.clone(), store, root.clone()));
        wait_until_ready(&endpoint, &token).await.unwrap();

        let request_id = "missing-session".to_owned();
        let request = IpcRequest::Resolve {
            schema: IPC_SCHEMA.to_owned(),
            request_id: request_id.clone(),
            token,
            request: Box::new(ResolveRequest {
                session_id: "missing".to_owned(),
                turn_id: "turn".to_owned(),
                platform: "codex".to_owned(),
                main_model: "model".to_owned(),
                cwd: root.clone(),
                need: NeedRequest {
                    key: NeedKey::new("locate.implementation").unwrap(),
                    body: "find it".to_owned(),
                },
                need_ir: None,
                declared_test_plan: None,
            }),
        };
        let response = tokio::time::timeout(HEALTH_TIMEOUT, call_endpoint(&endpoint, &request))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            response,
            IpcResponse::Error {
                request_id: response_id,
                code,
                ..
            } if response_id == request_id && code == "resolve_failed"
        ));

        task.abort();
        let _ = task.await;
        #[cfg(unix)]
        let _ = fs::remove_file(&endpoint);
        let _ = fs::remove_dir_all(root);
    }
}

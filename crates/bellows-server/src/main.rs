use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use bellows_core::{
    ActionCandidate, ArchiveManifest, Artifact, CandidateIndex, DeclaredActionRecord,
    ExecuteRequest, ExecuteResponse, GcReport, GcRequest, HealthResponse, LeaseRequest,
    LeaseResponse, PROTOCOL_VERSION, PathNormalizer, PlatformIdentity, ServerStats, Store,
    atomic_write, declared_action_key, digest_bytes, now_ms, rustup_home,
    validate_declared_command, validate_relative_path,
};
use clap::Parser;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

#[derive(Parser, Debug)]
#[command(
    name = "bellowsd",
    version,
    about = "Bellows remote build cache server"
)]
struct Args {
    #[arg(long, env = "BELLOWS_LISTEN", default_value = "127.0.0.1:7878")]
    listen: SocketAddr,
    #[arg(long, env = "BELLOWS_DATA_DIR", default_value = ".bellows/server")]
    data_dir: PathBuf,
    #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
    auth_token: Option<String>,
    #[arg(long, default_value_t = 8)]
    max_candidates: usize,
    #[arg(long, env = "BELLOWS_MAX_BLOB_MB", default_value_t = 512)]
    max_blob_mb: usize,
    #[arg(long, env = "BELLOWS_ENABLE_EXECUTION", default_value_t = false)]
    enable_execution: bool,
    #[arg(long, env = "BELLOWS_MAX_EXECUTORS", default_value_t = 2)]
    max_executors: usize,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    auth_token: Option<String>,
    max_candidates: usize,
    leases: Arc<StdMutex<HashMap<String, Lease>>>,
    writes: Arc<StdMutex<()>>,
    enable_execution: bool,
    execution_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    executor_slots: Arc<Semaphore>,
}

#[derive(Clone)]
struct Lease {
    token: String,
    expires_ms: u64,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let Some(expected) = &state.auth_token else {
        return Ok(());
    };
    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if supplied == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token".into(),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.enable_execution && args.auth_token.is_none() {
        anyhow::bail!("--enable-execution requires --auth-token")
    }
    if args.enable_execution {
        PlatformIdentity::detect().context("detect remote executor toolchain")?;
    }
    let state = AppState {
        store: Store::open(&args.data_dir)?,
        auth_token: args.auth_token,
        max_candidates: args.max_candidates,
        leases: Arc::new(StdMutex::new(HashMap::new())),
        writes: Arc::new(StdMutex::new(())),
        enable_execution: args.enable_execution,
        execution_locks: Arc::new(AsyncMutex::new(HashMap::new())),
        executor_slots: Arc::new(Semaphore::new(args.max_executors.max(1))),
    };
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/stats", get(stats))
        .route("/v1/blobs/{digest}", get(get_blob).put(put_blob))
        .route("/v1/actions/{static_key}", get(get_candidates))
        .route("/v1/actions/{static_key}/{action_key}", put(put_candidate))
        .route("/v1/declared/{key}", get(get_declared).put(put_declared))
        .route("/v1/archives/{name}", get(get_archive).put(put_archive))
        .route("/v1/execute", post(execute))
        .route("/v1/admin/gc", post(gc))
        .route("/v1/leases/{static_key}", post(acquire_lease))
        .route("/v1/leases/{static_key}/{token}", delete(release_lease))
        .layer(DefaultBodyLimit::max(
            args.max_blob_mb.saturating_mul(1024 * 1024),
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    eprintln!(
        "bellowsd {} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        args.listen
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<HealthResponse>> {
    authorize(&state, &headers)?;
    Ok(Json(HealthResponse {
        service: "bellowsd".into(),
        protocol: PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").into(),
    }))
}

async fn get_blob(
    State(state): State<AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Vec<u8>> {
    authorize(&state, &headers)?;
    state.store.read_blob(&digest).map_err(|error| {
        if state.store.blob_path(&digest).is_ok_and(|p| !p.exists()) {
            ApiError(StatusCode::NOT_FOUND, "blob not found".into())
        } else {
            internal(error)
        }
    })
}

async fn put_blob(
    State(state): State<AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    if digest_bytes(&body) != digest {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "blob digest mismatch".into(),
        ));
    }
    let created = state.store.put_blob(&digest, &body).map_err(internal)?;
    Ok(if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    })
}

async fn get_candidates(
    State(state): State<AppState>,
    Path(static_key): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<CandidateIndex>> {
    authorize(&state, &headers)?;
    let index = state.store.read_candidates(&static_key).map_err(internal)?;
    if index.candidates.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, "action not found".into()));
    }
    Ok(Json(index))
}

async fn put_candidate(
    State(state): State<AppState>,
    Path((static_key, action_key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(candidate): Json<ActionCandidate>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    if candidate.static_key != static_key || candidate.action_key != action_key {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "action key does not match path".into(),
        ));
    }
    let mut referenced = candidate
        .artifacts
        .iter()
        .map(|a| &a.digest)
        .collect::<Vec<_>>();
    referenced.push(&candidate.stdout.digest);
    referenced.push(&candidate.stderr.digest);
    for digest in referenced {
        state.store.read_blob(digest).map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("referenced blob {digest} is absent or corrupt"),
            )
        })?;
    }
    let _guard = state
        .writes
        .lock()
        .map_err(|_| internal("write lock poisoned"))?;
    state
        .store
        .put_candidate(candidate, state.max_candidates)
        .map_err(internal)?;
    Ok(StatusCode::CREATED)
}

async fn get_declared(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<DeclaredActionRecord>> {
    authorize(&state, &headers)?;
    state
        .store
        .read_declared(&key)
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "declared action not found".into()))
}

async fn put_declared(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    Json(record): Json<DeclaredActionRecord>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    if record.key != key {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "declared key does not match path".into(),
        ));
    }
    verify_declared_record(&state.store, &record)?;
    let _guard = state
        .writes
        .lock()
        .map_err(|_| internal("write lock poisoned"))?;
    let created = state.store.put_declared(&record).map_err(internal)?;
    Ok(if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    })
}

async fn get_archive(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<ArchiveManifest>> {
    authorize(&state, &headers)?;
    state
        .store
        .read_archive(&name)
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "archive not found".into()))
}

async fn put_archive(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(manifest): Json<ArchiveManifest>,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    if manifest.name != name {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "archive name does not match path".into(),
        ));
    }
    for file in &manifest.files {
        validate_relative_path(&file.file_name)
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        state.store.read_blob(&file.digest).map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("archive blob {} is absent or corrupt", file.digest),
            )
        })?;
    }
    if bellows_core::tree_digest(&manifest.files) != manifest.tree_digest {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "archive tree digest mismatch".into(),
        ));
    }
    let _guard = state
        .writes
        .lock()
        .map_err(|_| internal("write lock poisoned"))?;
    let created = state.store.put_archive(&manifest).map_err(|error| {
        if error.to_string().contains("already bound") {
            ApiError(StatusCode::CONFLICT, error.to_string())
        } else {
            internal(error)
        }
    })?;
    Ok(if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    })
}

async fn execute(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExecuteRequest>,
) -> ApiResult<Json<ExecuteResponse>> {
    authorize(&state, &headers)?;
    if !state.enable_execution {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "remote execution is disabled".into(),
        ));
    }
    let platform = tokio::task::spawn_blocking(PlatformIdentity::detect)
        .await
        .map_err(internal)?
        .map_err(internal)?;
    if request.platform != platform {
        return Err(ApiError(
            StatusCode::PRECONDITION_FAILED,
            "executor toolchain identity does not match request".into(),
        ));
    }
    let expected_key = declared_action_key(
        &request.name,
        &request.platform,
        &request.command,
        &request.environment,
        &request.inputs,
        &request.outputs,
    );
    if request.key != expected_key {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "declared action key mismatch".into(),
        ));
    }
    validate_execution_request(&request)?;
    let action_lock = {
        let mut locks = state.execution_locks.lock().await;
        locks
            .entry(request.key.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _action_guard = action_lock.lock().await;
    if let Some(record) = state.store.read_declared(&request.key).map_err(internal)? {
        return Ok(Json(ExecuteResponse {
            cache_hit: true,
            record,
        }));
    }
    let _permit = state
        .executor_slots
        .acquire()
        .await
        .map_err(|_| internal("executor scheduler closed"))?;
    let store = state.store.clone();
    let record = tokio::task::spawn_blocking(move || execute_declared(&store, &platform, request))
        .await
        .map_err(internal)?
        .map_err(|error| ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    Ok(Json(ExecuteResponse {
        cache_hit: false,
        record,
    }))
}

async fn gc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GcRequest>,
) -> ApiResult<Json<GcReport>> {
    authorize(&state, &headers)?;
    let _guard = state
        .writes
        .lock()
        .map_err(|_| internal("write lock poisoned"))?;
    Ok(Json(state.store.gc(request.max_bytes).map_err(internal)?))
}

fn verify_declared_record(store: &Store, record: &DeclaredActionRecord) -> ApiResult<()> {
    validate_declared_command(&record.command)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let expected_key = declared_action_key(
        &record.name,
        &record.platform,
        &record.command,
        &record.environment,
        &record.inputs,
        &record.output_paths,
    );
    if expected_key != record.key {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "record key does not match manifest".into(),
        ));
    }
    for artifact in record.inputs.iter().chain(&record.outputs) {
        validate_relative_path(&artifact.file_name)
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        store.read_blob(&artifact.digest).map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("record blob {} is absent or corrupt", artifact.digest),
            )
        })?;
    }
    for output in &record.outputs {
        let covered = record.output_paths.iter().any(|declaration| {
            output.file_name == *declaration
                || output
                    .file_name
                    .strip_prefix(declaration)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        });
        if !covered {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "record output {} is outside declared roots",
                    output.file_name
                ),
            ));
        }
    }
    for stream in [&record.stdout, &record.stderr] {
        store.read_blob(&stream.digest).map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("stream blob {} is absent or corrupt", stream.digest),
            )
        })?;
    }
    Ok(())
}

fn validate_execution_request(request: &ExecuteRequest) -> ApiResult<()> {
    validate_declared_command(&request.command)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    for artifact in &request.inputs {
        validate_relative_path(&artifact.file_name)
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    }
    for output in &request.outputs {
        validate_relative_path(output)
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    }
    for forbidden in [
        "PATH",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        if request.environment.contains_key(forbidden) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("execution environment may not override {forbidden}"),
            ));
        }
    }
    Ok(())
}

fn execute_declared(
    store: &Store,
    platform: &PlatformIdentity,
    request: ExecuteRequest,
) -> Result<DeclaredActionRecord> {
    let temp = tempfile::Builder::new().prefix("bellows-exec-").tempdir()?;
    let workspace = temp.path().join("workspace");
    let cargo_home = workspace.join(".bellows-cargo-home");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&cargo_home)?;
    for input in &request.inputs {
        let relative = validate_relative_path(&input.file_name)?;
        let destination = workspace.join(relative);
        let bytes = store.read_blob(&input.digest)?;
        atomic_write(&destination, &bytes)?;
        set_executable(&destination, input.executable)?;
    }
    if let Some(delay) = request
        .environment
        .get("BELLOWS_ACTION_DELAY_MS")
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay.min(10_000)));
    }

    let started = Instant::now();
    let mut command = Command::new(&request.command[0]);
    command
        .args(&request.command[1..])
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/homeless-shelter")
        .env("CARGO_HOME", ".bellows-cargo-home")
        .env("CARGO_NET_OFFLINE", "true")
        .envs(&request.environment);
    command.env("RUSTUP_HOME", rustup_home());
    if let Some(value) = &platform.rustup_toolchain {
        command.env("RUSTUP_TOOLCHAIN", value);
    }
    let program = FsPath::new(&request.command[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if program == "cargo" {
        command.env(
            "CARGO_ENCODED_RUSTFLAGS",
            format!(
                "--remap-path-prefix\u{1f}{}=/bellows/action",
                workspace.display()
            ),
        );
    } else if program == "rustc" {
        command
            .arg("--remap-path-prefix")
            .arg(format!("{}=/bellows/action", workspace.display()));
    }
    let output = command
        .output()
        .with_context(|| format!("execute {}", request.command.join(" ")))?;
    let duration_ms = started.elapsed().as_millis() as u64;
    if !output.status.success() {
        bail!(
            "remote command failed with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    }

    let mut outputs = Vec::new();
    for declaration in &request.outputs {
        let path = workspace.join(validate_relative_path(declaration)?);
        collect_output_files(store, &workspace, &path, &mut outputs)?;
    }
    outputs.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    outputs.dedup_by(|left, right| left.file_name == right.file_name);
    if outputs.is_empty() {
        bail!("remote command produced no declared outputs")
    }

    let normalizer = PathNormalizer::new(vec![("$SANDBOX".into(), workspace.clone())]);
    let stdout = normalizer.normalize_bytes(&output.stdout);
    let stderr = normalizer.normalize_bytes(&output.stderr);
    let stdout_digest = digest_bytes(&stdout);
    let stderr_digest = digest_bytes(&stderr);
    store.put_blob(&stdout_digest, &stdout)?;
    store.put_blob(&stderr_digest, &stderr)?;
    let record = DeclaredActionRecord {
        protocol: PROTOCOL_VERSION,
        key: request.key,
        name: request.name,
        created_ms: now_ms(),
        platform: platform.clone(),
        command: request.command,
        environment: request.environment,
        inputs: request.inputs,
        output_paths: request.outputs,
        outputs,
        stdout: bellows_core::StreamArtifact {
            digest: stdout_digest,
            len: stdout.len() as u64,
        },
        stderr: bellows_core::StreamArtifact {
            digest: stderr_digest,
            len: stderr.len() as u64,
        },
        duration_ms,
        executor: format!("bellowsd:{}", std::process::id()),
    };
    store.put_declared(&record)?;
    Ok(record)
}

fn collect_output_files(
    store: &Store,
    workspace: &FsPath,
    path: &FsPath,
    outputs: &mut Vec<Artifact>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("declared output {} is missing", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("declared output may not be a symlink: {}", path.display())
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_output_files(store, workspace, &entry?.path(), outputs)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        bail!("declared output is not a regular file: {}", path.display())
    }
    let relative = path.strip_prefix(workspace)?;
    let relative = validate_relative_path(&relative.to_string_lossy())?;
    let bytes = fs::read(path)?;
    let digest = digest_bytes(&bytes);
    store.put_blob(&digest, &bytes)?;
    outputs.push(Artifact {
        file_name: relative.to_string_lossy().into_owned(),
        digest,
        executable: is_executable(path)?,
    });
    Ok(())
}

fn is_executable(path: &FsPath) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

fn set_executable(path: &FsPath, executable: bool) -> Result<()> {
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

async fn acquire_lease(
    State(state): State<AppState>,
    Path(static_key): Path<String>,
    headers: HeaderMap,
    Json(request): Json<LeaseRequest>,
) -> ApiResult<Json<LeaseResponse>> {
    authorize(&state, &headers)?;
    let now = now_ms();
    let mut leases = state
        .leases
        .lock()
        .map_err(|_| internal("lease lock poisoned"))?;
    leases.retain(|_, lease| lease.expires_ms > now);
    if let Some(lease) = leases.get(&static_key) {
        return Ok(Json(LeaseResponse::Wait {
            retry_after_ms: 150,
            expires_ms: lease.expires_ms,
        }));
    }
    let ttl = request.ttl_ms.clamp(1_000, 10 * 60 * 1_000);
    let token = digest_bytes(format!("{}:{static_key}:{now}", request.client_id).as_bytes());
    let expires_ms = now + ttl;
    leases.insert(
        static_key,
        Lease {
            token: token.clone(),
            expires_ms,
        },
    );
    Ok(Json(LeaseResponse::Owned { token, expires_ms }))
}

async fn release_lease(
    State(state): State<AppState>,
    Path((static_key, token)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    authorize(&state, &headers)?;
    let mut leases = state
        .leases
        .lock()
        .map_err(|_| internal("lease lock poisoned"))?;
    if leases
        .get(&static_key)
        .is_some_and(|lease| lease.token == token)
    {
        leases.remove(&static_key);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "lease not found".into()))
    }
}

async fn stats(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<ServerStats>> {
    authorize(&state, &headers)?;
    let (blobs, blob_bytes) =
        count_files(&state.store.root().join("blobs"), false).map_err(internal)?;
    let (action_indexes, _) =
        count_files(&state.store.root().join("actions"), true).map_err(internal)?;
    let (declared_actions, _) =
        count_files(&state.store.root().join("declared"), true).map_err(internal)?;
    let (archives, _) =
        count_files(&state.store.root().join("archives"), true).map_err(internal)?;
    let mut candidates = 0;
    visit_files(&state.store.root().join("actions"), &mut |path| {
        let bytes = fs::read(path)?;
        let index: CandidateIndex = serde_json::from_slice(&bytes)?;
        candidates += index.candidates.len() as u64;
        Ok(())
    })
    .map_err(internal)?;
    let active_leases = state
        .leases
        .lock()
        .map_err(|_| internal("lease lock poisoned"))?
        .values()
        .filter(|lease| lease.expires_ms > now_ms())
        .count() as u64;
    Ok(Json(ServerStats {
        blobs,
        blob_bytes,
        action_indexes,
        candidates,
        active_leases,
        declared_actions,
        archives,
    }))
}

fn count_files(root: &FsPath, only_json: bool) -> Result<(u64, u64)> {
    let mut count = 0;
    let mut bytes = 0;
    visit_files(root, &mut |path| {
        if !only_json || path.extension().is_some_and(|ext| ext == "json") {
            let metadata = fs::metadata(path)?;
            count += 1;
            bytes += metadata.len();
        }
        Ok(())
    })?;
    Ok((count, bytes))
}

fn visit_files(root: &FsPath, visitor: &mut impl FnMut(&FsPath) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            visit_files(&path, visitor)?;
        } else {
            visitor(&path)?;
        }
    }
    Ok(())
}

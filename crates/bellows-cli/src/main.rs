use anyhow::{Context, Result, anyhow, bail};
use bellows_core::{
    ActionCandidate, ArchiveManifest, Artifact, CandidateIndex, DeclaredActionRecord, EnvInput,
    Event, ExecuteRequest, ExecuteResponse, FileInput, GcReport, GcRequest, HealthResponse,
    LeaseRequest, LeaseResponse, PIN_PREFIX, PROTOCOL_VERSION, PathNormalizer, PlatformIdentity,
    ServerStats, Store, StreamArtifact, atomic_write, compiler_action_key, declared_action_key,
    digest_bytes, now_ms, parse_dep_info, rustup_home, tree_digest, validate_archive_manifest,
    validate_candidate_manifest, validate_declared_command, validate_declared_record,
    validate_relative_path,
};
use clap::{Args, Parser, Subcommand};
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod archive;
mod diagnostics;
mod digests;
mod leak;
mod link;
mod restore;
mod terminal;

#[derive(Parser, Debug)]
#[command(name = "bellows", version, about = "Cargo-native remote builds")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run Cargo through the daemonless local cache.
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Run an ordinary command with Bellows installed as Cargo's rustc wrapper.
    Run {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Run Cargo-compatible work against a daemonless, durable local cache.
    Local {
        /// Persistent cache directory (defaults to the platform user cache).
        #[arg(long, env = "BELLOWS_STATE_DIR")]
        cache_dir: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Check server connectivity and the local toolchain.
    Doctor {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
    },
    /// Show remote cache and local session statistics.
    Stats {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        selection: diagnostics::Selection,
    },
    /// Explain recent misses, bypasses, fallbacks, and integrity failures.
    Explain {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Group decisions by reason, with affected crates and examples.
        #[arg(long)]
        summary: bool,
        #[command(flatten)]
        selection: diagnostics::Selection,
    },
    /// Publish and restore immutable compile-once/test-many trees.
    Archive {
        #[command(subcommand)]
        command: ArchiveCommands,
    },
    /// Run an explicitly declared, sandboxed Cargo/rustc action locally.
    Action {
        #[command(subcommand)]
        command: ActionCommands,
    },
    /// Execute a declared Cargo/rustc action on bellowsd.
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },
    /// Capture and compare advisory compiler-aware workspace snapshots.
    Analyze {
        #[command(subcommand)]
        command: AnalyzeCommands,
    },
    /// Run a quiescent, reference-aware remote cache collection.
    Gc {
        #[arg(long)]
        max_mb: u64,
        /// Collect the daemonless local cache instead of a server.
        #[arg(long)]
        local: bool,
        #[arg(long, requires = "local")]
        cache_dir: Option<PathBuf>,
        #[command(flatten)]
        connection: ConnectionArgs,
    },
}

#[derive(Subcommand, Debug)]
enum ArchiveCommands {
    Publish {
        name: String,
        path: PathBuf,
        #[command(flatten)]
        connection: ConnectionArgs,
    },
    Restore {
        name: String,
        path: PathBuf,
        #[command(flatten)]
        connection: ConnectionArgs,
    },
}

#[derive(Subcommand, Debug)]
enum ActionCommands {
    Run(DeclaredRunArgs),
}

#[derive(Subcommand, Debug)]
enum RemoteCommands {
    Run(DeclaredRunArgs),
}

#[derive(Subcommand, Debug)]
enum AnalyzeCommands {
    Snapshot {
        name: String,
    },
    Compare {
        before: String,
        after: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug, Clone)]
struct ConnectionArgs {
    #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
    server: String,
    #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
    token: Option<String>,
}

#[derive(Args, Debug)]
struct DeclaredRunArgs {
    /// Use the daemonless local cache instead of bellowsd.
    #[arg(long)]
    local: bool,
    #[arg(long, requires = "local")]
    cache_dir: Option<PathBuf>,
    #[arg(long)]
    name: String,
    #[arg(long = "input", required = true)]
    inputs: Vec<PathBuf>,
    #[arg(long = "output", required = true)]
    outputs: Vec<PathBuf>,
    #[arg(long = "env")]
    environment: Vec<String>,
    #[command(flatten)]
    connection: ConnectionArgs,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    let result = if env::var_os(bellows_core::execution::REMAP_ENV).is_some() {
        bellows_core::execution::remap_compiler(&args[1..]).map(|status| status.code().unwrap_or(1))
    } else if is_wrapper_invocation(&args) {
        rustc_wrapper(&args[1..]).map(|status| status.code().unwrap_or(1))
    } else {
        run_cli()
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code.clamp(1, 255) as u8),
        Err(error) => {
            eprintln!(
                "{}",
                terminal::error(terminal::stderr_color(), &format!("{error:#}"))
            );
            ExitCode::FAILURE
        }
    }
}

fn is_wrapper_invocation(args: &[OsString]) -> bool {
    if args.len() < 2 {
        return false;
    }
    let name = Path::new(&args[1])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let name = name.strip_suffix(".exe").unwrap_or(name);
    name == "rustc" || name.starts_with("rustc-") || name == "clippy-driver"
}

fn run_cli() -> Result<i32> {
    match Cli::parse().command {
        Commands::Cargo { arguments } => {
            let command = std::iter::once(OsString::from("cargo"))
                .chain(arguments)
                .collect();
            run_local_command(None, command)
        }
        Commands::Run {
            server,
            token,
            command,
        } => run_command(server, token, command),
        Commands::Local { cache_dir, command } => run_local_command(cache_dir, command),
        Commands::Doctor { server, token } => {
            doctor(&server, token.as_deref())?;
            Ok(0)
        }
        Commands::Stats {
            server,
            token,
            json,
            selection,
        } => {
            if selection.local {
                show_local_stats(&selection, json)?;
            } else {
                show_stats(&server, token.as_deref(), &selection, json)?;
            }
            Ok(0)
        }
        Commands::Explain {
            limit,
            json,
            summary,
            selection,
        } => {
            explain(&selection, limit, json, summary)?;
            Ok(0)
        }
        Commands::Archive { command } => {
            run_archive(command)?;
            Ok(0)
        }
        Commands::Action { command } => {
            match command {
                ActionCommands::Run(args) => run_declared_action(args, false)?,
            }
            Ok(0)
        }
        Commands::Remote { command } => {
            match command {
                RemoteCommands::Run(args) => {
                    if args.local {
                        bail!("bellows remote run cannot be combined with --local")
                    }
                    run_declared_action(args, true)?
                }
            }
            Ok(0)
        }
        Commands::Analyze { command } => {
            run_analysis(command)?;
            Ok(0)
        }
        Commands::Gc {
            max_mb,
            local,
            cache_dir,
            connection,
        } => {
            let report = if local {
                local_store(cache_dir.as_deref(), true)?.gc(max_mb.saturating_mul(1024 * 1024))?
            } else {
                Remote::new(&connection.server, connection.token)?
                    .gc(max_mb.saturating_mul(1024 * 1024))?
            };
            println!(
                "{}",
                terminal::status(
                    terminal::stdout_color(),
                    "gc",
                    &format!(
                        "{} → {}",
                        human_bytes(report.bytes_before),
                        human_bytes(report.bytes_after)
                    ),
                    &format!(
                        "{} records · {} blobs evicted",
                        report.records_evicted, report.blobs_evicted
                    ),
                )
            );
            Ok(0)
        }
    }
}

fn run_command(server: String, token: Option<String>, command: Vec<OsString>) -> Result<i32> {
    let (program, arguments) = command.split_first().context("missing command")?;
    let workspace = env::current_dir()?.canonicalize()?;
    let state_dir = state_dir(&workspace);
    if let Err(error) = fs::create_dir_all(&state_dir) {
        return run_without_cache(program, arguments, &error.into());
    }
    let wrapper = env::current_exe()?.canonicalize()?;
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("RUSTC_WRAPPER", &wrapper)
        .env("BELLOWS_SERVER", server)
        .env_remove("BELLOWS_LOCAL_ONLY")
        .env("BELLOWS_WORKSPACE", &workspace)
        .env("BELLOWS_STATE_DIR", &state_dir);
    if let Some(token) = token {
        child.env("BELLOWS_AUTH_TOKEN", token);
    }
    eprintln!(
        "{}",
        terminal::status(
            terminal::stderr_color(),
            "running",
            "cargo",
            &display_command(program, arguments),
        )
    );
    digests::prune(&state_dir, Duration::from_secs(14 * 24 * 60 * 60));
    let session = diagnostics::BuildSession::start(&state_dir, &workspace);
    session.configure(&mut child);
    let result = child.status().context("start wrapped command");
    session.finish(result.as_ref().ok().and_then(ExitStatus::code).unwrap_or(1));
    Ok(result?.code().unwrap_or(1))
}

fn run_local_command(cache_dir: Option<PathBuf>, command: Vec<OsString>) -> Result<i32> {
    let (program, arguments) = command.split_first().context("missing command")?;
    let workspace = env::current_dir()?.canonicalize()?;
    let state_dir = match local_state_dir(cache_dir.as_deref()).and_then(|state| {
        local_store(Some(&state), true)?.check_writable()?;
        Ok(state)
    }) {
        Ok(state) => state,
        Err(error) => return run_without_cache(program, arguments, &error),
    };
    let wrapper = env::current_exe()?.canonicalize()?;
    eprintln!(
        "{}",
        terminal::status(
            terminal::stderr_color(),
            "running",
            "local",
            &display_command(program, arguments),
        )
    );
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("RUSTC_WRAPPER", &wrapper)
        .env("BELLOWS_LOCAL_ONLY", "1")
        .env("BELLOWS_WORKSPACE", &workspace)
        .env("BELLOWS_STATE_DIR", &state_dir)
        .env_remove("BELLOWS_SERVER")
        .env_remove("BELLOWS_AUTH_TOKEN");
    digests::prune(&state_dir, Duration::from_secs(14 * 24 * 60 * 60));
    let session = diagnostics::BuildSession::start(&state_dir, &workspace);
    session.configure(&mut child);
    let result = child.status().context("start locally wrapped command");
    session.finish(result.as_ref().ok().and_then(ExitStatus::code).unwrap_or(1));
    Ok(result?.code().unwrap_or(1))
}

fn run_without_cache(
    program: &OsStr,
    arguments: &[OsString],
    error: &anyhow::Error,
) -> Result<i32> {
    eprintln!(
        "{}",
        terminal::warning(
            terminal::stderr_color(),
            "cache disabled",
            &format!("cache initialization failed; running command normally: {error:#}")
        )
    );
    let mut child = Command::new(program);
    child.args(arguments);
    // Preserve an unrelated caller-supplied wrapper, but don't re-enter Bellows.
    if env::var_os("RUSTC_WRAPPER").is_some_and(|path| {
        Path::new(&path).canonicalize().ok()
            == env::current_exe().ok().and_then(|p| p.canonicalize().ok())
    }) {
        child.env_remove("RUSTC_WRAPPER");
    }
    Ok(child
        .status()
        .context("start command without cache")?
        .code()
        .unwrap_or(1))
}

fn local_state_dir(configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()?.join(path)
        });
    }
    if let Some(path) = env::var_os("BELLOWS_STATE_DIR") {
        return Ok(absolute_path(Path::new(&path), &env::current_dir()?));
    }
    if let Some(root) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(root).join("bellows"));
    }
    if let Some(root) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(root).join("Bellows"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache/bellows"));
    }
    bail!("cannot locate a user cache directory; pass --cache-dir")
}

fn local_store(cache_dir: Option<&Path>, maintenance: bool) -> Result<Store> {
    let root = local_state_dir(cache_dir)?.join(format!("store-v{PROTOCOL_VERSION}"));
    if maintenance {
        Store::open(root)
    } else {
        Store::open_for_access(root)
    }
}

fn display_command(program: &OsStr, args: &[OsString]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|v| v.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone)]
struct Remote {
    base: String,
    token: Option<String>,
    client: Client,
}

impl Remote {
    fn new(base: &str, token: Option<String>) -> Result<Self> {
        let parsed = Url::parse(base).context("parse Bellows server URL")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            bail!(
                "Bellows server must be an HTTP(S) base URL without credentials, query, or fragment"
            )
        }
        let connect_timeout = bounded_timeout(
            "BELLOWS_CONNECT_TIMEOUT_MS",
            env::var("BELLOWS_CONNECT_TIMEOUT_MS").ok().as_deref(),
            2_000,
            100,
            30_000,
        )?;
        let request_timeout = bounded_timeout(
            "BELLOWS_REQUEST_TIMEOUT_MS",
            env::var("BELLOWS_REQUEST_TIMEOUT_MS").ok().as_deref(),
            120_000,
            1_000,
            600_000,
        )?;
        Ok(Self {
            base: parsed.as_str().trim_end_matches('/').to_owned(),
            token: token.filter(|value| !value.is_empty()),
            client: Client::builder()
                .connect_timeout(connect_timeout)
                .timeout(request_timeout)
                .build()?,
        })
    }

    fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    fn health(&self) -> Result<HealthResponse> {
        Ok(self
            .auth(self.client.get(format!("{}/v1/health", self.base)))
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn stats(&self) -> Result<ServerStats> {
        Ok(self
            .auth(self.client.get(format!("{}/v1/stats", self.base)))
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn candidates(&self, static_key: &str) -> Result<CandidateIndex> {
        let response = self
            .auth(
                self.client
                    .get(format!("{}/v1/actions/{static_key}", self.base)),
            )
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(CandidateIndex::default());
        }
        Ok(response.error_for_status()?.json()?)
    }

    fn blob(&self, digest: &str) -> Result<Vec<u8>> {
        let bytes = self
            .auth(self.client.get(format!("{}/v1/blobs/{digest}", self.base)))
            .send()?
            .error_for_status()?
            .bytes()?
            .to_vec();
        let actual = digest_bytes(&bytes);
        if actual != digest {
            bail!("remote blob {digest} failed integrity verification (got {actual})")
        }
        Ok(bytes)
    }

    fn put_blob(&self, digest: &str, bytes: Vec<u8>) -> Result<()> {
        let present = self
            .auth(self.client.head(format!("{}/v1/blobs/{digest}", self.base)))
            .send()?;
        if present.status().is_success() {
            return Ok(());
        }
        self.auth(self.client.put(format!("{}/v1/blobs/{digest}", self.base)))
            .body(bytes)
            .send()?
            .error_for_status()?;
        Ok(())
    }

    fn declared(&self, key: &str) -> Result<Option<DeclaredActionRecord>> {
        let response = self
            .auth(self.client.get(format!("{}/v1/declared/{key}", self.base)))
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let record: DeclaredActionRecord = response.error_for_status()?.json()?;
        validate_declared_record(&record)?;
        if record.key != key {
            bail!("remote declared record does not match requested key")
        }
        Ok(Some(record))
    }

    fn put_declared(&self, record: &DeclaredActionRecord) -> Result<()> {
        validate_declared_record(record)?;
        self.auth(
            self.client
                .put(format!("{}/v1/declared/{}", self.base, record.key)),
        )
        .json(record)
        .send()?
        .error_for_status()?;
        Ok(())
    }

    fn archive(&self, name: &str) -> Result<Option<ArchiveManifest>> {
        let response = self
            .auth(self.client.get(format!("{}/v1/archives/{name}", self.base)))
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let manifest: ArchiveManifest = response.error_for_status()?.json()?;
        validate_archive_manifest(&manifest)?;
        if manifest.name != name {
            bail!("remote archive does not match requested name")
        }
        Ok(Some(manifest))
    }

    fn put_archive(&self, manifest: &ArchiveManifest) -> Result<()> {
        validate_archive_manifest(manifest)?;
        self.auth(
            self.client
                .put(format!("{}/v1/archives/{}", self.base, manifest.name)),
        )
        .json(manifest)
        .send()?
        .error_for_status()?;
        Ok(())
    }

    fn execute(&self, request: &ExecuteRequest) -> Result<ExecuteResponse> {
        Ok(self
            .auth(self.client.post(format!("{}/v1/execute", self.base)))
            .json(request)
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn gc(&self, max_bytes: u64) -> Result<GcReport> {
        Ok(self
            .auth(self.client.post(format!("{}/v1/admin/gc", self.base)))
            .json(&GcRequest { max_bytes })
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn put_candidate(&self, candidate: &ActionCandidate) -> Result<()> {
        self.auth(self.client.put(format!(
            "{}/v1/actions/{}/{}",
            self.base, candidate.static_key, candidate.action_key
        )))
        .json(candidate)
        .send()?
        .error_for_status()?;
        Ok(())
    }

    fn acquire(&self, static_key: &str, client_id: &str) -> Result<LeaseResponse> {
        Ok(self
            .auth(
                self.client
                    .post(format!("{}/v1/leases/{static_key}", self.base)),
            )
            .json(&LeaseRequest {
                client_id: client_id.into(),
                ttl_ms: 120_000,
            })
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn release(&self, static_key: &str, token: &str) {
        let _ = self
            .auth(
                self.client
                    .delete(format!("{}/v1/leases/{static_key}/{token}", self.base)),
            )
            .send();
    }
}

fn bounded_timeout(
    name: &str,
    value: Option<&str>,
    default_ms: u64,
    minimum_ms: u64,
    maximum_ms: u64,
) -> Result<Duration> {
    let milliseconds = match value {
        Some(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} must be an integer number of milliseconds"))?,
        None => default_ms,
    };
    if !(minimum_ms..=maximum_ms).contains(&milliseconds) {
        bail!("{name} must be between {minimum_ms} and {maximum_ms} milliseconds")
    }
    Ok(Duration::from_millis(milliseconds))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    /// rlib/rmeta/dep-info only: rustc never runs a linker.
    Library,
    /// Executables, test harnesses, cdylib/dylib/proc-macro shared objects.
    Linked,
}

impl OutputKind {
    fn name(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Linked => "linked",
        }
    }
}

/// File naming for the compilation target (Cargo omits `--target` for host
/// units such as build scripts and procedural macros).
#[derive(Debug, Clone, Copy)]
struct Naming {
    exe_suffix: &'static str,
    dll_prefix: &'static str,
    dll_suffix: &'static str,
    msvc: bool,
}

impl Naming {
    fn for_target(target: Option<&str>) -> Self {
        let (windows, msvc, apple, wasm) = match target {
            Some(triple) => (
                triple.contains("windows"),
                triple.contains("msvc"),
                triple.contains("apple"),
                triple.starts_with("wasm"),
            ),
            None => (
                cfg!(windows),
                cfg!(target_env = "msvc"),
                cfg!(target_vendor = "apple"),
                false,
            ),
        };
        if wasm {
            Self {
                exe_suffix: ".wasm",
                dll_prefix: "",
                dll_suffix: ".wasm",
                msvc: false,
            }
        } else if windows {
            Self {
                exe_suffix: ".exe",
                dll_prefix: "",
                dll_suffix: ".dll",
                msvc,
            }
        } else if apple {
            Self {
                exe_suffix: "",
                dll_prefix: "lib",
                dll_suffix: ".dylib",
                msvc: false,
            }
        } else {
            Self {
                exe_suffix: "",
                dll_prefix: "lib",
                dll_suffix: ".so",
                msvc: false,
            }
        }
    }
}

/// A `-l` request on a library compile whose file rustc bundles into the rlib.
#[derive(Debug, Clone)]
struct StaticLibrary {
    name: String,
    verbatim: bool,
}

#[derive(Debug)]
struct Invocation {
    rustc: PathBuf,
    args: Vec<String>,
    crate_name: String,
    out_dir: PathBuf,
    kind: OutputKind,
    naming: Naming,
    /// Output file-name stems owned by this unit (`{crate}{extra}`, `lib…`).
    stems: Vec<String>,
    /// Outputs that must exist after a successful compile.
    expected_names: BTreeSet<String>,
    explicit_inputs: Vec<PathBuf>,
    /// Crate names of directly loaded procedural macros.
    proc_macros: Vec<String>,
    /// Cargo's shared incremental directory (`-C incremental=`).
    incremental: Option<PathBuf>,
    /// `{crate}{extra-filename}`: unique per Cargo unit.
    unit: String,
    /// This compile produces a procedural macro (it runs inside rustc).
    proc_macro_crate: bool,
    static_libraries: Vec<StaticLibrary>,
    native_search: Vec<PathBuf>,
}

/// Crates found in the sysroot; Cargo passes them to `--extern` without a
/// path (`--extern proc_macro`). The compiler identity covers them.
const SYSROOT_CRATES: &[&str] = &["proc_macro", "test", "std", "core", "alloc"];

impl Invocation {
    fn analyze(raw: &[OsString]) -> std::result::Result<Self, String> {
        let rustc = PathBuf::from(raw.first().ok_or("missing rustc executable")?);
        let args = raw[1..]
            .iter()
            .map(|v| {
                v.to_str()
                    .map(str::to_owned)
                    .ok_or("non-UTF-8 rustc argument")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if args
            .iter()
            .any(|a| a == "-vV" || a.starts_with("--print") || a == "-")
        {
            return Err("compiler probe".into());
        }
        if args.iter().any(|arg| arg.starts_with('@')) {
            return Err("rustc response files are not modeled".into());
        }
        if option_value(&args, "--sysroot").is_some() {
            return Err("custom sysroot contents are not modeled".into());
        }
        let target = option_value(&args, "--target");
        if target
            .as_deref()
            .is_some_and(|target| target.ends_with(".json") || Path::new(&target).is_file())
        {
            return Err("custom target specification contents are not modeled".into());
        }
        if codegen_value(&args, "target-cpu").as_deref() == Some("native") {
            return Err("host-native CPU features are not modeled in the cache key".into());
        }
        if codegen_values(&args)
            .any(|value| value == "save-temps" || value.starts_with("save-temps="))
        {
            return Err("compiler temporary outputs are not modeled".into());
        }
        if let Some(flag) = unmodeled_unstable_flag(&args) {
            return Err(format!("unstable compiler flag -Z {flag} is not modeled"));
        }
        let crate_name = option_value(&args, "--crate-name").ok_or("missing --crate-name")?;
        let test_harness = args.iter().any(|arg| arg == "--test");
        let declared_crate_types = multi_option_values(&args, "--crate-type");
        let crate_types = if test_harness || declared_crate_types.is_empty() {
            // `--test` builds a harness executable whatever the crate type.
            vec!["bin".to_owned()]
        } else {
            declared_crate_types
                .iter()
                .flat_map(|value| value.split(','))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        if let Some(kind) = crate_types.iter().find(|kind| {
            !matches!(
                kind.as_str(),
                "lib" | "rlib" | "bin" | "cdylib" | "dylib" | "proc-macro"
            )
        }) {
            return Err(format!("linked crate type {kind} is not modeled"));
        }
        let incremental = codegen_value(&args, "incremental").map(PathBuf::from);
        let (native_search, static_libraries) = native_inputs(&args)?;
        let out_dir = PathBuf::from(option_value(&args, "--out-dir").ok_or("missing --out-dir")?);
        // Cargo omits the extra filename for executables on MSVC and wasm
        // (their debug-info and output names must stay predictable); the
        // unit is still unique within its output directory.
        let executable_only = crate_types.iter().all(|kind| kind == "bin");
        let extra_filename = match codegen_value(&args, "extra-filename") {
            Some(extra) if extra.is_empty() || extra.contains(['/', '\\']) => {
                return Err("ambiguous extra filename".into());
            }
            Some(extra) => extra,
            None if executable_only => String::new(),
            None => return Err("missing -C extra-filename".into()),
        };
        let source = args
            .iter()
            .find(|arg| arg.ends_with(".rs") && Path::new(arg.as_str()).exists())
            .map(PathBuf::from)
            .ok_or("missing primary Rust source")?;

        let mut explicit_inputs = vec![source.clone()];
        let mut proc_macros = Vec::new();
        for value in multi_option_values(&args, "--extern") {
            let Some((name, path)) = value.split_once('=') else {
                let name = value.rsplit(':').next().unwrap_or(&value);
                if SYSROOT_CRATES.contains(&name) {
                    continue;
                }
                return Err("extern dependency has no explicit artifact path".into());
            };
            let path = PathBuf::from(path);
            if !path.is_file() {
                return Err("extern dependency artifact is missing".into());
            }
            let ext = path.extension().and_then(OsStr::to_str).unwrap_or_default();
            if matches!(ext, "so" | "dylib" | "dll") {
                proc_macros.push(name.rsplit(':').next().unwrap_or(name).to_owned());
            }
            explicit_inputs.push(path);
        }

        let emits = multi_option_values(&args, "--emit");
        if emits.len() != 1
            || emits[0]
                .split(',')
                .any(|kind| !matches!(kind, "dep-info" | "metadata" | "link"))
            || args.iter().any(|arg| arg.starts_with("-o"))
        {
            return Err("unsupported emit set or custom output destination; running rustc to produce every requested output".into());
        }
        let emit = emits[0].split(',').collect::<BTreeSet<_>>();
        if !emit.contains("dep-info") {
            return Err("unsupported emit set".into());
        }
        let naming = Naming::for_target(target.as_deref());
        let unit = format!("{crate_name}{extra_filename}");
        let library = format!("lib{unit}");
        let mut expected_names = BTreeSet::from([format!("{unit}.d")]);
        let has = |kind: &str| crate_types.iter().any(|t| t == kind);
        let mut linked = false;
        if emit.contains("metadata") {
            expected_names.insert(format!("{library}.rmeta"));
        }
        if emit.contains("link") {
            if has("lib") || has("rlib") {
                expected_names.insert(format!("{library}.rlib"));
            }
            if has("bin") {
                expected_names.insert(format!("{unit}{}", naming.exe_suffix));
                linked = true;
            }
            if has("cdylib") || has("dylib") || has("proc-macro") {
                expected_names.insert(format!("{}{unit}{}", naming.dll_prefix, naming.dll_suffix));
                linked = true;
            }
        }
        let kind = if linked {
            OutputKind::Linked
        } else {
            OutputKind::Library
        };
        if has_unmodeled_codegen_inputs(&args) {
            return Err("native linker or external codegen inputs are not modeled".into());
        }
        let stems = vec![unit.clone(), library];
        let proc_macro_crate = has("proc-macro");
        Ok(Self {
            rustc,
            args,
            crate_name,
            out_dir,
            kind,
            naming,
            stems,
            expected_names,
            explicit_inputs,
            proc_macros,
            incremental,
            unit,
            proc_macro_crate,
            static_libraries,
            native_search,
        })
    }

    /// True for a file name this unit's compile writes.
    fn owns(&self, file_name: &str) -> bool {
        self.stems.iter().any(|stem| {
            file_name
                .strip_prefix(stem.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
    }
}

/// Parse native search paths and libraries. Library search directories
/// (`-L native=`, `-L framework=`) cannot supply Rust crates; `-L all=`,
/// `crate=` and bare `-L` can, so they remain unmodeled.
fn native_inputs(
    args: &[String],
) -> std::result::Result<(Vec<PathBuf>, Vec<StaticLibrary>), String> {
    let mut search = Vec::new();
    let mut libraries = Vec::new();
    let mut values = Vec::<(&str, &str)>::new();
    for (index, arg) in args.iter().enumerate() {
        for flag in ["-L", "-l"] {
            if arg == flag {
                if let Some(value) = args.get(index + 1) {
                    values.push((flag, value));
                }
            } else if let Some(value) = arg.strip_prefix(flag)
                && !value.is_empty()
            {
                values.push((flag, value));
            }
        }
    }
    for (flag, value) in values {
        if flag == "-L" {
            match value.split_once('=') {
                Some(("dependency", _)) => {}
                Some(("native" | "framework", path)) => search.push(PathBuf::from(path)),
                _ => {
                    return Err(format!(
                        "native linker or external codegen inputs are not modeled: search path -L {value} may supply crates"
                    ));
                }
            }
            continue;
        }
        let (kind, name) = match value.split_once('=') {
            Some((kind, name)) => (kind, name),
            None => ("dylib", value),
        };
        let (kind, modifiers) = kind.split_once(':').unwrap_or((kind, ""));
        let name = name.split(':').next().unwrap_or(name);
        match kind {
            "static" => {
                if !modifiers.split(',').any(|m| m == "-bundle") {
                    libraries.push(StaticLibrary {
                        name: name.to_owned(),
                        verbatim: modifiers.split(',').any(|m| m == "+verbatim"),
                    });
                }
            }
            "dylib" | "framework" | "raw-dylib" => {}
            _ => {
                return Err(format!(
                    "native linker or external codegen inputs are not modeled: -l {value}"
                ));
            }
        }
    }
    Ok((search, libraries))
}

/// Unstable flags whose effect is fully described by the arguments (kept in
/// the key) and whose inputs arrive through dep-info. `-Zbuild-std` passes
/// the first two to every crate; the rust-src sources are tracked files and
/// the nightly compiler is part of the identity. Anything else that could
/// write extra files or read untracked inputs stays unmodeled.
const MODELED_UNSTABLE_FLAGS: &[&str] = &[
    "unstable-options",
    "force-unstable-if-unmarked",
    "share-generics",
    "threads",
    "macro-backtrace",
];

fn unmodeled_unstable_flag(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(index, arg)| {
        let value = if arg == "-Z" {
            args.get(index + 1).map(String::as_str).unwrap_or_default()
        } else {
            arg.strip_prefix("-Z")?
        };
        let name = value.split('=').next().unwrap_or(value);
        (!MODELED_UNSTABLE_FLAGS.contains(&name)).then(|| name.to_owned())
    })
}

/// Codegen inputs that remain outside the model for any output kind.
fn has_unmodeled_codegen_inputs(args: &[String]) -> bool {
    codegen_values(args).any(|value| {
        ["linker-plugin-lto=", "profile-use=", "llvm-plugins="]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    })
}

fn codegen_values(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter().enumerate().filter_map(|(i, arg)| {
        if arg == "-C" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("-C")
        }
    })
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == name {
            args.get(i + 1).cloned()
        } else {
            arg.strip_prefix(&format!("{name}=")).map(str::to_owned)
        }
    })
}

fn multi_option_values(args: &[String], name: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter_map(|(i, arg)| {
            if arg == name {
                args.get(i + 1).cloned()
            } else {
                arg.strip_prefix(&format!("{name}=")).map(str::to_owned)
            }
        })
        .collect()
}

fn codegen_value(args: &[String], name: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == "-C" {
            args.get(i + 1)?
                .strip_prefix(&format!("{name}="))
                .map(str::to_owned)
        } else {
            arg.strip_prefix("-C")?
                .strip_prefix(&format!("{name}="))
                .map(str::to_owned)
        }
    })
}

struct Identity {
    static_key: String,
    normalizer: PathNormalizer,
    /// Normalizes only the workspace and target roots: path-valued `env!`
    /// dependencies compare in this form when the outputs embed no root.
    root_normalizer: PathNormalizer,
    workspace: PathBuf,
    fingerprint: diagnostics::Fingerprint,
    diagnostic_group: String,
    digests: digests::Digests,
    /// Synthetic inputs that pin a leaking candidate to this checkout.
    pins: Vec<(String, String)>,
    /// Environment overrides given to rustc (see `stable_empty_directories`).
    virtual_env: BTreeMap<String, String>,
}

impl Identity {
    /// The value rustc sees for an environment variable.
    fn env_value(&self, name: &str) -> Option<String> {
        self.virtual_env
            .get(name)
            .cloned()
            .or_else(|| env::var(name).ok())
    }

    fn pin_value(&self, name: &str) -> Option<&str> {
        self.pins
            .iter()
            .find(|(pin, _)| pin == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A successful compile whose result is deliberately not stored.
#[derive(Debug)]
struct NotStored(String);

impl std::fmt::Display for NotStored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotStored {}

fn rustc_wrapper(raw: &[OsString]) -> Result<ExitStatus> {
    let started = Instant::now();
    let result = match cache_or_compile(raw) {
        Ok(status) => Ok(status),
        Err(error) => {
            let crate_name = wrapper_crate_name(raw);
            record_event(
                "fallback",
                crate_name,
                None,
                None,
                &format!("cache pipeline failed; retrying official rustc: {error:#}"),
            );
            passthrough(raw)
        }
    };
    record_event_duration(
        "compiler_timing",
        wrapper_crate_name(raw),
        None,
        None,
        "compiler wrapper elapsed time",
        Some(started.elapsed().as_millis() as u64),
    );
    result
}

fn wrapper_crate_name(raw: &[OsString]) -> &str {
    raw.windows(2)
        .find(|pair| pair[0] == "--crate-name")
        .and_then(|pair| pair[1].to_str())
        .unwrap_or("rustc")
}

fn cache_or_compile(raw: &[OsString]) -> Result<ExitStatus> {
    let local_only = env::var("BELLOWS_LOCAL_ONLY").as_deref() == Ok("1");
    let invocation = match Invocation::analyze(raw) {
        Ok(invocation) => invocation,
        Err(reason) => {
            record_event("bypass", wrapper_crate_name(raw), None, None, &reason);
            return passthrough(raw);
        }
    };
    let identity = match build_identity(&invocation) {
        Ok(identity) => identity,
        Err(error) => {
            record_event(
                "fallback",
                &invocation.crate_name,
                None,
                None,
                &format!("identity: {error:#}"),
            );
            return passthrough(raw);
        }
    };
    let remote = if local_only {
        None
    } else {
        let server = env::var("BELLOWS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
        match Remote::new(&server, env::var("BELLOWS_AUTH_TOKEN").ok()) {
            Ok(remote) => Some(remote),
            Err(error) => {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("invalid remote configuration: {error}"),
                );
                return passthrough(raw);
            }
        }
    };
    let l1_disabled = env::var("BELLOWS_L1").as_deref() == Ok("0");
    let l1 = if l1_disabled {
        None
    } else {
        let store = if local_only {
            local_store(Some(&state_dir(&identity.workspace)), false)
        } else {
            Store::open_for_access(
                state_dir(&identity.workspace).join(format!("l1-v{PROTOCOL_VERSION}")),
            )
        };
        match store {
            Ok(store) => Some(store),
            Err(error) => {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("L1 is unavailable: {error:#}"),
                );
                None
            }
        }
    };
    let identity_hint = diagnostics::remember_identity(
        &state_dir(&identity.workspace),
        &identity.diagnostic_group,
        &identity.fingerprint,
    )
    .unwrap_or_else(|error| {
        format!("no usable candidate; diagnostic identity history unavailable: {error:#}")
    });
    let mut local_index = CandidateIndex::default();
    if let Some(store) = &l1 {
        match store.read_candidates(&identity.static_key) {
            Ok(index) => {
                local_index = index;
                match try_l1_candidates(store, &invocation, &identity, &local_index) {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {}
                    Err(error) => record_event(
                        "fallback",
                        &invocation.crate_name,
                        Some(&identity.static_key),
                        None,
                        &format!("L1 restore failed: {error:#}"),
                    ),
                }
            }
            Err(error) => record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("L1 index is unavailable or corrupt: {error:#}"),
            ),
        }
    }

    let (index, remote_available) = match &remote {
        Some(remote) => match remote.candidates(&identity.static_key) {
            Ok(index) => (index, true),
            Err(error) => {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("remote unavailable: {error}"),
                );
                (CandidateIndex::default(), false)
            }
        },
        None => (CandidateIndex::default(), false),
    };
    if let Some(remote) = &remote
        && let Some(status) = try_candidates(remote, l1.as_ref(), &invocation, &identity, &index)?
    {
        return Ok(status);
    }

    let mut reasons = Vec::new();
    if !local_index.candidates.is_empty() {
        reasons.push(format!(
            "local cache: {}",
            explain_candidates(&identity, &local_index)
        ));
    }
    if !index.candidates.is_empty() {
        reasons.push(format!(
            "remote cache: {}",
            explain_candidates(&identity, &index)
        ));
    }
    if remote.is_some() && !remote_available {
        reasons.push("remote unavailable (see fallback event for request error)".into());
    }
    // An L1 turned off with BELLOWS_L1=0 is configuration, reported once by
    // `doctor`; only a failure to open it (already a fallback) explains a miss.
    if l1.is_none() && !l1_disabled {
        reasons.push("local cache unavailable (see fallback event)".into());
    }
    if local_index.candidates.is_empty() && index.candidates.is_empty() {
        reasons.push(identity_hint);
    }
    let miss_detail = reasons.join("; ");
    record_event(
        "miss",
        &invocation.crate_name,
        Some(&identity.static_key),
        None,
        &miss_detail,
    );

    let client_id = format!("{}-{}", std::process::id(), now_ms());
    let mut owned_token = None;
    if let Some(remote) = &remote
        && remote_available
    {
        match remote.acquire(&identity.static_key, &client_id) {
            Ok(LeaseResponse::Owned { token, .. }) => owned_token = Some(token),
            Ok(LeaseResponse::Wait {
                retry_after_ms,
                expires_ms,
            }) => {
                record_event(
                    "wait",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    "another runner owns this cold action",
                );
                let max_wait = match bounded_timeout(
                    "BELLOWS_MAX_WAIT_MS",
                    env::var("BELLOWS_MAX_WAIT_MS").ok().as_deref(),
                    30_000,
                    0,
                    600_000,
                ) {
                    Ok(duration) => duration,
                    Err(error) => {
                        record_event(
                            "fallback",
                            &invocation.crate_name,
                            Some(&identity.static_key),
                            None,
                            &format!("invalid single-flight wait limit: {error}"),
                        );
                        Duration::ZERO
                    }
                };
                let deadline = Instant::now() + max_wait;
                let mut checked_candidates = index
                    .candidates
                    .iter()
                    .map(|candidate| candidate.action_key.clone())
                    .collect::<BTreeSet<_>>();
                let mut lease_failed = false;
                while Instant::now() < deadline && now_ms() < expires_ms {
                    thread::sleep(Duration::from_millis(retry_after_ms.clamp(50, 1_000)));
                    if let Ok(mut index) = remote.candidates(&identity.static_key) {
                        index.candidates.retain(|candidate| {
                            checked_candidates.insert(candidate.action_key.clone())
                        });
                        if let Some(status) =
                            try_candidates(remote, l1.as_ref(), &invocation, &identity, &index)?
                        {
                            record_event(
                                "single_flight",
                                &invocation.crate_name,
                                Some(&identity.static_key),
                                None,
                                "restored result published by lease owner",
                            );
                            return Ok(status);
                        }
                    }
                    // A static key can have several valid candidates (for example,
                    // different exact OUT_DIR dependencies). Once the owner releases
                    // its lease, compile our variant instead of waiting for an
                    // incompatible result until the original lease expires.
                    match remote.acquire(&identity.static_key, &client_id) {
                        Ok(LeaseResponse::Owned { token, .. }) => {
                            owned_token = Some(token);
                            record_event(
                                "lease_acquired",
                                &invocation.crate_name,
                                Some(&identity.static_key),
                                None,
                                "acquired released lease; no published candidate matches this invocation",
                            );
                            break;
                        }
                        Ok(LeaseResponse::Wait { .. }) => {}
                        Err(error) => {
                            record_event(
                                "fallback",
                                &invocation.crate_name,
                                Some(&identity.static_key),
                                None,
                                &format!("lease unavailable while waiting: {error}"),
                            );
                            lease_failed = true;
                            break;
                        }
                    }
                }
                if owned_token.is_none() && !lease_failed {
                    record_event(
                        "fallback",
                        &invocation.crate_name,
                        Some(&identity.static_key),
                        None,
                        "single-flight wait timed out; compiling locally",
                    );
                }
            }
            Err(error) => record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("lease unavailable: {error}"),
            ),
        }
    }

    let outcome = compile_and_capture(&invocation, &identity);
    let (status, captured) = match outcome {
        Ok(value) => value,
        Err(error) => {
            if let Some(token) = &owned_token
                && let Some(remote) = &remote
            {
                remote.release(&identity.static_key, token);
            }
            return Err(error);
        }
    };
    if !status.success() {
        if let Some(token) = &owned_token
            && let Some(remote) = &remote
        {
            remote.release(&identity.static_key, token);
        }
        return Ok(status);
    }
    // A read-only client (for example CI jobs sharing a developer's local
    // service) restores verified results but never publishes its own.
    let captured = captured.filter(|_| env::var("BELLOWS_READ_ONLY").as_deref() != Ok("1"));
    if let Some(captured) = captured {
        let scope = if captured.pinned {
            format!(
                " ({} output, pinned to this checkout: it embeds the workspace or target path)",
                invocation.kind.name()
            )
        } else {
            format!(
                " ({} output, shareable across checkouts)",
                invocation.kind.name()
            )
        };
        let mut stored_locally = false;
        if let Some(store) = &l1 {
            match cache_captured(store, &captured) {
                Ok(()) => stored_locally = true,
                Err(error) => record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("L1 publication failed: {error:#}"),
                ),
            }
        }
        if stored_locally && remote.is_none() {
            record_event(
                "store",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&captured.candidate.action_key),
                &format!("stored compiler result{scope}"),
            );
        }
        if let Some(remote) = &remote
            && remote_available
        {
            match publish(remote, captured) {
                Ok(action_key) => record_event(
                    "store",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&action_key),
                    &format!("published compiler result{scope}"),
                ),
                Err(error) => record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("upload failed after successful local compile: {error:#}"),
                ),
            }
        }
    }
    if let Some(token) = &owned_token
        && let Some(remote) = &remote
    {
        remote.release(&identity.static_key, token);
    }
    Ok(status)
}

fn passthrough(raw: &[OsString]) -> Result<ExitStatus> {
    let (rustc, args) = raw.split_first().context("missing rustc executable")?;
    Ok(Command::new(rustc).args(args).status()?)
}

fn normalizer(workspace: &Path, out_dir: &Path) -> PathNormalizer {
    let mut bases = root_bases(workspace, out_dir);
    let home = bellows_core::user_home();
    if let Some(cargo_home) = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")))
    {
        bases.push(("$CARGO_HOME".into(), canonical_base(cargo_home)));
    }
    bases.push(("$RUSTUP_HOME".into(), canonical_base(rustup_home())));
    if let Some(home) = home {
        bases.push(("$HOME".into(), canonical_base(home)));
    }
    PathNormalizer::new(bases)
}

/// The per-checkout roots: the workspace and the Cargo target directory.
fn root_bases(workspace: &Path, out_dir: &Path) -> Vec<(String, PathBuf)> {
    let target = target_root(workspace, out_dir);
    let mut bases = vec![("$WORKSPACE".into(), workspace.to_path_buf())];
    // The directory a `bellows run/local` session started in. A nested Cargo
    // build (a build script compiling another workspace) runs rustc from a
    // subdirectory but inherits paths such as OUT_DIR from the outer build.
    if let Some(checkout) = env::var_os("BELLOWS_WORKSPACE").map(PathBuf::from)
        && checkout.is_absolute()
        && checkout != workspace
    {
        bases.push(("$CHECKOUT".into(), canonical_base(checkout.clone())));
        bases.push(("$CHECKOUT".into(), checkout));
    }
    // Cargo's manifest directory may retain an 8.3 spelling even when the
    // process working directory has already been expanded by Windows.
    for alias in [
        env::current_dir().ok(),
        env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        if alias.canonicalize().ok().as_deref() == workspace.canonicalize().ok().as_deref() {
            bases.push(("$WORKSPACE".into(), alias));
        }
    }
    if let Some(target) = target {
        // Profiles that pass identical rustc arguments (for example
        // `release` and a `test-fast` inheriting it) differ only in their
        // `<target>/<profile>` directory. Naming it separately lets one
        // verified result serve every such profile.
        if let Some(profile) = profile_root(&target, out_dir) {
            bases.push(("$PROFILE".into(), canonical_base(profile.clone())));
            bases.push(("$PROFILE".into(), profile));
        }
        bases.push(("$TARGET".into(), canonical_base(target.clone())));
        // Retain the caller's spelling too: Windows temp directories may use
        // 8.3 names that canonicalize() expands to a different path string.
        bases.push(("$TARGET".into(), target));
    }
    bases
}

/// `<target>[/<triple>]/<profile>` for Cargo's `deps`, `build/<unit>` and
/// `examples` output directories.
fn profile_root(target: &Path, out_dir: &Path) -> Option<PathBuf> {
    out_dir
        .ancestors()
        .find(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "deps" || name == "build" || name == "examples")
        })
        .and_then(Path::parent)
        .filter(|profile| profile.starts_with(target) && *profile != target)
        .map(Path::to_path_buf)
}

fn canonical_base(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn target_root(workspace: &Path, out_dir: &Path) -> Option<PathBuf> {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| absolute_path(&path, workspace))
        .or_else(|| infer_target_root(out_dir))
}

fn infer_target_root(out_dir: &Path) -> Option<PathBuf> {
    out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "target"))
        .map(Path::to_path_buf)
        .or_else(|| {
            // Cargo's --target-dir flag is not exported to rustc as
            // CARGO_TARGET_DIR. Compiler products still have the stable
            // <root>/<profile>/deps shape (or <root>/<triple>/<profile>/deps
            // for cross compilation), so normalize the nearest complete
            // target subtree even when its directory has a custom name.
            (out_dir.file_name().is_some_and(|name| name == "deps"))
                .then(|| out_dir.parent()?.parent().map(Path::to_path_buf))
                .flatten()
        })
}

fn absolute_path(path: &Path, workspace: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

// Canonicalizing a symlink loses the dependency on its resolution. Until the
// manifest models that resolution, do not cache any input reached through one.
fn canonical_compiler_input(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
            bail!(
                "symlinked compiler input is not cacheable: {}",
                ancestor.display()
            );
        }
    }
    Ok(path.canonicalize()?)
}

fn normalized_compiler_arguments(
    invocation: &Invocation,
    normalizer: &PathNormalizer,
) -> Vec<String> {
    let source = invocation
        .explicit_inputs
        .first()
        .map(|path| path.to_string_lossy());
    let mut normalized = Vec::with_capacity(invocation.args.len());
    let mut path_value = false;
    let mut skip_next = false;
    for (index, arg) in invocation.args.iter().enumerate() {
        // The incremental session directory is scratch state, not an input:
        // incremental and non-incremental compiles of the same crate share
        // one identity (see the incremental policy in compile_and_capture).
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if arg == "-C"
            && invocation
                .args
                .get(index + 1)
                .is_some_and(|value| value.starts_with("incremental="))
        {
            skip_next = true;
            continue;
        }
        if arg.starts_with("-Cincremental=") {
            continue;
        }
        let is_path = path_value
            || source.as_deref() == Some(arg.as_str())
            || arg.starts_with("--out-dir=")
            || arg.starts_with("--extern=")
            || arg.starts_with("-Ldependency=");
        normalized.push(if is_path {
            normalizer.normalize(arg)
        } else {
            arg.clone()
        });
        path_value = matches!(arg.as_str(), "--out-dir" | "--extern" | "-L");
    }
    normalized
}

fn build_identity(invocation: &Invocation) -> Result<Identity> {
    // Cargo can change directory for a manifest or a nested build. Relative
    // rustc arguments and dep-info belong to that invocation's working dir,
    // not the parent Bellows session's launch directory.
    let workspace = env::current_dir()?.canonicalize()?;
    let normalizer = normalizer(&workspace, &invocation.out_dir);
    let root_normalizer = PathNormalizer::new(root_bases(&workspace, &invocation.out_dir));
    let digests = digests::Digests::new(&state_dir(&workspace));
    let compiler = Command::new(&invocation.rustc).arg("-vV").output()?;
    if !compiler.status.success() {
        bail!("rustc -vV failed")
    }
    let normalized_args = normalized_compiler_arguments(invocation, &normalizer);
    let mut components = diagnostics::argument_components(&normalized_args, str::to_owned);
    components.insert(
        "argument order".into(),
        digest_bytes(&serde_json::to_vec(&normalized_args)?),
    );
    components.insert(
        "compiler version changed".into(),
        digest_bytes(&compiler.stdout),
    );
    components.insert(
        "protocol changed".into(),
        digest_bytes(PROTOCOL_VERSION.to_string().as_bytes()),
    );
    // Earlier captures treated every backslash as a Make escape and could
    // omit the real dependency. Never reuse those compiler manifests.
    let dep_info_format = b"rustc-space-escape-v5-structured-dep-info";
    components.insert(
        "dependency parser changed".into(),
        digest_bytes(dep_info_format),
    );
    components.insert(
        "output kind changed".into(),
        digest_bytes(invocation.kind.name().as_bytes()),
    );
    let mut hasher = blake3::Hasher::new();
    hash_field(
        &mut hasher,
        "protocol",
        PROTOCOL_VERSION.to_string().as_bytes(),
    );
    hash_field(&mut hasher, "compiler", &compiler.stdout);
    hash_field(&mut hasher, "dep-info-format", dep_info_format);
    hash_field(
        &mut hasher,
        "output-kind",
        invocation.kind.name().as_bytes(),
    );
    for arg in &normalized_args {
        hash_field(&mut hasher, "arg", arg.as_bytes());
    }
    hash_field(
        &mut hasher,
        "remap",
        b"$CHECKOUT=/bellows/checkout;$WORKSPACE=/bellows/workspace;$TARGET=/bellows/target;$PROFILE=/bellows/profile",
    );
    for (name, value) in relevant_environment(&normalizer) {
        hash_field(&mut hasher, &format!("env:{name}"), value.as_bytes());
        components.insert(
            format!("environment changed: {name}"),
            digest_bytes(value.as_bytes()),
        );
    }
    let mut inputs = invocation.explicit_inputs.clone();
    inputs.extend(resolve_static_libraries(invocation, &workspace)?);
    inputs.sort();
    inputs.dedup();
    for path in inputs {
        let absolute = canonical_compiler_input(&absolute_path(&path, &workspace))
            .with_context(|| format!("canonicalize compiler input {}", path.display()))?;
        let input_digest = digests.file(&absolute)?;
        components.insert(
            format!(
                "explicit input changed: {}",
                normalizer.normalize(&absolute.to_string_lossy())
            ),
            input_digest.clone(),
        );
        hash_field(
            &mut hasher,
            &format!(
                "input:{}",
                normalizer.normalize(&absolute.to_string_lossy())
            ),
            input_digest.as_bytes(),
        );
    }
    let static_key = hasher.finalize().to_hex().to_string();
    let source = invocation
        .explicit_inputs
        .first()
        .map(|p| normalizer.normalize(&absolute_path(p, &workspace).to_string_lossy()))
        .unwrap_or_default();
    let mut pins = vec![(
        format!("{PIN_PREFIX}$WORKSPACE"),
        workspace.to_string_lossy().into_owned(),
    )];
    if let Some(target) = target_root(&workspace, &invocation.out_dir) {
        pins.push((
            format!("{PIN_PREFIX}$TARGET"),
            canonical_base(target).to_string_lossy().into_owned(),
        ));
    }
    if let Some(checkout) = root_normalizer.spellings("$CHECKOUT").first() {
        pins.push((format!("{PIN_PREFIX}$CHECKOUT"), checkout.clone()));
    }
    let virtual_env = stable_empty_directories(invocation, &root_normalizer);
    Ok(Identity {
        virtual_env,
        fingerprint: diagnostics::Fingerprint {
            key: static_key.clone(),
            components,
        },
        diagnostic_group: format!("{}:{source}", invocation.crate_name),
        static_key,
        normalizer,
        root_normalizer,
        workspace,
        digests,
        pins,
    })
}

/// A procedural macro runs only inside rustc, so a checkout directory it
/// bakes in (wit-bindgen bakes its build script's `OUT_DIR` as
/// `DEBUG_OUTPUT_DIR`) is only used while compiling its consumers. When such
/// a directory is empty, a machine-wide empty directory at a checkout-
/// independent path is observably equivalent, and it makes the macro — and
/// through its crate hash every consumer — identical across worktrees.
/// Cargo's own variables and non-empty directories are never substituted.
fn stable_empty_directories(
    invocation: &Invocation,
    root_normalizer: &PathNormalizer,
) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    if !invocation.proc_macro_crate {
        return overrides;
    }
    let Some(root) = user_cache_root().map(|root| root.join("stable-dirs-v1")) else {
        return overrides;
    };
    for (name, value) in env::vars() {
        if name.starts_with("CARGO_") {
            continue;
        }
        let normalized = root_normalizer.normalize(&value);
        if !(normalized.starts_with("$TARGET") || normalized.starts_with("$PROFILE")) {
            continue;
        }
        let empty = fs::read_dir(&value).is_ok_and(|mut entries| entries.next().is_none());
        if !empty {
            continue;
        }
        let stable = root.join(&digest_bytes(normalized.as_bytes())[..32]);
        if fs::create_dir_all(&stable).is_ok() {
            overrides.insert(name, stable.to_string_lossy().into_owned());
        }
    }
    overrides
}

/// The platform user cache, independent of per-workspace `BELLOWS_STATE_DIR`.
fn user_cache_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(root).join("bellows"));
    }
    if let Some(root) = env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(root).join("Bellows"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/bellows"))
}

/// Files a library compile bundles for `-l static=NAME`: every candidate in
/// every `-L native=` directory, so a newly shadowing archive also misses.
fn resolve_static_libraries(invocation: &Invocation, workspace: &Path) -> Result<Vec<PathBuf>> {
    if invocation.kind != OutputKind::Library || invocation.static_libraries.is_empty() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for library in &invocation.static_libraries {
        let names = if library.verbatim {
            vec![library.name.clone()]
        } else if invocation.naming.msvc {
            vec![format!("{}.lib", library.name)]
        } else {
            vec![
                format!("lib{}.a", library.name),
                format!("{}.lib", library.name),
            ]
        };
        let before = found.len();
        for dir in &invocation.native_search {
            for name in &names {
                let path = absolute_path(&dir.join(name), workspace);
                if path.is_file() {
                    found.push(path);
                }
            }
        }
        if found.len() == before {
            bail!(
                "static native library {} was not found in the -L native directories",
                library.name
            )
        }
    }
    Ok(found)
}

fn relevant_environment(normalizer: &PathNormalizer) -> BTreeMap<String, String> {
    env::vars()
        .filter(|(name, _)| is_relevant_environment_name(name))
        .map(|(name, value)| (name, normalizer.normalize(&value)))
        .collect()
}

fn is_relevant_environment_name(name: &str) -> bool {
    // Scheduling, terminal and rustup-proxy bookkeeping never reach rustc's
    // output. A crate that reads one with env!/option_env! is still tracked
    // exactly through dep-info.
    const EXCLUDED_CARGO_CONTROL: &[&str] = &[
        "CARGO_BUILD_JOBS",
        "CARGO_INCREMENTAL",
        "CARGO_MAKEFLAGS",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
        "CARGO_TERM_COLOR",
        "CARGO_TERM_PROGRESS_WHEN",
        "CARGO_TERM_PROGRESS_WIDTH",
        "CARGO_TERM_QUIET",
        "CARGO_TERM_VERBOSE",
        "CLIPPY_TERMINAL_WIDTH",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_TOOLCHAIN_SOURCE",
        "RUST_BACKTRACE",
        "RUST_LOG",
        "RUST_MIN_STACK",
        "RUST_RECURSION_COUNT",
        "RUST_TEST_THREADS",
    ];
    const EXACT: &[&str] = &[
        "AR",
        "CC",
        "CXX",
        "DEBUG",
        "DYLD_LIBRARY_PATH",
        "HOME",
        "HOST",
        "INCLUDE",
        "LANG",
        "LC_ALL",
        "LD_LIBRARY_PATH",
        "LIB",
        "LIBPATH",
        "LIBRARY_PATH",
        "MACOSX_DEPLOYMENT_TARGET",
        "NM",
        "OBJCOPY",
        "OPT_LEVEL",
        "OUT_DIR",
        "PATH",
        "PROFILE",
        "RANLIB",
        "SDKROOT",
        "SOURCE_DATE_EPOCH",
        "STRIP",
        "TARGET",
        "TZ",
        "UCRTVersion",
        "UniversalCRTSdkDir",
        "VCINSTALLDIR",
        "VCToolsInstallDir",
        "VCToolsVersion",
        "VSINSTALLDIR",
        "WindowsSdkDir",
        "WindowsSDKVersion",
    ];
    const PREFIXES: &[&str] = &[
        "AR_", "CARGO_", "CC_", "CFLAGS", "CLIPPY_", "CPPFLAGS", "CXX_", "CXXFLAGS", "DEP_",
        "LDFLAGS", "RUST",
    ];
    const BELLOWS_CONTROL: &[&str] = &[
        "BELLOWS_AUTH_TOKEN",
        "BELLOWS_READ_ONLY",
        "BELLOWS_DEMO_COMPILE_DELAY_MS",
        "BELLOWS_EVENT_LOG",
        "BELLOWS_LOCAL_ONLY",
        "BELLOWS_MAX_WAIT_MS",
        "BELLOWS_SERVER",
        "BELLOWS_STATE_DIR",
    ];
    !BELLOWS_CONTROL.contains(&name)
        && !EXCLUDED_CARGO_CONTROL.contains(&name)
        && (EXACT.contains(&name) || PREFIXES.iter().any(|prefix| name.starts_with(prefix)))
}

fn hash_field(hasher: &mut blake3::Hasher, name: &str, bytes: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn validate_candidate(
    candidate: &ActionCandidate,
    identity: &Identity,
) -> std::result::Result<(), String> {
    validate_candidate_manifest(candidate).map_err(|error| error.to_string())?;
    // Environment first: it is free, and a checkout pin rejects before any
    // large link input is read. Named variables before pins, so a miss names
    // the variable whose embedded path differs.
    let (pins, named): (Vec<_>, Vec<_>) = candidate
        .env
        .iter()
        .partition(|input| input.name.starts_with(PIN_PREFIX));
    for input in named.into_iter().chain(pins) {
        let actual = if input.name.starts_with(PIN_PREFIX) {
            match identity.pin_value(&input.name) {
                Some(value) => EnvInput::capture(&input.name, Some(value)),
                None => return Err(format!("environment changed: {}", input.name)),
            }
        } else {
            let value = identity.env_value(&input.name);
            if input.normalized {
                match value {
                    Some(value) => EnvInput::capture_normalized(
                        &input.name,
                        &identity.root_normalizer.normalize(&value),
                    ),
                    None => EnvInput::capture(&input.name, None),
                }
            } else {
                EnvInput::capture(&input.name, value.as_deref())
            }
        };
        if actual.value_digest != input.value_digest {
            return Err(if input.name.starts_with(PIN_PREFIX) {
                format!(
                    "environment changed: {} (the cached output embeds another checkout's path)",
                    input.name
                )
            } else {
                format!("environment changed: {}", input.name)
            });
        }
    }
    for input in &candidate.files {
        let localized = identity.normalizer.localize(&input.path);
        let path = absolute_path(Path::new(&localized), &identity.workspace);
        let actual = identity
            .digests
            .file(&path)
            .map_err(|_| format!("input disappeared: {}", input.path))?;
        if actual != input.digest {
            return Err(format!("input changed: {}", input.path));
        }
    }
    for input in &candidate.host_files {
        let actual = identity
            .digests
            .file(Path::new(&input.path))
            .map_err(|_| format!("input disappeared: {}", input.path))?;
        if actual != input.digest {
            return Err(format!("input changed: {}", input.path));
        }
    }
    Ok(())
}

fn explain_candidates(identity: &Identity, index: &CandidateIndex) -> String {
    if index.candidates.is_empty() {
        return "no candidate for this static identity".into();
    }
    index
        .candidates
        .iter()
        .filter_map(|candidate| validate_candidate(candidate, identity).err())
        .next()
        .unwrap_or_else(|| "candidate artifacts unavailable or restore failed; see corrupt event for the blob/path error".into())
}

fn try_candidates(
    remote: &Remote,
    l1: Option<&Store>,
    invocation: &Invocation,
    identity: &Identity,
    index: &CandidateIndex,
) -> Result<Option<ExitStatus>> {
    for candidate in &index.candidates {
        if let Err(reason) = validate_candidate(candidate, identity) {
            record_event(
                "candidate_rejected",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &reason,
            );
            continue;
        }
        match restore(remote, l1, invocation, identity, candidate) {
            Ok(()) => {
                if let Some(store) = l1 {
                    let _ = store.put_candidate(candidate.clone(), 8);
                }
                record_event(
                    "hit",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&candidate.action_key),
                    &hit_detail("restored remote compiler result", invocation, candidate),
                );
                return Ok(Some(success_status()));
            }
            Err(error) => record_event(
                "corrupt",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("candidate rejected during restore: {error:#}"),
            ),
        }
    }
    Ok(None)
}

fn try_l1_candidates(
    store: &Store,
    invocation: &Invocation,
    identity: &Identity,
    index: &CandidateIndex,
) -> Result<Option<ExitStatus>> {
    for candidate in &index.candidates {
        if let Err(reason) = validate_candidate(candidate, identity) {
            record_event(
                "candidate_rejected",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("local cache: {reason}"),
            );
            continue;
        }
        match restore_l1(store, invocation, identity, candidate) {
            Ok(()) => {
                record_event(
                    "l1_hit",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&candidate.action_key),
                    &hit_detail(
                        "restored runner-local compiler result",
                        invocation,
                        candidate,
                    ),
                );
                return Ok(Some(success_status()));
            }
            Err(error) => record_event(
                "corrupt",
                &invocation.crate_name,
                Some(&identity.static_key),
                Some(&candidate.action_key),
                &format!("L1 candidate rejected: {error:#}"),
            ),
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn success_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn success_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

fn hit_detail(prefix: &str, invocation: &Invocation, candidate: &ActionCandidate) -> String {
    let mut detail = format!("{prefix} ({} output", invocation.kind.name());
    if !candidate.proc_macros.is_empty() {
        detail.push_str(&format!(
            "; proc macros: {}",
            candidate.proc_macros.join(", ")
        ));
    }
    detail.push(')');
    detail
}

fn restore(
    remote: &Remote,
    l1: Option<&Store>,
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
) -> Result<()> {
    restore_with(invocation, identity, candidate, |digest| {
        let bytes = remote.blob(digest)?;
        if let Some(store) = l1 {
            let _ = store.put_blob(digest, &bytes);
        }
        Ok(bytes)
    })
}

fn restore_l1(
    store: &Store,
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
) -> Result<()> {
    restore_with(invocation, identity, candidate, |digest| {
        store.read_blob(digest)
    })
    .context("L1")
}

/// Verify the manifest describes exactly this unit's outputs, fetch and verify
/// every blob, then write each output atomically with its mode.
fn restore_with(
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
    fetch: impl Fn(&str) -> Result<Vec<u8>>,
) -> Result<()> {
    validate_candidate_manifest(candidate)?;
    let supplied = candidate
        .artifacts
        .iter()
        .map(|artifact| artifact.file_name.as_str())
        .collect::<BTreeSet<_>>();
    for expected in &invocation.expected_names {
        let rmeta_may_be_folded_into_rlib = expected.ends_with(".rmeta")
            && invocation
                .expected_names
                .iter()
                .any(|name| name.ends_with(".rlib"));
        if !supplied.contains(expected.as_str()) && !rmeta_may_be_folded_into_rlib {
            bail!("manifest is missing expected output {expected}")
        }
    }
    let mut downloaded = Vec::with_capacity(candidate.artifacts.len());
    for artifact in &candidate.artifacts {
        let allowed = match invocation.kind {
            OutputKind::Library => invocation.expected_names.contains(&artifact.file_name),
            OutputKind::Linked => invocation.owns(&artifact.file_name),
        };
        if !allowed {
            bail!("manifest contains unexpected output {}", artifact.file_name)
        }
        downloaded.push((artifact, fetch(&artifact.digest)?));
    }
    let stdout_blob = fetch(&candidate.stdout.digest)?;
    let stderr_blob = fetch(&candidate.stderr.digest)?;
    if stdout_blob.len() as u64 != candidate.stdout.len
        || stderr_blob.len() as u64 != candidate.stderr.len
    {
        bail!("compiler stream length does not match manifest")
    }
    for (artifact, stored) in downloaded {
        let bytes = if artifact.file_name.ends_with(".d") {
            transform_dep_info(&stored, &identity.normalizer, true)
        } else {
            stored
        };
        let destination = invocation.out_dir.join(&artifact.file_name);
        install_output(&destination, &bytes)?;
        set_file_executable(&destination, artifact.executable)?;
    }
    let stdout = transform_compiler_stream(&stdout_blob, &identity.normalizer, true);
    let stderr = transform_compiler_stream(&stderr_blob, &identity.normalizer, true);
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

/// Replace an output file. Windows refuses to replace an executable image
/// that is running (a test binary still executing, or a loaded DLL) but
/// allows renaming it; move it aside and sweep leftovers on later restores.
fn install_output(destination: &Path, bytes: &[u8]) -> Result<()> {
    match atomic_write(destination, bytes) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error) if destination.exists() => {
            let parent = destination.parent().context("output has no parent")?;
            let aside = parent.join(format!(
                ".bellows-replaced-{}-{}-{}",
                std::process::id(),
                now_ms(),
                destination
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("output")
            ));
            fs::rename(destination, &aside).with_context(|| {
                format!("replace in-use output {}: {error:#}", destination.display())
            })?;
            atomic_write(destination, bytes)?;
            sweep_replaced(parent);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn sweep_replaced(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".bellows-replaced-"))
            {
                // Still-running images stay locked and are retried later.
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

struct Captured {
    candidate: ActionCandidate,
    blobs: BTreeMap<String, Vec<u8>>,
    /// The outputs embed a checkout root; the candidate carries pin inputs.
    pinned: bool,
}

fn compile_and_capture(
    invocation: &Invocation,
    identity: &Identity,
) -> Result<(ExitStatus, Option<Captured>)> {
    if let Some(delay) = env::var("BELLOWS_DEMO_COMPILE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        thread::sleep(Duration::from_millis(delay.min(10_000)));
    }
    // Incremental policy: publish only a from-scratch incremental compile.
    // A compile that reused an existing session could carry state from
    // another source version; its result stays in this checkout.
    //
    // Cargo shares one incremental directory between all units, where a
    // crate's lib, test harness and binaries all create `{crate}-*` sessions
    // concurrently. Each unit gets its own subdirectory instead, named so
    // `cargo clean -p` still removes it, which makes "from scratch" exact.
    let unit_incremental = invocation.incremental.as_deref().map(|dir| {
        absolute_path(dir, &identity.workspace).join(format!(
            "{}-bellows{}",
            invocation.crate_name,
            invocation
                .unit
                .strip_prefix(&invocation.crate_name)
                .unwrap_or_default()
        ))
    });
    let reused_session = unit_incremental
        .as_deref()
        .is_some_and(|dir| fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some()));
    let mut args = Vec::with_capacity(invocation.args.len() + 6);
    let mut rewrite_next = false;
    for arg in &invocation.args {
        let replacement = unit_incremental
            .as_ref()
            .map(|dir| format!("incremental={}", dir.display()));
        if std::mem::take(&mut rewrite_next) && arg.starts_with("incremental=") {
            args.push(replacement.unwrap_or_else(|| arg.clone()));
        } else if arg.starts_with("-Cincremental=") {
            args.push(format!(
                "-C{}",
                replacement.unwrap_or_else(|| arg[2..].to_owned())
            ));
        } else {
            rewrite_next = arg == "-C";
            args.push(arg.clone());
        }
    }
    // Remap every spelling of each root: on Windows the canonical workspace
    // is a verbatim `\\?\C:\…` path that never prefixes rustc's own paths.
    // rustc applies the last matching mapping, so the target (often inside
    // the workspace) comes last.
    for (token, virtual_root) in [
        ("$CHECKOUT", "/bellows/checkout"),
        ("$WORKSPACE", "/bellows/workspace"),
        ("$TARGET", "/bellows/target"),
        ("$PROFILE", "/bellows/profile"),
    ] {
        for spelling in identity.root_normalizer.spellings(token) {
            if spelling.starts_with(r"\\?\") {
                continue;
            }
            args.push("--remap-path-prefix".into());
            args.push(format!("{spelling}={virtual_root}"));
        }
    }
    // rustc writes the exact linker command to a file, leaving the stdout
    // Cargo reads untouched.
    let link_record = if invocation.kind == OutputKind::Linked {
        fs::create_dir_all(&invocation.out_dir).context("create output directory")?;
        let record = tempfile::Builder::new()
            .prefix(".bellows-link-")
            .tempfile_in(&invocation.out_dir)
            .context("create link-args record")?
            .into_temp_path();
        args.push(format!("--print=link-args={}", record.display()));
        Some(record)
    } else {
        None
    };
    let started = std::time::SystemTime::now();
    let mut child = Command::new(&invocation.rustc)
        .args(&args)
        .envs(&identity.virtual_env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start rustc")?;
    let stdout = child.stdout.take().context("capture rustc stdout")?;
    let stderr = child.stderr.take().context("capture rustc stderr")?;
    let stdout_thread = thread::spawn(move || tee(stdout, std::io::stdout()));
    let stderr_thread = thread::spawn(move || tee(stderr, std::io::stderr()));
    let status = child.wait()?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow!("stdout relay panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow!("stderr relay panicked"))??;
    if !status.success() {
        return Ok((status, None));
    }
    let link_command = match &link_record {
        Some(record) => Some(fs::read_to_string(record).context("read link-args record")?),
        None => None,
    };
    drop(link_record);
    let result = if reused_session {
        Err(anyhow::Error::new(NotStored(
            "incremental compile reused an existing session; result stays in this checkout".into(),
        )))
    } else {
        capture_outputs(
            invocation,
            identity,
            stdout,
            stderr,
            started,
            link_command.as_deref(),
        )
    };
    let captured = match result {
        Ok(captured) => Some(captured),
        Err(error) => {
            if let Some(reason) = error.downcast_ref::<NotStored>() {
                record_event(
                    "not_stored",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("compiled but not stored: {reason}"),
                );
            } else {
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("capture skipped after successful rustc: {error:#}"),
                );
            }
            None
        }
    };
    Ok((status, captured))
}

fn not_stored(reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(NotStored(reason.into()))
}

// rustc's JSON diagnostics/artifact notifications contain escaped strings.
// Replacing raw bytes can inject unescaped Windows backslashes into JSON.
fn transform_compiler_stream(bytes: &[u8], normalizer: &PathNormalizer, localize: bool) -> Vec<u8> {
    fn visit(value: &mut serde_json::Value, normalizer: &PathNormalizer, localize: bool) {
        match value {
            serde_json::Value::String(text) => {
                *text = if localize {
                    normalizer.localize(text)
                } else {
                    normalizer.normalize(text)
                };
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, normalizer, localize);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    visit(value, normalizer, localize);
                }
            }
            _ => {}
        }
    }
    let mut result = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if let Ok(mut value @ serde_json::Value::Object(_)) = serde_json::from_slice(line) {
            visit(&mut value, normalizer, localize);
            result.extend(serde_json::to_vec(&value).expect("serialize compiler JSON"));
            if line.ends_with(b"\r\n") {
                result.extend_from_slice(b"\r\n");
            } else if line.ends_with(b"\n") {
                result.push(b'\n');
            }
        } else {
            result.extend(if localize {
                normalizer.localize_bytes(line)
            } else {
                normalizer.normalize_bytes(line)
            });
        }
    }
    result
}

fn transform_dep_info(bytes: &[u8], normalizer: &PathNormalizer, localize: bool) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    bellows_core::rewrite_dep_info(text, |path| {
        if localize {
            normalizer.localize(path)
        } else {
            normalizer.normalize(path)
        }
    })
    .into_bytes()
}

/// Outputs of a linked unit: every regular file named for this unit that
/// rustc wrote during the compile. The predicted primary outputs must be
/// among them; directory outputs (`.dSYM`) are not modeled.
fn discover_linked_outputs(
    invocation: &Invocation,
    started: std::time::SystemTime,
) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    // Allow for filesystems whose timestamps are coarser than the clock.
    let threshold = started - Duration::from_secs(2);
    for entry in fs::read_dir(&invocation.out_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !invocation.owns(&name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        let fresh = metadata.modified().is_ok_and(|time| time >= threshold);
        if !fresh {
            continue;
        }
        if !metadata.is_file() {
            return Err(not_stored(format!(
                "rustc produced a non-file output {name} (split debuginfo directories are not modeled)"
            )));
        }
        names.insert(name);
    }
    for expected in &invocation.expected_names {
        if !names.contains(expected) {
            bail!("rustc succeeded but expected output {expected} is absent")
        }
    }
    Ok(names)
}

fn capture_outputs(
    invocation: &Invocation,
    identity: &Identity,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    started: std::time::SystemTime,
    link_command: Option<&str>,
) -> Result<Captured> {
    let names = match invocation.kind {
        OutputKind::Library => invocation.expected_names.clone(),
        OutputKind::Linked => discover_linked_outputs(invocation, started)?,
    };
    // Each root is scanned separately: a registry crate's "workspace" is its
    // registry directory, identical in every checkout, and pinning it must
    // not also pin the per-checkout target directory.
    let scanners = ["$CHECKOUT", "$WORKSPACE", "$TARGET"].map(|token| {
        (
            token,
            leak::Scanner::new(&identity.root_normalizer.spellings(token)),
        )
    });
    let mut leaked = BTreeSet::<&str>::new();
    let mut blobs = BTreeMap::new();
    let mut artifacts = Vec::new();
    let mut dep_text = None;
    for name in &names {
        let path = invocation.out_dir.join(name);
        if !path.is_file() {
            if name.ends_with(".rmeta") && names.iter().any(|n| n.ends_with(".rlib")) {
                continue;
            }
            bail!(
                "rustc succeeded but expected output {} is absent",
                path.display()
            )
        }
        let raw = fs::read(&path)?;
        let stored = if name.ends_with(".d") {
            let normalized = transform_dep_info(&raw, &identity.normalizer, false);
            dep_text = Some(String::from_utf8(raw).context("dep-info is not UTF-8")?);
            normalized
        } else {
            if name.ends_with(".rlib") {
                check_bundled_members(invocation, &raw, &identity.workspace)?;
            }
            // Linker-written companions (program databases, export files,
            // import libraries) record the linker's working directory and
            // module paths; see `leaks_in_program_database`.
            let lower = name.to_ascii_lowercase();
            let program_database = invocation.kind == OutputKind::Linked
                && [".pdb", ".exp", ".lib"]
                    .iter()
                    .any(|extension| lower.ends_with(extension));
            for (token, scanner) in &scanners {
                let leaks = if program_database {
                    scanner.leaks_in_program_database(&raw)
                } else {
                    scanner.leaks(&raw)
                };
                if !leaked.contains(token) && leaks {
                    leaked.insert(token);
                }
            }
            raw
        };
        let digest = digest_bytes(&stored);
        let executable = is_executable(&path)?;
        blobs.insert(digest.clone(), stored);
        artifacts.push(Artifact {
            file_name: name.clone(),
            digest,
            executable,
        });
    }
    let dep_text = dep_text.context("rustc did not produce dep-info")?;
    let (dep_files, dep_env) = parse_dep_info(&dep_text);
    let mut file_paths = invocation.explicit_inputs.clone();
    file_paths.extend(dep_files.into_iter().map(PathBuf::from));
    file_paths.extend(resolve_static_libraries(invocation, &identity.workspace)?);
    let mut files = Vec::new();
    let mut host_files = Vec::new();
    let mut record = |absolute: &Path, host: bool| -> Result<()> {
        let digest = identity.digests.file(absolute)?;
        let normalized = identity.normalizer.normalize(&absolute.to_string_lossy());
        if bellows_core::validate_normalized_input_path(&normalized).is_ok() {
            files.push(FileInput {
                path: normalized,
                digest,
            });
        } else if host {
            host_files.push(FileInput {
                path: absolute.to_string_lossy().into_owned(),
                digest,
            });
        } else {
            bail!(
                "compiler input outside every normalized root: {}",
                absolute.display()
            )
        }
        Ok(())
    };
    file_paths.sort();
    file_paths.dedup();
    for path in file_paths {
        let absolute = absolute_path(&path, &identity.workspace);
        if !absolute.is_file() {
            bail!(
                "rustc dependency disappeared before capture: {}",
                absolute.display()
            )
        }
        let absolute = canonical_compiler_input(&absolute)
            .with_context(|| format!("canonicalize rustc dependency {}", absolute.display()))?;
        record(&absolute, true)?;
    }
    if let Some(text) = link_command {
        let command = link::parse_link_command(text).context("parse linker command")?;
        let sysroot = sysroot(&invocation.rustc)?;
        let outputs = |path: &Path| {
            path.parent() == Some(invocation.out_dir.as_path())
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| invocation.owns(name))
        };
        let inputs = link::link_inputs(
            &command,
            &link::LinkContext {
                cwd: &identity.workspace,
                msvc: invocation.naming.msvc,
                outputs: &outputs,
            },
        )?;
        let sysroot = sysroot.canonicalize().unwrap_or(sysroot);
        for path in inputs.files {
            // Toolchain files are identified by `rustc -vV`; symlinked system
            // libraries (libfoo.so -> libfoo.so.1) are recorded by content.
            let absolute = absolute_path(&path, &identity.workspace);
            let resolved = absolute.canonicalize().unwrap_or(absolute);
            if resolved.starts_with(&sysroot) {
                continue;
            }
            record(&resolved, true)?;
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    host_files.sort_by(|a, b| a.path.cmp(&b.path));
    host_files.dedup_by(|a, b| a.path == b.path);
    let mut env_inputs = dep_env
        .into_iter()
        .map(|(name, _)| {
            let value = identity.env_value(&name);
            match value {
                Some(value) => {
                    let normalized = identity.root_normalizer.normalize(&value);
                    // Compare normalized only when no output embeds a root
                    // this value names (the profile directory is inside the
                    // target directory).
                    let names_leaked_root = leaked.iter().any(|token| {
                        normalized.contains(token)
                            || (*token == "$TARGET" && normalized.contains("$PROFILE"))
                    });
                    if normalized != value && !names_leaked_root {
                        EnvInput::capture_normalized(&name, &normalized)
                    } else {
                        EnvInput::capture(&name, Some(&value))
                    }
                }
                None => EnvInput::capture(&name, None),
            }
        })
        .collect::<Vec<_>>();
    let pinned = !leaked.is_empty();
    env_inputs.extend(
        identity
            .pins
            .iter()
            .filter(|(name, _)| leaked.iter().any(|token| name.ends_with(token)))
            .map(|(name, value)| EnvInput::capture(name, Some(value))),
    );
    env_inputs.sort_by(|a, b| a.name.cmp(&b.name));
    env_inputs.dedup_by(|a, b| a.name == b.name);
    let normalized_stdout = transform_compiler_stream(&stdout, &identity.normalizer, false);
    let normalized_stderr = transform_compiler_stream(&stderr, &identity.normalizer, false);
    let stdout_digest = digest_bytes(&normalized_stdout);
    let stderr_digest = digest_bytes(&normalized_stderr);
    let stdout_len = normalized_stdout.len() as u64;
    let stderr_len = normalized_stderr.len() as u64;
    blobs.insert(stdout_digest.clone(), normalized_stdout);
    blobs.insert(stderr_digest.clone(), normalized_stderr);
    let action_key = compiler_action_key(&identity.static_key, &files, &host_files, &env_inputs);
    let candidate = ActionCandidate {
        protocol: PROTOCOL_VERSION,
        static_key: identity.static_key.clone(),
        action_key,
        crate_name: invocation.crate_name.clone(),
        created_ms: now_ms(),
        files,
        host_files,
        env: env_inputs,
        artifacts,
        stdout: StreamArtifact {
            digest: stdout_digest,
            len: stdout_len,
        },
        stderr: StreamArtifact {
            digest: stderr_digest,
            len: stderr_len,
        },
        proc_macros: invocation.proc_macros.clone(),
    };
    validate_candidate_manifest(&candidate).map_err(|error| {
        if error.to_string().contains("cardinality") {
            not_stored(format!("{error:#}"))
        } else {
            error
        }
    })?;
    Ok(Captured {
        candidate,
        blobs,
        pinned,
    })
}

fn sysroot(rustc: &Path) -> Result<PathBuf> {
    let output = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .context("run rustc --print sysroot")?;
    if !output.status.success() {
        bail!("rustc --print sysroot failed")
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

/// rustc bundles `-l static` archives, and archives named by `#[link(kind =
/// "static")]` in source, into the rlib. Only archives resolved from `-l`
/// are inputs; any other native member means an unmodeled bundled library.
fn check_bundled_members(invocation: &Invocation, rlib: &[u8], workspace: &Path) -> Result<()> {
    let members = archive::member_names(rlib).context("read rlib members")?;
    let foreign = members
        .into_iter()
        .filter(|name| name != "lib.rmeta" && !name.ends_with(".rcgu.o") && !name.starts_with('/'))
        .collect::<BTreeSet<_>>();
    if foreign.is_empty() {
        return Ok(());
    }
    let mut declared = BTreeSet::new();
    for library in resolve_static_libraries(invocation, workspace).unwrap_or_default() {
        if let Ok(bytes) = fs::read(&library)
            && let Ok(names) = archive::member_names(&bytes)
        {
            declared.extend(names);
        }
    }
    if let Some(unknown) = foreign.iter().find(|name| !declared.contains(*name)) {
        return Err(not_stored(format!(
            "rlib bundles native member {unknown} that no -l static library supplies (#[link] in source)"
        )));
    }
    Ok(())
}

fn tee(mut reader: impl Read, mut writer: impl Write) -> Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        writer.flush()?;
        captured.extend_from_slice(&buffer[..count]);
    }
    Ok(captured)
}

fn publish(remote: &Remote, captured: Captured) -> Result<String> {
    validate_candidate_manifest(&captured.candidate).context("validate captured candidate")?;
    for (digest, bytes) in captured.blobs {
        remote.put_blob(&digest, bytes)?;
    }
    let action_key = captured.candidate.action_key.clone();
    remote.put_candidate(&captured.candidate)?;
    Ok(action_key)
}

fn cache_captured(store: &Store, captured: &Captured) -> Result<()> {
    for (digest, bytes) in &captured.blobs {
        store.put_blob(digest, bytes)?;
    }
    store.put_candidate(captured.candidate.clone(), 8)?;
    Ok(())
}

fn is_executable(path: &Path) -> Result<bool> {
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

fn state_dir(workspace: &Path) -> PathBuf {
    env::var_os("BELLOWS_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join(".bellows"))
}

fn event_log_path() -> PathBuf {
    env::var_os("BELLOWS_EVENT_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let workspace = env::var_os("BELLOWS_WORKSPACE")
                .map(PathBuf::from)
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            state_dir(&workspace).join("events.jsonl")
        })
}

fn record_event(
    kind: &str,
    crate_name: &str,
    static_key: Option<&str>,
    action_key: Option<&str>,
    detail: &str,
) {
    record_event_duration(kind, crate_name, static_key, action_key, detail, None);
}

fn record_event_duration(
    kind: &str,
    crate_name: &str,
    static_key: Option<&str>,
    action_key: Option<&str>,
    detail: &str,
    duration_ms: Option<u64>,
) {
    let event = Event {
        timestamp_ms: now_ms(),
        kind: kind.into(),
        crate_name: crate_name.into(),
        static_key: static_key.map(str::to_owned),
        action_key: action_key.map(str::to_owned),
        detail: detail.into(),
        reason: Some(diagnostics::reason_code(kind, detail).into()),
        session_id: env::var("BELLOWS_SESSION_ID").ok(),
        workspace: env::var("BELLOWS_WORKSPACE").ok(),
        duration_ms,
    };
    if let Err(error) = diagnostics::append(&event_log_path(), &event) {
        eprintln!("Bellows diagnostics unavailable: {error:#}");
    }
    if terminal::Output::from_env().prints(kind) {
        eprintln!(
            "{}",
            terminal::status(terminal::stderr_color(), kind, crate_name, detail)
        );
    }
}

fn doctor(server: &str, token: Option<&str>) -> Result<()> {
    let remote = Remote::new(server, token.map(str::to_owned))?;
    let health = remote.health().context("connect to bellowsd")?;
    validate_protocol(health.protocol)?;
    let rustc = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("run rustc -vV")?;
    let compiler = String::from_utf8_lossy(&rustc.stdout);
    let color = terminal::stdout_color();
    println!("{}", terminal::heading(color, "Bellows · Doctor"));
    println!(
        "{}",
        terminal::success(
            color,
            "bellowsd",
            &format!("v{} · {server}", health.version)
        )
    );
    println!(
        "{}",
        terminal::success(color, "protocol", &health.protocol.to_string())
    );
    println!(
        "{}",
        terminal::success(
            color,
            "compiler",
            compiler.lines().next().unwrap_or("rustc")
        )
    );
    println!(
        "{}",
        terminal::success(color, "fallback", "official rustc enabled")
    );
    let l1 = if env::var("BELLOWS_L1").as_deref() == Ok("0") {
        "off (BELLOWS_L1=0)"
    } else {
        "on"
    };
    println!("{}", terminal::success(color, "local cache", l1));
    println!(
        "{}",
        terminal::success(
            color,
            "output",
            &format!("{} (BELLOWS_OUTPUT)", terminal::Output::from_env().name())
        )
    );
    Ok(())
}

fn validate_protocol(server_protocol: u32) -> Result<()> {
    if server_protocol != PROTOCOL_VERSION {
        bail!(
            "protocol mismatch: client {PROTOCOL_VERSION}, server {}",
            server_protocol
        )
    }
    Ok(())
}

#[derive(Serialize)]
struct CombinedStats {
    remote: ServerStats,
    events: BTreeMap<String, u64>,
    diagnostics: diagnostics::Summary,
}

fn show_stats(
    server: &str,
    token: Option<&str>,
    selection: &diagnostics::Selection,
    json: bool,
) -> Result<()> {
    let remote = Remote::new(server, token.map(str::to_owned))?.stats()?;
    let diagnostics = diagnostics::summarize(&diagnostics::read_selected(selection)?);
    let events = diagnostics.decisions.clone();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CombinedStats {
                remote,
                events,
                diagnostics
            })?
        );
    } else {
        let color = terminal::stdout_color();
        println!("{}", terminal::heading(color, "Bellows · Cache statistics"));
        println!("{}", terminal::section(color, "Remote cache"));
        println!(
            "{}",
            terminal::key_value(color, "compiler actions", remote.candidates)
        );
        println!(
            "{}",
            terminal::key_value(color, "declared actions", remote.declared_actions)
        );
        println!(
            "{}",
            terminal::key_value(color, "archives", remote.archives)
        );
        println!("{}", terminal::key_value(color, "blobs", remote.blobs));
        println!(
            "{}",
            terminal::key_value(color, "stored", human_bytes(remote.blob_bytes))
        );
        println!(
            "{}",
            terminal::key_value(color, "active leases", remote.active_leases)
        );
        println!("{}", terminal::section(color, "This workspace"));
        diagnostics::print_grouped(&diagnostics, color);
        diagnostics::print_details(&diagnostics, 6);
    }
    Ok(())
}

fn show_local_stats(selection: &diagnostics::Selection, json: bool) -> Result<()> {
    let state = local_state_dir(selection.cache_dir.as_deref())?;
    let store = local_store(Some(&state), true)?;
    let remote = collect_local_stats(&store)?;
    let diagnostics = diagnostics::summarize(&diagnostics::read_selected(selection)?);
    let events = diagnostics.decisions.clone();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CombinedStats {
                remote,
                events,
                diagnostics
            })?
        );
        return Ok(());
    }
    let color = terminal::stdout_color();
    println!("{}", terminal::heading(color, "Bellows · Local cache"));
    println!(
        "{}",
        terminal::key_value(color, "path", store.root().display())
    );
    println!(
        "{}",
        terminal::key_value(color, "compiler actions", remote.candidates)
    );
    println!(
        "{}",
        terminal::key_value(color, "declared actions", remote.declared_actions)
    );
    println!("{}", terminal::key_value(color, "blobs", remote.blobs));
    println!(
        "{}",
        terminal::key_value(color, "stored", human_bytes(remote.blob_bytes))
    );
    println!(
        "{}",
        terminal::section(color, "Selected decisions (retained log)")
    );
    diagnostics::print_grouped(&diagnostics, color);
    diagnostics::print_details(&diagnostics, 6);
    Ok(())
}

fn collect_local_stats(store: &Store) -> Result<ServerStats> {
    let (blobs, blob_bytes) = count_store_files(&store.root().join("blobs"), false)?;
    let (action_indexes, _) = count_store_files(&store.root().join("actions"), true)?;
    let (declared_actions, _) = count_store_files(&store.root().join("declared"), true)?;
    let (archives, _) = count_store_files(&store.root().join("archives"), true)?;
    let mut candidates = 0;
    visit_store_files(&store.root().join("actions"), &mut |path| {
        let index: CandidateIndex = serde_json::from_slice(&fs::read(path)?)?;
        candidates += index.candidates.len() as u64;
        Ok(())
    })?;
    Ok(ServerStats {
        blobs,
        blob_bytes,
        action_indexes,
        candidates,
        active_leases: 0,
        declared_actions,
        archives,
    })
}

fn count_store_files(root: &Path, only_json: bool) -> Result<(u64, u64)> {
    let mut count = 0;
    let mut bytes = 0;
    visit_store_files(root, &mut |path| {
        if !only_json
            || path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            count += 1;
            bytes += fs::metadata(path)?.len();
        }
        Ok(())
    })?;
    Ok((count, bytes))
}

fn visit_store_files(root: &Path, visitor: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            visit_store_files(&path, visitor)?;
        } else {
            visitor(&path)?;
        }
    }
    Ok(())
}

fn explain(
    selection: &diagnostics::Selection,
    limit: usize,
    json: bool,
    summary: bool,
) -> Result<()> {
    let events = diagnostics::read_selected(selection)?;
    if summary {
        let summary = diagnostics::summarize(&events);
        if json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            diagnostics::print_summary(&summary, limit);
        }
        return Ok(());
    }
    let selected = events
        .into_iter()
        .rev()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "miss" | "bypass" | "fallback" | "candidate_rejected" | "corrupt"
            )
        })
        .take(limit)
        .collect::<Vec<_>>();
    if json {
        println!("{}", serde_json::to_string_pretty(&selected)?);
    } else if selected.is_empty() {
        println!(
            "{}",
            terminal::success(
                terminal::stdout_color(),
                "clean",
                "no matching cache decisions in the retained log (use --local, --session, or --latest to select a build)",
            )
        );
    } else {
        let color = terminal::stdout_color();
        println!(
            "{}",
            terminal::heading(color, "Bellows · Recent cache decisions")
        );
        for event in selected {
            println!(
                "{}",
                terminal::status(
                    color,
                    &event.kind,
                    &event.crate_name,
                    &format!("{} · {}", event.timestamp_ms, event.detail),
                )
            );
        }
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn run_archive(command: ArchiveCommands) -> Result<()> {
    match command {
        ArchiveCommands::Publish {
            name,
            path,
            connection,
        } => {
            let root = path
                .canonicalize()
                .with_context(|| format!("open archive root {}", path.display()))?;
            if !root.is_dir() {
                bail!("archive root must be a directory")
            }
            let remote = Remote::new(&connection.server, connection.token)?;
            let files = collect_tree(&root, &root)?;
            if files.is_empty() {
                bail!("cannot publish an empty archive")
            }
            let mut artifacts = Vec::new();
            for (artifact, bytes) in files {
                remote.put_blob(&artifact.digest, bytes)?;
                artifacts.push(artifact);
            }
            artifacts.sort_by(|left, right| left.file_name.cmp(&right.file_name));
            let manifest = ArchiveManifest {
                protocol: PROTOCOL_VERSION,
                name: name.clone(),
                tree_digest: tree_digest(&artifacts),
                created_ms: now_ms(),
                producer_action: None,
                files: artifacts,
            };
            remote.put_archive(&manifest)?;
            println!(
                "{}",
                terminal::status(
                    terminal::stdout_color(),
                    "published",
                    &name,
                    &format!(
                        "{} · {} files",
                        &manifest.tree_digest[..12],
                        manifest.files.len()
                    ),
                )
            );
        }
        ArchiveCommands::Restore {
            name,
            path,
            connection,
        } => {
            let remote = Remote::new(&connection.server, connection.token)?;
            let manifest = remote
                .archive(&name)?
                .with_context(|| format!("archive {name} does not exist"))?;
            validate_archive_manifest(&manifest)?;
            let mut staged = Vec::with_capacity(manifest.files.len());
            for artifact in &manifest.files {
                staged.push((artifact, remote.blob(&artifact.digest)?));
            }
            fs::create_dir_all(&path)?;
            let restore_root = path.canonicalize()?;
            for (artifact, bytes) in staged {
                let relative = validate_relative_path(&artifact.file_name)?;
                let destination = safe_destination(&restore_root, &relative)?;
                atomic_write(&destination, &bytes)?;
                set_file_executable(&destination, artifact.executable)?;
            }
            println!(
                "{}",
                terminal::status(
                    terminal::stdout_color(),
                    "restored",
                    &name,
                    &format!(
                        "{} · {} files",
                        &manifest.tree_digest[..12],
                        manifest.files.len()
                    ),
                )
            );
        }
    }
    Ok(())
}

fn run_declared_action(args: DeclaredRunArgs, remote_execution: bool) -> Result<()> {
    let workspace = env::current_dir()?.canonicalize()?;
    let platform = PlatformIdentity::detect()?;
    validate_declared_command(&args.command)?;
    let inputs_with_bytes = collect_declared_inputs(&workspace, &args.inputs)?;
    let inputs = inputs_with_bytes
        .iter()
        .map(|(artifact, _)| artifact.clone())
        .collect::<Vec<_>>();
    let outputs = args
        .outputs
        .iter()
        .map(|path| normalize_declared_path(&workspace, path))
        .collect::<Result<Vec<_>>>()?;
    bellows_core::validate_output_roots(&inputs, &outputs)?;
    let environment = declared_environment(&args.environment)?;
    let key = declared_action_key(
        &args.name,
        &platform,
        &args.command,
        &environment,
        &inputs,
        &outputs,
    );
    let request = ExecuteRequest {
        key: key.clone(),
        name: args.name.clone(),
        platform: platform.clone(),
        command: args.command.clone(),
        environment: environment.clone(),
        inputs: inputs.clone(),
        outputs: outputs.clone(),
    };
    if args.local {
        return run_local_declared_action(
            &workspace,
            args.cache_dir.as_deref(),
            request,
            &inputs_with_bytes,
        );
    }
    let remote = Remote::new(&args.connection.server, args.connection.token)?;
    if let Some(record) = remote.declared(&key)? {
        restore_declared_record(&remote, &workspace, &record)?;
        println!(
            "{}",
            terminal::status(terminal::stdout_color(), "hit", &args.name, &key[..12],)
        );
        return Ok(());
    }

    for (artifact, bytes) in &inputs_with_bytes {
        remote.put_blob(&artifact.digest, bytes.clone())?;
    }
    let record = if remote_execution {
        let response = remote.execute(&request)?;
        println!(
            "{}",
            terminal::status(
                terminal::stdout_color(),
                if response.cache_hit {
                    "hit"
                } else {
                    "executed"
                },
                &args.name,
                &key[..12],
            )
        );
        response.record
    } else {
        let (record, blobs) = execute_local_declared(&workspace, request, &inputs_with_bytes)?;
        for (digest, bytes) in blobs {
            remote.put_blob(&digest, bytes)?;
        }
        remote.put_declared(&record)?;
        println!(
            "{}",
            terminal::status(terminal::stdout_color(), "miss", &args.name, &key[..12],)
        );
        record
    };
    restore_declared_record(&remote, &workspace, &record)?;
    Ok(())
}

fn run_local_declared_action(
    workspace: &Path,
    cache_dir: Option<&Path>,
    request: ExecuteRequest,
    inputs: &[(Artifact, Vec<u8>)],
) -> Result<()> {
    let store = local_store(cache_dir, true)?;
    if let Some(record) = store.read_declared(&request.key)? {
        match load_declared_record_local(&store, &record) {
            Ok(loaded) => {
                materialize_declared_record(workspace, &record, loaded)?;
                println!(
                    "{}",
                    terminal::status(
                        terminal::stdout_color(),
                        "hit",
                        &request.name,
                        &request.key[..12],
                    )
                );
                return Ok(());
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    terminal::warning(
                        terminal::stderr_color(),
                        "local action",
                        &format!("stale cached result will be rebuilt: {error:#}"),
                    )
                );
                store.remove_declared(&request.key)?;
            }
        }
    }

    let name = request.name.clone();
    let key = request.key.clone();
    let (record, blobs) = execute_local_declared(workspace, request, inputs)?;
    for (digest, bytes) in blobs {
        store.put_blob(&digest, &bytes)?;
    }
    if let Err(error) = store.put_declared(&record) {
        eprintln!(
            "{}",
            terminal::warning(
                terminal::stderr_color(),
                "local action",
                &format!("result was produced but its record raced publication: {error:#}"),
            )
        );
    }
    restore_declared_record_local(&store, workspace, &record)?;
    println!(
        "{}",
        terminal::status(terminal::stdout_color(), "miss", &name, &key[..12],)
    );
    Ok(())
}

fn declared_environment(names: &[String]) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for name in names {
        if matches!(
            name.as_str(),
            "PATH" | "HOME" | "CARGO_HOME" | "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN"
        ) {
            bail!("declared environment may not override {name}")
        }
        let value =
            env::var(name).with_context(|| format!("declared environment {name} is absent"))?;
        values.insert(name.clone(), value);
    }
    Ok(values)
}

fn collect_declared_inputs(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<(Artifact, Vec<u8>)>> {
    let mut files = Vec::new();
    for path in paths {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            workspace.join(path)
        };
        let canonical = path
            .canonicalize()
            .with_context(|| format!("open declared input {}", path.display()))?;
        if !canonical.starts_with(workspace) {
            bail!("declared input escapes workspace: {}", path.display())
        }
        files.extend(collect_tree(workspace, &canonical)?);
    }
    files.sort_by(|left, right| left.0.file_name.cmp(&right.0.file_name));
    files.dedup_by(|left, right| left.0.file_name == right.0.file_name);
    Ok(files)
}

fn collect_tree(root: &Path, path: &Path) -> Result<Vec<(Artifact, Vec<u8>)>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!(
            "symlinks are not allowed in declared trees: {}",
            path.display()
        )
    }
    if metadata.is_dir() {
        let mut files = Vec::new();
        for entry in fs::read_dir(path)? {
            files.extend(collect_tree(root, &entry?.path())?);
        }
        return Ok(files);
    }
    if !metadata.is_file() {
        bail!(
            "declared tree entry is not a regular file: {}",
            path.display()
        )
    }
    let relative = path.strip_prefix(root)?;
    let relative = validate_relative_path(&relative.to_string_lossy())?;
    let bytes = fs::read(path)?;
    Ok(vec![(
        Artifact {
            file_name: relative.to_string_lossy().into_owned(),
            digest: digest_bytes(&bytes),
            executable: is_executable(path)?,
        },
        bytes,
    )])
}

fn normalize_declared_path(workspace: &Path, path: &Path) -> Result<String> {
    let relative = if path.is_absolute() {
        path.strip_prefix(workspace)
            .with_context(|| format!("declared output escapes workspace: {}", path.display()))?
    } else {
        path
    };
    Ok(validate_relative_path(&relative.to_string_lossy())?
        .to_string_lossy()
        .into_owned())
}

fn execute_local_declared(
    destination_workspace: &Path,
    request: ExecuteRequest,
    inputs: &[(Artifact, Vec<u8>)],
) -> Result<(DeclaredActionRecord, BTreeMap<String, Vec<u8>>)> {
    let state = state_dir(destination_workspace);
    fs::create_dir_all(state.join("sandboxes"))?;
    let temp = tempfile::Builder::new()
        .prefix("action-")
        .tempdir_in(state.join("sandboxes"))?;
    let workspace = temp.path().join("workspace");
    let cargo_home = workspace.join(".bellows-cargo-home");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&cargo_home)?;
    for (artifact, bytes) in inputs {
        let destination = workspace.join(validate_relative_path(&artifact.file_name)?);
        atomic_write(&destination, bytes)?;
        set_file_executable(&destination, artifact.executable)?;
    }

    let started = Instant::now();
    let output = run_sandbox_command(&workspace, &request)?;
    let duration_ms = started.elapsed().as_millis() as u64;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        bail!("declared command failed with {}", output.status)
    }
    let mut blobs = BTreeMap::new();
    for (artifact, bytes) in inputs {
        blobs.insert(artifact.digest.clone(), bytes.clone());
    }
    let mut outputs = Vec::new();
    for declaration in &request.outputs {
        let path = workspace.join(validate_relative_path(declaration)?);
        for (artifact, bytes) in collect_tree(&workspace, &path)? {
            blobs.insert(artifact.digest.clone(), bytes);
            outputs.push(artifact);
        }
    }
    outputs.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    outputs.dedup_by(|left, right| left.file_name == right.file_name);
    if outputs.is_empty() {
        bail!("declared command produced no outputs")
    }
    let normalizer = PathNormalizer::new(vec![("$SANDBOX".into(), workspace)]);
    let stdout = normalizer.normalize_bytes(&output.stdout);
    let stderr = normalizer.normalize_bytes(&output.stderr);
    let stdout_digest = digest_bytes(&stdout);
    let stderr_digest = digest_bytes(&stderr);
    blobs.insert(stdout_digest.clone(), stdout.clone());
    blobs.insert(stderr_digest.clone(), stderr.clone());
    let record = DeclaredActionRecord {
        protocol: PROTOCOL_VERSION,
        key: request.key,
        name: request.name,
        created_ms: now_ms(),
        platform: request.platform,
        command: request.command,
        environment: request.environment,
        inputs: request.inputs,
        output_paths: request.outputs,
        outputs,
        stdout: StreamArtifact {
            digest: stdout_digest,
            len: stdout.len() as u64,
        },
        stderr: StreamArtifact {
            digest: stderr_digest,
            len: stderr.len() as u64,
        },
        duration_ms,
        executor: "local-sandbox".into(),
    };
    Ok((record, blobs))
}

fn run_sandbox_command(workspace: &Path, request: &ExecuteRequest) -> Result<std::process::Output> {
    let mut command = Command::new(&request.command[0]);
    command
        .args(&request.command[1..])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", env::var("PATH").unwrap_or_default())
        .env("HOME", "/homeless-shelter")
        .env("CARGO_HOME", ".bellows-cargo-home")
        .env("CARGO_NET_OFFLINE", "true")
        .envs(bellows_core::execution::platform_environment())
        .envs(&request.environment);
    command.env("RUSTUP_HOME", rustup_home());
    if let Some(value) = &request.platform.rustup_toolchain {
        command.env("RUSTUP_TOOLCHAIN", value);
    }
    let program = Path::new(&request.command[0])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if program == "cargo" {
        bellows_core::execution::configure_cargo_remapping(
            &mut command,
            workspace,
            &request.environment,
        )?;
    } else if program == "rustc" {
        command
            .arg("--remap-path-prefix")
            .arg(format!("{}=/bellows/action", workspace.display()));
    }
    command.output().context("run declared sandbox command")
}

fn restore_declared_record(
    remote: &Remote,
    workspace: &Path,
    record: &DeclaredActionRecord,
) -> Result<()> {
    validate_declared_record(record)?;
    let mut staged = Vec::with_capacity(record.outputs.len());
    for artifact in &record.outputs {
        let bytes = remote.blob(&artifact.digest)?;
        staged.push((artifact, bytes));
    }
    let stdout = remote.blob(&record.stdout.digest)?;
    let stderr = remote.blob(&record.stderr.digest)?;
    if stdout.len() as u64 != record.stdout.len || stderr.len() as u64 != record.stderr.len {
        bail!("declared stream length does not match manifest")
    }
    restore::declared_outputs(workspace, record, &staged)?;
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

struct LoadedDeclared<'a> {
    artifacts: Vec<(&'a Artifact, Vec<u8>)>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn load_declared_record_local<'a>(
    store: &Store,
    record: &'a DeclaredActionRecord,
) -> Result<LoadedDeclared<'a>> {
    validate_declared_record(record)?;
    let mut artifacts = Vec::with_capacity(record.outputs.len());
    for artifact in &record.outputs {
        artifacts.push((artifact, store.read_blob(&artifact.digest)?));
    }
    let stdout = store.read_blob(&record.stdout.digest)?;
    let stderr = store.read_blob(&record.stderr.digest)?;
    if stdout.len() as u64 != record.stdout.len || stderr.len() as u64 != record.stderr.len {
        bail!("declared stream length does not match manifest")
    }
    Ok(LoadedDeclared {
        artifacts,
        stdout,
        stderr,
    })
}

fn materialize_declared_record(
    workspace: &Path,
    record: &DeclaredActionRecord,
    loaded: LoadedDeclared<'_>,
) -> Result<()> {
    // Destination failures aren't corrupt cache entries: keep the record and
    // report the real filesystem error instead of deleting it and recompiling.
    restore::declared_outputs(workspace, record, &loaded.artifacts)?;
    std::io::stdout().write_all(&loaded.stdout)?;
    std::io::stderr().write_all(&loaded.stderr)?;
    Ok(())
}

fn restore_declared_record_local(
    store: &Store,
    workspace: &Path,
    record: &DeclaredActionRecord,
) -> Result<()> {
    materialize_declared_record(
        workspace,
        record,
        load_declared_record_local(store, record)?,
    )
}

fn safe_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(segment) = component else {
            bail!("unsafe restore path: {}", relative.display())
        };
        current.push(segment);
        if index + 1 == components.len() {
            if fs::symlink_metadata(&current)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                bail!("restore destination is a symlink: {}", current.display())
            }
        } else if current.exists() {
            let metadata = fs::symlink_metadata(&current)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("unsafe restore parent: {}", current.display())
            }
        } else {
            fs::create_dir(&current)?;
        }
    }
    Ok(current)
}

fn set_file_executable(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

fn run_analysis(command: AnalyzeCommands) -> Result<()> {
    match command {
        AnalyzeCommands::Snapshot { name } => create_workspace_snapshot(&name),
        AnalyzeCommands::Compare {
            before,
            after,
            json,
        } => compare_workspace_snapshots(&before, &after, json),
    }
}

const SURFACE_CAVEAT: &str = "Advisory syntactic public surface only; generic and #[inline] bodies, default trait methods, exported macros, generated code, and compiler metadata may affect downstream crates without changing this digest. Never use this result to authorize a cache hit.";

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    resolve: Option<CargoResolve>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    manifest_path: String,
}

#[derive(Debug, Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoNode>,
}

#[derive(Debug, Deserialize)]
struct CargoNode {
    id: String,
    dependencies: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkspaceSnapshot {
    protocol: u32,
    name: String,
    created_ms: u64,
    workspace: String,
    caveat: String,
    packages: BTreeMap<String, PackageSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PackageSnapshot {
    id: String,
    name: String,
    root: String,
    source_digest: String,
    syntactic_surface_digest: String,
    dependencies: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImpactReport {
    before: String,
    after: String,
    caveat: String,
    source_changed: Vec<String>,
    syntactic_surface_changed: Vec<String>,
    private_implementation_candidates: Vec<String>,
    affected_downstream: Vec<String>,
}

fn snapshot_path(name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid snapshot name")
    }
    let workspace = env::current_dir()?.canonicalize()?;
    Ok(state_dir(&workspace)
        .join("snapshots")
        .join(format!("{name}.json")))
}

fn create_workspace_snapshot(name: &str) -> Result<()> {
    let workspace = env::current_dir()?.canonicalize()?;
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .context("run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)?;
    let members = metadata
        .workspace_members
        .into_iter()
        .collect::<BTreeSet<_>>();
    let dependency_map = metadata
        .resolve
        .map(|resolve| {
            resolve
                .nodes
                .into_iter()
                .map(|node| (node.id, node.dependencies))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut packages = BTreeMap::new();
    for package in metadata
        .packages
        .into_iter()
        .filter(|package| members.contains(&package.id))
    {
        let manifest = PathBuf::from(&package.manifest_path);
        let root = manifest
            .parent()
            .context("manifest has no parent")?
            .canonicalize()?;
        let rust_files = collect_rust_sources(&root)?;
        let mut source_hasher = blake3::Hasher::new();
        let mut public_lines = Vec::new();
        for path in &rust_files {
            let relative = path.strip_prefix(&root)?;
            let bytes = fs::read(path)?;
            hash_field(&mut source_hasher, &relative.to_string_lossy(), &bytes);
            let source = String::from_utf8_lossy(&bytes);
            for line in source.lines().map(str::trim) {
                if is_syntactic_public_line(line) {
                    public_lines.push(format!("{}:{line}", relative.display()));
                }
            }
        }
        for control in [&manifest, &root.join("build.rs")] {
            if control.is_file() {
                hash_field(
                    &mut source_hasher,
                    &control
                        .strip_prefix(&root)
                        .unwrap_or(control)
                        .to_string_lossy(),
                    &fs::read(control)?,
                );
            }
        }
        public_lines.sort();
        let dependencies = dependency_map
            .get(&package.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|dependency| members.contains(dependency))
            .collect();
        packages.insert(
            package.id.clone(),
            PackageSnapshot {
                id: package.id,
                name: package.name,
                root: root
                    .strip_prefix(&workspace)
                    .unwrap_or(&root)
                    .to_string_lossy()
                    .into_owned(),
                source_digest: source_hasher.finalize().to_hex().to_string(),
                syntactic_surface_digest: digest_bytes(public_lines.join("\n").as_bytes()),
                dependencies,
            },
        );
    }
    let snapshot = WorkspaceSnapshot {
        protocol: PROTOCOL_VERSION,
        name: name.into(),
        created_ms: now_ms(),
        workspace: workspace.to_string_lossy().into_owned(),
        caveat: SURFACE_CAVEAT.into(),
        packages,
    };
    let path = snapshot_path(name)?;
    atomic_write(&path, &serde_json::to_vec_pretty(&snapshot)?)?;
    println!(
        "{}",
        terminal::status(
            terminal::stdout_color(),
            "captured",
            name,
            &format!("{} packages", snapshot.packages.len()),
        )
    );
    Ok(())
}

fn collect_rust_sources(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == "target" || name == ".git")
            {
                return Ok(());
            }
            for entry in fs::read_dir(path)? {
                visit(&entry?.path(), files)?;
            }
        } else if metadata.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        {
            files.push(path.to_path_buf());
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn is_syntactic_public_line(line: &str) -> bool {
    line.starts_with("pub ")
        || line.starts_with("pub async ")
        || line.starts_with("pub const ")
        || line.starts_with("pub unsafe ")
        || line == "#[macro_export]"
}

fn compare_workspace_snapshots(before: &str, after: &str, json: bool) -> Result<()> {
    let before_snapshot: WorkspaceSnapshot =
        serde_json::from_slice(&fs::read(snapshot_path(before)?)?)?;
    let after_snapshot: WorkspaceSnapshot =
        serde_json::from_slice(&fs::read(snapshot_path(after)?)?)?;
    let mut source_changed = BTreeSet::new();
    let mut surface_changed = BTreeSet::new();
    for (id, package) in &after_snapshot.packages {
        match before_snapshot.packages.get(id) {
            Some(previous) => {
                if previous.source_digest != package.source_digest {
                    source_changed.insert(id.clone());
                }
                if previous.syntactic_surface_digest != package.syntactic_surface_digest {
                    surface_changed.insert(id.clone());
                }
            }
            None => {
                source_changed.insert(id.clone());
                surface_changed.insert(id.clone());
            }
        }
    }
    let mut reverse = BTreeMap::<String, Vec<String>>::new();
    for (id, package) in &after_snapshot.packages {
        for dependency in &package.dependencies {
            reverse
                .entry(dependency.clone())
                .or_default()
                .push(id.clone());
        }
    }
    let mut affected = surface_changed.clone();
    let mut frontier = surface_changed.iter().cloned().collect::<Vec<_>>();
    while let Some(changed) = frontier.pop() {
        for downstream in reverse.get(&changed).into_iter().flatten() {
            if affected.insert(downstream.clone()) {
                frontier.push(downstream.clone());
            }
        }
    }
    let package_names = |ids: &BTreeSet<String>| {
        ids.iter()
            .filter_map(|id| {
                after_snapshot
                    .packages
                    .get(id)
                    .map(|package| package.name.clone())
            })
            .collect::<Vec<_>>()
    };
    let private = source_changed
        .difference(&surface_changed)
        .cloned()
        .collect::<BTreeSet<_>>();
    let report = ImpactReport {
        before: before.into(),
        after: after.into(),
        caveat: SURFACE_CAVEAT.into(),
        source_changed: package_names(&source_changed),
        syntactic_surface_changed: package_names(&surface_changed),
        private_implementation_candidates: package_names(&private),
        affected_downstream: package_names(&affected),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let color = terminal::stdout_color();
        println!(
            "{}",
            terminal::heading(color, &format!("Bellows · Impact · {before} → {after}"))
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "source changed",
                display_names(&report.source_changed)
            )
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "surface changed",
                display_names(&report.syntactic_surface_changed),
            )
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "relink candidates",
                display_names(&report.private_implementation_candidates),
            )
        );
        println!(
            "{}",
            terminal::key_value(
                color,
                "affected downstream",
                display_names(&report.affected_downstream),
            )
        );
        println!("{}", terminal::section(color, "Caveat"));
        println!("  {}", report.caveat);
    }
    Ok(())
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_skew_is_rejected_in_both_directions() {
        assert!(validate_protocol(PROTOCOL_VERSION).is_ok());
        assert!(validate_protocol(PROTOCOL_VERSION - 1).is_err());
        assert!(validate_protocol(PROTOCOL_VERSION + 1).is_err());
    }

    #[test]
    fn remote_configuration_rejects_unsafe_urls_and_unbounded_timeouts() {
        for url in [
            "file:///tmp/cache",
            "http://user:secret@localhost:7878",
            "http://localhost:7878?token=secret",
            "://not-a-url",
        ] {
            assert!(Remote::new(url, None).is_err(), "accepted {url}");
        }
        assert!(Remote::new("http://127.0.0.1:7878", None).is_ok());
        assert!(bounded_timeout("TEST", Some("99"), 2_000, 100, 30_000).is_err());
        assert!(bounded_timeout("TEST", Some("30001"), 2_000, 100, 30_000).is_err());
        assert_eq!(
            bounded_timeout("TEST", Some("2500"), 2_000, 100, 30_000).unwrap(),
            Duration::from_millis(2_500)
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalizer_bases_resolve_symlinked_mounts() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        assert_eq!(canonical_base(alias), real.canonicalize().unwrap());
    }

    #[test]
    fn infers_custom_cargo_target_roots_from_deps_outputs() {
        assert_eq!(
            infer_target_root(Path::new("/tmp/runner-a/release/deps")),
            Some(PathBuf::from("/tmp/runner-a"))
        );
        assert_eq!(
            infer_target_root(Path::new(
                "/tmp/runner-b/wasm32-unknown-unknown/release/deps"
            )),
            Some(PathBuf::from("/tmp/runner-b/wasm32-unknown-unknown"))
        );
    }

    fn test_identity(workspace: &Path) -> Identity {
        let bases = vec![("$WORKSPACE".to_owned(), workspace.to_path_buf())];
        Identity {
            static_key: digest_bytes(b"identity"),
            fingerprint: diagnostics::Fingerprint::default(),
            diagnostic_group: String::new(),
            normalizer: PathNormalizer::new(bases.clone()),
            root_normalizer: PathNormalizer::new(bases),
            workspace: workspace.to_path_buf(),
            digests: digests::Digests::uncached(),
            pins: vec![(
                format!("{PIN_PREFIX}$WORKSPACE"),
                workspace.to_string_lossy().into_owned(),
            )],
            virtual_env: BTreeMap::new(),
        }
    }

    fn test_invocation(out_dir: &Path, expected: &[&str], inputs: Vec<PathBuf>) -> Invocation {
        Invocation {
            rustc: PathBuf::from("rustc"),
            args: vec![],
            crate_name: "fixture".into(),
            out_dir: out_dir.to_path_buf(),
            kind: OutputKind::Library,
            naming: Naming::for_target(None),
            stems: vec!["fixture-abc".into(), "libfixture-abc".into()],
            expected_names: expected.iter().map(|name| (*name).to_owned()).collect(),
            explicit_inputs: inputs,
            proc_macros: vec![],
            incremental: None,
            unit: "fixture-abc".into(),
            proc_macro_crate: false,
            static_libraries: vec![],
            native_search: vec![],
        }
    }

    fn empty_candidate(identity: &Identity, protocol: u32) -> ActionCandidate {
        ActionCandidate {
            protocol,
            static_key: identity.static_key.clone(),
            action_key: compiler_action_key(&identity.static_key, &[], &[], &[]),
            crate_name: "fixture".into(),
            created_ms: 0,
            files: vec![],
            host_files: vec![],
            env: vec![],
            artifacts: vec![],
            stdout: StreamArtifact {
                digest: digest_bytes(b""),
                len: 0,
            },
            stderr: StreamArtifact {
                digest: digest_bytes(b""),
                len: 0,
            },
            proc_macros: vec![],
        }
    }

    #[test]
    fn checkout_pins_and_normalized_environment_validate_per_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let identity = test_identity(&first);
        let mut candidate = empty_candidate(&identity, PROTOCOL_VERSION);
        let pin = format!("{PIN_PREFIX}$WORKSPACE");
        candidate.env = vec![EnvInput::capture(&pin, identity.pin_value(&pin))];
        candidate.action_key = compiler_action_key(&candidate.static_key, &[], &[], &candidate.env);
        assert!(validate_candidate(&candidate, &identity).is_ok());
        let other = test_identity(&second);
        let reason = validate_candidate(&candidate, &other).unwrap_err();
        assert!(reason.contains("another checkout's path"), "{reason}");

        // A normalized env-dep matches the same relative location anywhere.
        let name = "BELLOWS_TEST_NORMALIZED_DIR";
        // SAFETY: only this test reads or writes this variable.
        unsafe { env::set_var(name, second.join("out")) };
        let mut normalized = empty_candidate(&other, PROTOCOL_VERSION);
        normalized.env = vec![EnvInput::capture_normalized(
            name,
            &format!("$WORKSPACE{}out", std::path::MAIN_SEPARATOR),
        )];
        normalized.action_key =
            compiler_action_key(&normalized.static_key, &[], &[], &normalized.env);
        assert!(validate_candidate(&normalized, &other).is_ok());
        unsafe { env::set_var(name, second.join("elsewhere")) };
        assert!(validate_candidate(&normalized, &other).is_err());
        unsafe { env::remove_var(name) };
    }

    #[test]
    fn prior_protocol_candidates_are_cleanly_rejected() {
        let workspace = std::env::temp_dir().join(format!("bellows-protocol-test-{}", now_ms()));
        let identity = test_identity(&workspace);
        let candidate = empty_candidate(&identity, PROTOCOL_VERSION - 1);
        assert_eq!(
            validate_candidate(&candidate, &identity).unwrap_err(),
            format!("unsupported candidate protocol {}", PROTOCOL_VERSION - 1)
        );
    }

    #[test]
    fn captured_stream_lengths_describe_normalized_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp
            .path()
            .canonicalize()
            .unwrap()
            .join("a-very-long-workspace-name");
        let out_dir = workspace.join("target/debug/deps");
        fs::create_dir_all(&out_dir).unwrap();
        let source = workspace.join("src/lib.rs");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();
        let dep_name = "fixture-abc.d";
        let rmeta_name = "libfixture-abc.rmeta";
        fs::write(out_dir.join(rmeta_name), b"metadata").unwrap();
        fs::write(
            out_dir.join(dep_name),
            format!(
                "{rmeta_name}: {}\n",
                source.to_string_lossy().replace(' ', "\\ ")
            ),
        )
        .unwrap();
        let invocation = test_invocation(&out_dir, &[dep_name, rmeta_name], vec![source]);
        let identity = test_identity(&workspace);
        let stdout = format!("compiled {}", workspace.display()).into_bytes();
        let stderr = format!("warning in {}", workspace.display()).into_bytes();
        let captured = capture_outputs(
            &invocation,
            &identity,
            stdout.clone(),
            stderr.clone(),
            std::time::SystemTime::now(),
            None,
        )
        .unwrap();
        assert!(!captured.pinned);
        assert_eq!(
            captured.candidate.files[0].path,
            format!("$WORKSPACE{0}src{0}lib.rs", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(
            captured.candidate.stdout.len,
            identity.normalizer.normalize_bytes(&stdout).len() as u64
        );
        assert_eq!(
            captured.candidate.stderr.len,
            identity.normalizer.normalize_bytes(&stderr).len() as u64
        );
        assert_ne!(captured.candidate.stdout.len, stdout.len() as u64);
    }

    #[test]
    fn recognizes_rustc_and_clippy_wrapper_invocations() {
        for compiler in ["rustc", "rustc-1.92.0", "clippy-driver"] {
            assert!(is_wrapper_invocation(&[
                OsString::from("bellows"),
                OsString::from(compiler),
                OsString::from("-vV"),
            ]));
        }
        assert!(!is_wrapper_invocation(&[
            OsString::from("bellows"),
            OsString::from("doctor"),
        ]));
    }

    #[test]
    fn cargo_shorthand_accepts_ordinary_cargo_flags() {
        let cli = Cli::try_parse_from(["bellows", "cargo", "run", "--release", "-p", "flagship"])
            .unwrap();
        let Commands::Cargo { arguments } = cli.command else {
            panic!("cargo shorthand parsed as the wrong command");
        };
        assert_eq!(
            arguments,
            ["run", "--release", "-p", "flagship"].map(OsString::from)
        );
    }

    #[test]
    fn parses_cacheable_library_invocation() {
        let dir = std::env::temp_dir().join(format!("bellows-cli-test-{}", now_ms()));
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
        let raw = vec![
            OsString::from("rustc"),
            OsString::from("--crate-name"),
            OsString::from("demo"),
            dir.join("lib.rs").into_os_string(),
            OsString::from("--crate-type"),
            OsString::from("lib"),
            OsString::from("--emit=dep-info,metadata,link"),
            OsString::from("-C"),
            OsString::from("extra-filename=-abc"),
            OsString::from("--out-dir"),
            dir.join("out").into_os_string(),
        ];
        let invocation = Invocation::analyze(&raw).unwrap();
        assert!(invocation.expected_names.contains("demo-abc.d"));
        assert!(invocation.expected_names.contains("libdemo-abc.rlib"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_all_unmodeled_emit_and_custom_output_forms() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("lib.rs");
        fs::write(&source, "pub fn value() {}").unwrap();
        for extra in [
            vec!["--emit=dep-info,metadata,asm"],
            vec!["--emit=dep-info,metadata=custom.rmeta"],
            vec!["--emit=dep-info,metadata", "-ocustom"],
            vec!["--emit=dep-info,metadata", "-o", "custom"],
            vec!["--emit=dep-info,metadata", "--emit=link"],
        ] {
            let mut raw = [
                "rustc",
                "--crate-name=fixture",
                "--crate-type=rlib",
                "-Cextra-filename=-fixture",
                "--out-dir=out",
            ]
            .map(OsString::from)
            .to_vec();
            raw.push(source.clone().into_os_string());
            raw.extend(extra.iter().map(OsString::from));
            assert!(
                Invocation::analyze(&raw)
                    .unwrap_err()
                    .contains("unsupported emit")
            );
        }
    }

    #[test]
    fn accepts_inert_library_link_arguments() {
        let dir = std::env::temp_dir().join(format!("bellows-cli-link-arg-{}", now_ms()));
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
        let base = vec![
            OsString::from("rustc"),
            OsString::from("--crate-name"),
            OsString::from("demo"),
            dir.join("lib.rs").into_os_string(),
            OsString::from("--crate-type=rlib"),
            OsString::from("--emit=dep-info,metadata,link"),
            OsString::from("-Cextra-filename=-abc"),
            OsString::from("--out-dir"),
            dir.join("out").into_os_string(),
        ];

        for link_args in [
            vec![
                OsString::from("-C"),
                OsString::from("link-arg=-fuse-ld=lld"),
            ],
            vec![OsString::from("-Clink-arg=-fuse-ld=lld")],
            vec![OsString::from("-Clink-args=-fuse-ld=lld")],
            vec![
                OsString::from("-C"),
                OsString::from("link-arg=-Wl,-rpath,/x"),
            ],
        ] {
            let mut raw = base.clone();
            raw.extend(link_args);
            Invocation::analyze(&raw).unwrap();
        }
        let _ = fs::remove_dir_all(dir);
    }

    fn analyze_with(dir: &Path, extra: &[&str]) -> std::result::Result<Invocation, String> {
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
        let mut raw = vec![
            OsString::from("rustc"),
            OsString::from("--crate-name=demo"),
            dir.join("lib.rs").into_os_string(),
            OsString::from("-Cextra-filename=-abc"),
            OsString::from("--out-dir"),
            dir.join("out").into_os_string(),
        ];
        raw.extend(extra.iter().map(OsString::from));
        Invocation::analyze(&raw)
    }

    #[test]
    fn linked_crate_types_and_test_harnesses_are_modeled() {
        let temp = tempfile::tempdir().unwrap();
        let exe = std::env::consts::EXE_SUFFIX;
        let linked = |extra: &[&str]| analyze_with(temp.path(), extra).unwrap();

        let test = linked(&["--emit=dep-info,link", "--test"]);
        assert_eq!(test.kind, OutputKind::Linked);
        assert!(test.expected_names.contains(&format!("demo-abc{exe}")));
        assert!(test.owns("demo-abc.pdb") && test.owns("demo-abc") && test.owns("demo-abc.d"));
        assert!(!test.owns("demo-abcd") && !test.owns("demo-abc2.pdb"));

        let bin = linked(&[
            "--crate-type=bin",
            "--emit=dep-info,link",
            "-Clink-arg=-fuse-ld=lld",
        ]);
        assert_eq!(bin.kind, OutputKind::Linked);

        let cdylib = linked(&["--crate-type=rlib,cdylib", "--emit=dep-info,metadata,link"]);
        assert_eq!(cdylib.kind, OutputKind::Linked);
        assert!(cdylib.expected_names.contains("libdemo-abc.rlib"));
        let dll = Naming::for_target(None);
        assert!(
            cdylib
                .expected_names
                .contains(&format!("{}demo-abc{}", dll.dll_prefix, dll.dll_suffix))
        );

        // Checking a test target writes metadata only: no linker runs.
        let check = linked(&["--emit=dep-info,metadata", "--test"]);
        assert_eq!(check.kind, OutputKind::Library);
        assert!(check.expected_names.contains("libdemo-abc.rmeta"));

        let staticlib = analyze_with(
            temp.path(),
            &["--crate-type=staticlib", "--emit=dep-info,link"],
        );
        assert!(staticlib.unwrap_err().contains("staticlib"));
    }

    #[test]
    fn target_naming_follows_the_triple() {
        let windows = Naming::for_target(Some("x86_64-pc-windows-msvc"));
        assert_eq!(
            (windows.exe_suffix, windows.dll_suffix, windows.msvc),
            (".exe", ".dll", true)
        );
        let wasm = Naming::for_target(Some("wasm32-unknown-unknown"));
        assert_eq!(
            (wasm.exe_suffix, wasm.dll_prefix, wasm.dll_suffix),
            (".wasm", "", ".wasm")
        );
        let linux = Naming::for_target(Some("x86_64-unknown-linux-gnu"));
        assert_eq!(
            (linux.exe_suffix, linux.dll_prefix, linux.dll_suffix),
            ("", "lib", ".so")
        );
    }

    #[test]
    fn incremental_invocations_share_the_non_incremental_identity() {
        let temp = tempfile::tempdir().unwrap();
        let emit = ["--crate-type=rlib", "--emit=dep-info,link"];
        let plain = analyze_with(temp.path(), &emit).unwrap();
        let spaced = analyze_with(
            temp.path(),
            &[emit[0], emit[1], "-C", "incremental=/x/incr"],
        )
        .unwrap();
        let joined =
            analyze_with(temp.path(), &[emit[0], emit[1], "-Cincremental=/y/incr"]).unwrap();
        assert_eq!(spaced.incremental, Some(PathBuf::from("/x/incr")));
        let normalizer = PathNormalizer::new(vec![]);
        let args = normalized_compiler_arguments(&plain, &normalizer);
        assert_eq!(normalized_compiler_arguments(&spaced, &normalizer), args);
        assert_eq!(normalized_compiler_arguments(&joined, &normalizer), args);
    }

    #[test]
    fn proc_macro_consumers_record_macro_names() {
        let temp = tempfile::tempdir().unwrap();
        let so = temp.path().join("libderive-abc.so");
        fs::write(&so, b"dylib").unwrap();
        let external = format!("derive={}", so.display());
        let invocation = analyze_with(
            temp.path(),
            &[
                "--crate-type=rlib",
                "--emit=dep-info,metadata,link",
                "--extern",
                &external,
                "--extern",
                "proc_macro",
            ],
        )
        .unwrap();
        assert_eq!(invocation.proc_macros, ["derive"]);
        assert!(invocation.explicit_inputs.contains(&so));
    }

    #[test]
    fn bypasses_host_dependent_and_unmodeled_toolchain_inputs() {
        for extra in [
            vec!["-Ctarget-cpu=native"],
            vec!["-C", "target-cpu=native"],
            vec!["--sysroot", "/custom/toolchain"],
            vec!["--target=custom.json"],
            vec!["-Csave-temps"],
        ] {
            let mut raw = vec![OsString::from("rustc")];
            raw.extend(extra.into_iter().map(OsString::from));
            let reason = Invocation::analyze(&raw).unwrap_err();
            assert!(reason.contains("not modeled"), "{reason}");
        }
    }

    #[test]
    fn native_search_paths_that_can_supply_crates_stay_unmodeled() {
        let parse =
            |args: &[&str]| native_inputs(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        for search in ["/native", "all=/native", "crate=/native"] {
            assert!(parse(&["-L", search]).unwrap_err().contains("not modeled"));
            assert!(
                parse(&[&format!("-L{search}")])
                    .unwrap_err()
                    .contains("not modeled")
            );
        }
        let (dirs, libs) = parse(&[
            "-L",
            "dependency=/deps",
            "-Lnative=/opt/sdk",
            "-L",
            "framework=/f",
            "-l",
            "static=zstd",
            "-lstatic:+verbatim=libq.a",
            "-l",
            "asound",
            "-ldylib=z",
            "-lstatic:-bundle=nb",
        ])
        .unwrap();
        assert_eq!(dirs, [PathBuf::from("/opt/sdk"), PathBuf::from("/f")]);
        assert_eq!(
            libs.iter()
                .map(|l| (l.name.as_str(), l.verbatim))
                .collect::<Vec<_>>(),
            [("zstd", false), ("libq.a", true)]
        );
        assert!(parse(&["-l", "link-arg=-foo"]).is_err());
        assert!(has_unmodeled_codegen_inputs(&[
            "-C".into(),
            "profile-use=/p.profdata".into()
        ]));
        assert!(has_unmodeled_codegen_inputs(&[
            "-Cllvm-plugins=/opt/plugin".into()
        ]));
        assert!(has_unmodeled_codegen_inputs(&[
            "-C".into(),
            "linker-plugin-lto=/opt/p".into()
        ]));
        assert!(!has_unmodeled_codegen_inputs(&["-Clinker=mold".into()]));
        assert!(!has_unmodeled_codegen_inputs(&[
            "-C".into(),
            "link-arg=-fuse-ld=lld".into()
        ]));
    }

    #[test]
    fn only_descriptive_unstable_flags_are_modeled() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(
            unmodeled_unstable_flag(&args(&[
                "-Zunstable-options",
                "-Z",
                "force-unstable-if-unmarked",
                "-Zthreads=8"
            ])),
            None
        );
        assert_eq!(
            unmodeled_unstable_flag(&args(&["-Z", "self-profile"])).as_deref(),
            Some("self-profile")
        );
        assert_eq!(
            unmodeled_unstable_flag(&args(&["-Zdump-mir=all"])).as_deref(),
            Some("dump-mir")
        );
        let temp = tempfile::tempdir().unwrap();
        let std = temp.path().join("libstd-abc.rlib");
        fs::write(&std, b"rlib").unwrap();
        let external = format!("noprelude:std={}", std.display());
        let invocation = analyze_with(
            temp.path(),
            &[
                "--crate-type=rlib",
                "--emit=dep-info,metadata,link",
                "-Zunstable-options",
                "--extern",
                &external,
            ],
        )
        .unwrap();
        assert!(invocation.explicit_inputs.contains(&std));
    }

    #[test]
    fn library_compiles_accept_a_linker_they_never_run() {
        let temp = tempfile::tempdir().unwrap();
        let invocation = analyze_with(
            temp.path(),
            &[
                "--crate-type=rlib",
                "--emit=dep-info,metadata,link",
                "-Clinker=/opt/mold/cc",
                "-Lnative=/usr/lib",
            ],
        )
        .unwrap();
        assert_eq!(invocation.kind, OutputKind::Library);
    }

    #[test]
    fn rustc_library_link_arguments_are_byte_inert() {
        let dir = std::env::temp_dir().join(format!("bellows-cli-link-inert-{}", now_ms()));
        let plain = dir.join("plain");
        let linked = dir.join("linked");
        fs::create_dir_all(&plain).unwrap();
        fs::create_dir_all(&linked).unwrap();
        let source = dir.join("lib.rs");
        fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();

        let compile = |out_dir: &Path, with_link_arg: bool| {
            let mut command = Command::new("rustc");
            command
                .arg("--crate-name=demo")
                .arg(&source)
                .arg("--crate-type=rlib")
                .arg("--emit=link,metadata")
                .arg("--out-dir")
                .arg(out_dir);
            if with_link_arg {
                command.arg("-Clink-arg=-fuse-ld=lld");
            }
            assert!(command.status().unwrap().success());
        };
        compile(&plain, false);
        compile(&linked, true);
        for output in ["libdemo.rlib", "libdemo.rmeta"] {
            assert_eq!(
                fs::read(plain.join(output)).unwrap(),
                fs::read(linked.join(output)).unwrap()
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn excludes_per_run_ci_environment_from_static_keys() {
        assert!(!is_relevant_environment_name("GITHUB_RUN_ID"));
        assert!(!is_relevant_environment_name("GITHUB_ENV"));
        assert!(!is_relevant_environment_name("BELLOWS_AUTH_TOKEN"));
        assert!(is_relevant_environment_name("RUSTFLAGS"));
        assert!(is_relevant_environment_name("CARGO_MANIFEST_DIR"));
        assert!(is_relevant_environment_name("CC_x86_64_unknown_linux_gnu"));
        assert!(is_relevant_environment_name("CLIPPY_ARGS"));
        assert!(is_relevant_environment_name("CLIPPY_CONF_DIR"));
    }
}
#[test]
fn compiler_json_streams_escape_localized_windows_paths() {
    let normalizer = PathNormalizer::new(vec![(
        "$TARGET".into(),
        PathBuf::from(r"C:\cache with spaces"),
    )]);
    let message = serde_json::json!({
        "$message_type": "artifact",
        "artifact": r"C:\cache with spaces\libfixture.rlib",
        "nested": [{"rendered": "a quoted \"message\"\nsecond line"}]
    });
    let mut original = serde_json::to_vec(&message).unwrap();
    original.extend_from_slice(b"\r\n");
    let stored = transform_compiler_stream(&original, &normalizer, false);
    assert!(String::from_utf8_lossy(&stored).contains("$TARGET"));
    let restored = transform_compiler_stream(&stored, &normalizer, true);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&restored).unwrap(),
        message
    );
    assert!(restored.ends_with(b"\r\n"));
}

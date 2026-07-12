use anyhow::{Context, Result, anyhow, bail};
use bellows_core::{
    ActionCandidate, ArchiveManifest, Artifact, CandidateIndex, DeclaredActionRecord, EnvInput,
    Event, ExecuteRequest, ExecuteResponse, FileInput, GcReport, GcRequest, HealthResponse,
    LeaseRequest, LeaseResponse, PROTOCOL_VERSION, PathNormalizer, PlatformIdentity, ServerStats,
    Store, StreamArtifact, atomic_write, compiler_action_key, declared_action_key, digest_bytes,
    digest_file, now_ms, parse_dep_info, rustup_home, tree_digest, validate_archive_manifest,
    validate_candidate_manifest, validate_declared_command, validate_declared_record,
    validate_relative_path,
};
use clap::{Args, Parser, Subcommand};
use fs2::FileExt;
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

#[derive(Parser, Debug)]
#[command(name = "bellows", version, about = "Cargo-native remote builds")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run an ordinary command with Bellows installed as Cargo's rustc wrapper.
    Run {
        #[arg(long, env = "BELLOWS_SERVER", default_value = "http://127.0.0.1:7878")]
        server: String,
        #[arg(long, env = "BELLOWS_AUTH_TOKEN")]
        token: Option<String>,
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
    },
    /// Explain recent misses, bypasses, fallbacks, and integrity failures.
    Explain {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
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
    let result = if is_wrapper_invocation(&args) {
        rustc_wrapper(&args[1..]).map(|status| status.code().unwrap_or(1))
    } else {
        run_cli()
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code.clamp(1, 255) as u8),
        Err(error) => {
            eprintln!("bellows: {error:#}");
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
        Commands::Run {
            server,
            token,
            command,
        } => run_command(server, token, command),
        Commands::Doctor { server, token } => {
            doctor(&server, token.as_deref())?;
            Ok(0)
        }
        Commands::Stats {
            server,
            token,
            json,
        } => {
            show_stats(&server, token.as_deref(), json)?;
            Ok(0)
        }
        Commands::Explain { limit, json } => {
            explain(limit, json)?;
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
                RemoteCommands::Run(args) => run_declared_action(args, true)?,
            }
            Ok(0)
        }
        Commands::Analyze { command } => {
            run_analysis(command)?;
            Ok(0)
        }
        Commands::Gc { max_mb, connection } => {
            let report = Remote::new(&connection.server, connection.token)?
                .gc(max_mb.saturating_mul(1024 * 1024))?;
            println!(
                "GC: {} -> {}, {} records and {} blobs evicted",
                human_bytes(report.bytes_before),
                human_bytes(report.bytes_after),
                report.records_evicted,
                report.blobs_evicted
            );
            Ok(0)
        }
    }
}

fn run_command(server: String, token: Option<String>, command: Vec<OsString>) -> Result<i32> {
    let (program, arguments) = command.split_first().context("missing command")?;
    let workspace = env::current_dir()?.canonicalize()?;
    let state_dir = state_dir(&workspace);
    fs::create_dir_all(&state_dir)?;
    let wrapper = env::current_exe()?.canonicalize()?;
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("RUSTC_WRAPPER", &wrapper)
        .env("BELLOWS_SERVER", server)
        .env("BELLOWS_WORKSPACE", &workspace)
        .env("BELLOWS_STATE_DIR", &state_dir)
        .env("CARGO_INCREMENTAL", "0");
    if let Some(token) = token {
        child.env("BELLOWS_AUTH_TOKEN", token);
    }
    eprintln!(
        "bellows: feeding the forge — {}",
        display_command(program, arguments)
    );
    let status = child.status().context("start wrapped command")?;
    Ok(status.code().unwrap_or(1))
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

#[derive(Debug)]
struct Invocation {
    rustc: PathBuf,
    args: Vec<String>,
    crate_name: String,
    out_dir: PathBuf,
    expected_names: BTreeSet<String>,
    explicit_inputs: Vec<PathBuf>,
}

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
        if args.iter().any(|a| a.contains("incremental=")) {
            return Err("incremental compilation is enabled".into());
        }
        if args.iter().any(|arg| arg.starts_with('@')) {
            return Err("rustc response files are not modeled".into());
        }
        if args.iter().any(|arg| arg == "-Z" || arg.starts_with("-Z")) {
            return Err("unstable compiler flags are not modeled".into());
        }
        let crate_name = option_value(&args, "--crate-name").ok_or("missing --crate-name")?;
        let declared_crate_types = multi_option_values(&args, "--crate-type");
        let crate_types = if declared_crate_types.is_empty() {
            vec!["bin".to_owned()]
        } else {
            declared_crate_types
                .iter()
                .flat_map(|value| value.split(','))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        if crate_types
            .iter()
            .any(|kind| !matches!(kind.as_str(), "lib" | "rlib"))
        {
            return Err(format!("linked crate type {}", crate_types.join(",")));
        }
        if has_native_or_external_codegen_inputs(&args) {
            return Err("native linker or external codegen inputs are not modeled".into());
        }
        let out_dir = PathBuf::from(option_value(&args, "--out-dir").ok_or("missing --out-dir")?);
        let extra_filename =
            codegen_value(&args, "extra-filename").ok_or("missing -C extra-filename")?;
        if extra_filename.is_empty() || extra_filename.contains('/') {
            return Err("ambiguous extra filename".into());
        }
        let source = args
            .iter()
            .find(|arg| arg.ends_with(".rs") && Path::new(arg.as_str()).exists())
            .map(PathBuf::from)
            .ok_or("missing primary Rust source")?;

        let mut explicit_inputs = vec![source.clone()];
        let mut proc_macro = false;
        for value in multi_option_values(&args, "--extern") {
            let path = value
                .split_once('=')
                .map(|(_, path)| path)
                .unwrap_or(&value);
            let path = PathBuf::from(path);
            if path.exists() {
                let ext = path.extension().and_then(OsStr::to_str).unwrap_or_default();
                if matches!(ext, "so" | "dylib" | "dll") {
                    proc_macro = true;
                }
                explicit_inputs.push(path);
            }
        }
        if proc_macro {
            return Err("invocation loads a procedural macro or dynamic compiler plugin".into());
        }

        let emit = option_value(&args, "--emit").unwrap_or_default();
        let mut expected_names = BTreeSet::new();
        if emit
            .split(',')
            .any(|e| e.split('=').next() == Some("dep-info"))
        {
            expected_names.insert(format!("{crate_name}{extra_filename}.d"));
        }
        if emit
            .split(',')
            .any(|e| e.split('=').next() == Some("metadata"))
        {
            expected_names.insert(format!("lib{crate_name}{extra_filename}.rmeta"));
        }
        if emit.split(',').any(|e| e.split('=').next() == Some("link")) {
            expected_names.insert(format!("lib{crate_name}{extra_filename}.rlib"));
        }
        if expected_names.is_empty() || !expected_names.iter().any(|n| n.ends_with(".d")) {
            return Err("unsupported emit set".into());
        }
        Ok(Self {
            rustc,
            args,
            crate_name,
            out_dir,
            expected_names,
            explicit_inputs,
        })
    }
}

fn has_native_or_external_codegen_inputs(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        if arg == "-l" || (arg.starts_with("-l") && arg.len() > 2) {
            return true;
        }
        if arg == "-L" {
            return args.get(index + 1).is_some_and(|value| {
                value.starts_with("native=") || value.starts_with("framework=")
            });
        }
        if arg.starts_with("-Lnative=") || arg.starts_with("-Lframework=") {
            return true;
        }
        let codegen = if arg == "-C" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("-C")
        };
        codegen.is_some_and(|value| {
            [
                "linker=",
                "linker-plugin-lto=",
                "profile-use=",
                "llvm-plugins=",
            ]
            .iter()
            .any(|prefix| value.starts_with(prefix))
        })
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
    workspace: PathBuf,
}

fn rustc_wrapper(raw: &[OsString]) -> Result<ExitStatus> {
    match cache_or_compile(raw) {
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
    }
}

fn wrapper_crate_name(raw: &[OsString]) -> &str {
    raw.windows(2)
        .find(|pair| pair[0] == "--crate-name")
        .and_then(|pair| pair[1].to_str())
        .unwrap_or("rustc")
}

fn cache_or_compile(raw: &[OsString]) -> Result<ExitStatus> {
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
    let server = env::var("BELLOWS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
    let remote = match Remote::new(&server, env::var("BELLOWS_AUTH_TOKEN").ok()) {
        Ok(remote) => remote,
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
    };
    let l1 = if env::var("BELLOWS_L1").as_deref() == Ok("0") {
        None
    } else {
        match Store::open(state_dir(&identity.workspace).join("l1")) {
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
    if let Some(store) = &l1 {
        match store.read_candidates(&identity.static_key) {
            Ok(index) => match try_l1_candidates(store, &invocation, &identity, &index) {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(error) => record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    &format!("L1 restore failed: {error:#}"),
                ),
            },
            Err(error) => record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("L1 index is unavailable or corrupt: {error:#}"),
            ),
        }
    }

    let (index, remote_available) = match remote.candidates(&identity.static_key) {
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
    };
    if let Some(status) = try_candidates(&remote, l1.as_ref(), &invocation, &identity, &index)? {
        return Ok(status);
    }

    let miss_detail = explain_candidates(&identity, &index);
    record_event(
        "miss",
        &invocation.crate_name,
        Some(&identity.static_key),
        None,
        &miss_detail,
    );

    let client_id = format!("{}-{}", std::process::id(), now_ms());
    let mut owned_token = None;
    if remote_available {
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
                while Instant::now() < deadline && now_ms() < expires_ms {
                    thread::sleep(Duration::from_millis(retry_after_ms.clamp(50, 1_000)));
                    if let Ok(index) = remote.candidates(&identity.static_key)
                        && let Some(status) =
                            try_candidates(&remote, l1.as_ref(), &invocation, &identity, &index)?
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
                record_event(
                    "fallback",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    None,
                    "single-flight wait timed out; compiling locally",
                );
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
            if let Some(token) = &owned_token {
                remote.release(&identity.static_key, token);
            }
            return Err(error);
        }
    };
    if !status.success() {
        if let Some(token) = &owned_token {
            remote.release(&identity.static_key, token);
        }
        return Ok(status);
    }
    if let Some(captured) = captured {
        if let Some(store) = &l1
            && let Err(error) = cache_captured(store, &captured)
        {
            record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("L1 publication failed: {error:#}"),
            );
        }
        if remote_available {
            match publish(&remote, captured) {
                Ok(action_key) => record_event(
                    "store",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&action_key),
                    "published compiler result",
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
    if let Some(token) = &owned_token {
        remote.release(&identity.static_key, token);
    }
    Ok(status)
}

fn passthrough(raw: &[OsString]) -> Result<ExitStatus> {
    let (rustc, args) = raw.split_first().context("missing rustc executable")?;
    Ok(Command::new(rustc).args(args).status()?)
}

fn normalizer(workspace: &Path, out_dir: &Path) -> PathNormalizer {
    let target = target_root(workspace, out_dir);
    let mut bases = vec![("$WORKSPACE".into(), workspace.to_path_buf())];
    if let Some(target) = target {
        bases.push(("$TARGET".into(), target));
    }
    if let Some(value) = env::var_os("CARGO_HOME") {
        bases.push(("$CARGO_HOME".into(), PathBuf::from(value)));
    }
    if let Some(value) = env::var_os("RUSTUP_HOME") {
        bases.push(("$RUSTUP_HOME".into(), PathBuf::from(value)));
    }
    if let Some(value) = env::var_os("HOME") {
        bases.push(("$HOME".into(), PathBuf::from(value)));
    }
    PathNormalizer::new(bases)
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
}

fn absolute_path(path: &Path, workspace: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

fn build_identity(invocation: &Invocation) -> Result<Identity> {
    let workspace = env::var_os("BELLOWS_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?)
        .canonicalize()?;
    let normalizer = normalizer(&workspace, &invocation.out_dir);
    let compiler = Command::new(&invocation.rustc).arg("-vV").output()?;
    if !compiler.status.success() {
        bail!("rustc -vV failed")
    }
    let mut hasher = blake3::Hasher::new();
    hash_field(
        &mut hasher,
        "protocol",
        PROTOCOL_VERSION.to_string().as_bytes(),
    );
    hash_field(&mut hasher, "compiler", &compiler.stdout);
    for arg in &invocation.args {
        hash_field(&mut hasher, "arg", normalizer.normalize(arg).as_bytes());
    }
    hash_field(&mut hasher, "remap", b"$WORKSPACE=/bellows/workspace");
    for (name, value) in relevant_environment(&normalizer) {
        hash_field(&mut hasher, &format!("env:{name}"), value.as_bytes());
    }
    let mut inputs = invocation.explicit_inputs.clone();
    inputs.sort();
    inputs.dedup();
    for path in inputs {
        let absolute = absolute_path(&path, &workspace)
            .canonicalize()
            .with_context(|| format!("canonicalize compiler input {}", path.display()))?;
        hash_field(
            &mut hasher,
            &format!(
                "input:{}",
                normalizer.normalize(&absolute.to_string_lossy())
            ),
            digest_file(&absolute)?.as_bytes(),
        );
    }
    Ok(Identity {
        static_key: hasher.finalize().to_hex().to_string(),
        normalizer,
        workspace,
    })
}

fn relevant_environment(normalizer: &PathNormalizer) -> BTreeMap<String, String> {
    env::vars()
        .filter(|(name, _)| is_relevant_environment_name(name))
        .map(|(name, value)| (name, normalizer.normalize(&value)))
        .collect()
}

fn is_relevant_environment_name(name: &str) -> bool {
    const EXCLUDED_CARGO_CONTROL: &[&str] = &[
        "CARGO_MAKEFLAGS",
        "CARGO_TARGET_TMPDIR",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
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
    ];
    const PREFIXES: &[&str] = &[
        "AR_", "CARGO_", "CC_", "CFLAGS", "CLIPPY_", "CPPFLAGS", "CXX_", "CXXFLAGS", "DEP_",
        "LDFLAGS", "RUST",
    ];
    const BELLOWS_CONTROL: &[&str] = &[
        "BELLOWS_AUTH_TOKEN",
        "BELLOWS_DEMO_COMPILE_DELAY_MS",
        "BELLOWS_EVENT_LOG",
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
    for input in &candidate.files {
        let localized = identity.normalizer.localize(&input.path);
        let path = absolute_path(Path::new(&localized), &identity.workspace);
        let actual =
            digest_file(&path).map_err(|_| format!("input disappeared: {}", input.path))?;
        if actual != input.digest {
            return Err(format!("input changed: {}", input.path));
        }
    }
    for input in &candidate.env {
        let value = env::var(&input.name)
            .ok()
            .map(|value| identity.normalizer.normalize(&value));
        let actual = EnvInput::capture(&input.name, value.as_deref());
        if actual.value_digest != input.value_digest {
            return Err(format!("environment changed: {}", input.name));
        }
    }
    Ok(())
}

fn explain_candidates(identity: &Identity, index: &CandidateIndex) -> String {
    if index.candidates.is_empty() {
        return "no remote candidate has this compiler/command/input identity".into();
    }
    index
        .candidates
        .iter()
        .filter_map(|candidate| validate_candidate(candidate, identity).err())
        .next()
        .unwrap_or_else(|| "candidate artifacts were unavailable or corrupt".into())
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
                    "restored remote compiler result",
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
        if validate_candidate(candidate, identity).is_err() {
            continue;
        }
        match restore_l1(store, invocation, identity, candidate) {
            Ok(()) => {
                record_event(
                    "l1_hit",
                    &invocation.crate_name,
                    Some(&identity.static_key),
                    Some(&candidate.action_key),
                    "restored runner-local compiler result",
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

fn restore(
    remote: &Remote,
    l1: Option<&Store>,
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
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
        if !invocation.expected_names.contains(&artifact.file_name) {
            bail!("manifest contains unexpected output {}", artifact.file_name)
        }
        let stored = remote.blob(&artifact.digest)?;
        downloaded.push((artifact, stored));
    }
    let stdout_blob = remote.blob(&candidate.stdout.digest)?;
    let stderr_blob = remote.blob(&candidate.stderr.digest)?;
    if stdout_blob.len() as u64 != candidate.stdout.len
        || stderr_blob.len() as u64 != candidate.stderr.len
    {
        bail!("compiler stream length does not match manifest")
    }
    for (artifact, stored) in downloaded {
        if let Some(store) = l1 {
            let _ = store.put_blob(&artifact.digest, &stored);
        }
        let bytes = if artifact.file_name.ends_with(".d") {
            identity.normalizer.localize_bytes(&stored)
        } else {
            stored
        };
        atomic_write(&invocation.out_dir.join(&artifact.file_name), &bytes)?;
        #[cfg(unix)]
        if artifact.executable {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                invocation.out_dir.join(&artifact.file_name),
                fs::Permissions::from_mode(0o755),
            )?;
        }
    }
    if let Some(store) = l1 {
        let _ = store.put_blob(&candidate.stdout.digest, &stdout_blob);
        let _ = store.put_blob(&candidate.stderr.digest, &stderr_blob);
    }
    let stdout = identity.normalizer.localize_bytes(&stdout_blob);
    let stderr = identity.normalizer.localize_bytes(&stderr_blob);
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

fn restore_l1(
    store: &Store,
    invocation: &Invocation,
    identity: &Identity,
    candidate: &ActionCandidate,
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
            bail!("L1 manifest is missing expected output {expected}")
        }
    }
    let mut downloaded = Vec::with_capacity(candidate.artifacts.len());
    for artifact in &candidate.artifacts {
        if !invocation.expected_names.contains(&artifact.file_name) {
            bail!(
                "L1 manifest contains unexpected output {}",
                artifact.file_name
            )
        }
        let stored = store.read_blob(&artifact.digest)?;
        downloaded.push((artifact, stored));
    }
    let stdout_blob = store.read_blob(&candidate.stdout.digest)?;
    let stderr_blob = store.read_blob(&candidate.stderr.digest)?;
    if stdout_blob.len() as u64 != candidate.stdout.len
        || stderr_blob.len() as u64 != candidate.stderr.len
    {
        bail!("L1 compiler stream length does not match manifest")
    }
    for (artifact, stored) in downloaded {
        let bytes = if artifact.file_name.ends_with(".d") {
            identity.normalizer.localize_bytes(&stored)
        } else {
            stored
        };
        atomic_write(&invocation.out_dir.join(&artifact.file_name), &bytes)?;
    }
    let stdout = identity.normalizer.localize_bytes(&stdout_blob);
    let stderr = identity.normalizer.localize_bytes(&stderr_blob);
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

struct Captured {
    candidate: ActionCandidate,
    blobs: BTreeMap<String, Vec<u8>>,
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
    let mut args = invocation.args.clone();
    args.push("--remap-path-prefix".into());
    args.push(format!(
        "{}=/bellows/workspace",
        identity.workspace.display()
    ));
    if let Some(target) = target_root(&identity.workspace, &invocation.out_dir) {
        args.push("--remap-path-prefix".into());
        args.push(format!("{}=/bellows/target", target.display()));
    }
    let mut child = Command::new(&invocation.rustc)
        .args(&args)
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

    let captured = match capture_outputs(invocation, identity, stdout, stderr) {
        Ok(captured) => Some(captured),
        Err(error) => {
            record_event(
                "fallback",
                &invocation.crate_name,
                Some(&identity.static_key),
                None,
                &format!("capture skipped after successful rustc: {error:#}"),
            );
            None
        }
    };
    Ok((status, captured))
}

fn capture_outputs(
    invocation: &Invocation,
    identity: &Identity,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<Captured> {
    let mut blobs = BTreeMap::new();
    let mut artifacts = Vec::new();
    let mut dep_text = None;
    for name in &invocation.expected_names {
        let path = invocation.out_dir.join(name);
        if !path.is_file() {
            if name.ends_with(".rmeta")
                && invocation
                    .expected_names
                    .iter()
                    .any(|n| n.ends_with(".rlib"))
            {
                continue;
            }
            bail!(
                "rustc succeeded but expected output {} is absent",
                path.display()
            )
        }
        let raw = fs::read(&path)?;
        let stored = if name.ends_with(".d") {
            let normalized = identity.normalizer.normalize_bytes(&raw);
            dep_text = Some(String::from_utf8(raw).context("dep-info is not UTF-8")?);
            normalized
        } else {
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
    file_paths.sort();
    file_paths.dedup();
    let mut files = Vec::new();
    for path in file_paths {
        let absolute = absolute_path(&path, &identity.workspace);
        if !absolute.is_file() {
            bail!(
                "rustc dependency disappeared before capture: {}",
                absolute.display()
            )
        }
        let absolute = absolute
            .canonicalize()
            .with_context(|| format!("canonicalize rustc dependency {}", absolute.display()))?;
        files.push(FileInput {
            path: identity.normalizer.normalize(&absolute.to_string_lossy()),
            digest: digest_file(&absolute)?,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    let mut env_inputs = dep_env
        .into_iter()
        .map(|(name, _)| {
            let value = env::var(&name)
                .ok()
                .map(|value| identity.normalizer.normalize(&value));
            EnvInput::capture(&name, value.as_deref())
        })
        .collect::<Vec<_>>();
    env_inputs.sort_by(|a, b| a.name.cmp(&b.name));
    env_inputs.dedup_by(|a, b| a.name == b.name);
    let normalized_stdout = identity.normalizer.normalize_bytes(&stdout);
    let normalized_stderr = identity.normalizer.normalize_bytes(&stderr);
    let stdout_digest = digest_bytes(&normalized_stdout);
    let stderr_digest = digest_bytes(&normalized_stderr);
    let stdout_len = normalized_stdout.len() as u64;
    let stderr_len = normalized_stderr.len() as u64;
    blobs.insert(stdout_digest.clone(), normalized_stdout);
    blobs.insert(stderr_digest.clone(), normalized_stderr);
    let action_key = compiler_action_key(&identity.static_key, &files, &env_inputs);
    let candidate = ActionCandidate {
        protocol: PROTOCOL_VERSION,
        static_key: identity.static_key.clone(),
        action_key,
        crate_name: invocation.crate_name.clone(),
        created_ms: now_ms(),
        files,
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
    };
    Ok(Captured { candidate, blobs })
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
    let event = Event {
        timestamp_ms: now_ms(),
        kind: kind.into(),
        crate_name: crate_name.into(),
        static_key: static_key.map(str::to_owned),
        action_key: action_key.map(str::to_owned),
        detail: detail.into(),
    };
    let path = event_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path)
        && let Ok(line) = serde_json::to_string(&event)
    {
        let _ = file.lock_exclusive();
        let _ = file.write_all(format!("{line}\n").as_bytes());
        let _ = file.unlock();
    }
    if matches!(
        kind,
        "hit" | "l1_hit" | "miss" | "bypass" | "fallback" | "wait" | "single_flight" | "corrupt"
    ) {
        eprintln!("bellows [{kind}] {crate_name}: {detail}");
    }
}

fn read_events() -> Result<Vec<Event>> {
    let path = event_log_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut file = fs::OpenOptions::new().read(true).open(path)?;
    file.lock_shared()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    file.unlock()?;
    let mut events = Vec::new();
    let mut corrupt = 0usize;
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(event) => events.push(event),
            Err(_) => corrupt += 1,
        }
    }
    if corrupt > 0 {
        eprintln!("bellows: ignored {corrupt} malformed event-log line(s)");
    }
    Ok(events)
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
    println!("✓ bellowsd {} ({})", health.version, server);
    println!("✓ protocol {}", health.protocol);
    println!("✓ {}", compiler.lines().next().unwrap_or("rustc"));
    println!("✓ local fallback enabled");
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
}

fn show_stats(server: &str, token: Option<&str>, json: bool) -> Result<()> {
    let remote = Remote::new(server, token.map(str::to_owned))?.stats()?;
    let mut events = BTreeMap::new();
    for event in read_events()? {
        *events.entry(event.kind).or_insert(0) += 1;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CombinedStats { remote, events })?
        );
    } else {
        println!("Bellows remote cache");
        println!("  compiler actions {:>8}", remote.candidates);
        println!("  declared actions {:>8}", remote.declared_actions);
        println!("  archives         {:>8}", remote.archives);
        println!("  blobs         {:>8}", remote.blobs);
        println!("  stored bytes  {:>8}", human_bytes(remote.blob_bytes));
        println!("  active leases {:>8}", remote.active_leases);
        println!("This workspace");
        for (kind, count) in events {
            println!("  {kind:<13} {count:>8}");
        }
    }
    Ok(())
}

fn explain(limit: usize, json: bool) -> Result<()> {
    let selected = read_events()?
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
        println!("No recent misses or bypasses.");
    } else {
        for event in selected {
            println!(
                "{} {:<18} {:<24} {}",
                event.timestamp_ms, event.kind, event.crate_name, event.detail
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
                "published archive {name} {} ({} files)",
                &manifest.tree_digest[..12],
                manifest.files.len()
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
                "restored archive {name} {} ({} files)",
                &manifest.tree_digest[..12],
                manifest.files.len()
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
    let environment = declared_environment(&args.environment)?;
    let key = declared_action_key(
        &args.name,
        &platform,
        &args.command,
        &environment,
        &inputs,
        &outputs,
    );
    let remote = Remote::new(&args.connection.server, args.connection.token)?;
    if let Some(record) = remote.declared(&key)? {
        restore_declared_record(&remote, &workspace, &record)?;
        println!("declared action HIT: {} ({})", args.name, &key[..12]);
        return Ok(());
    }

    for (artifact, bytes) in &inputs_with_bytes {
        remote.put_blob(&artifact.digest, bytes.clone())?;
    }
    let request = ExecuteRequest {
        key: key.clone(),
        name: args.name.clone(),
        platform: platform.clone(),
        command: args.command.clone(),
        environment: environment.clone(),
        inputs: inputs.clone(),
        outputs: outputs.clone(),
    };
    let record = if remote_execution {
        let response = remote.execute(&request)?;
        println!(
            "remote execution {}: {} ({})",
            if response.cache_hit {
                "HIT"
            } else {
                "EXECUTED"
            },
            args.name,
            &key[..12]
        );
        response.record
    } else {
        let (record, blobs) = execute_local_declared(&workspace, request, &inputs_with_bytes)?;
        for (digest, bytes) in blobs {
            remote.put_blob(&digest, bytes)?;
        }
        remote.put_declared(&record)?;
        println!("declared action MISS: {} ({})", args.name, &key[..12]);
        record
    };
    restore_declared_record(&remote, &workspace, &record)?;
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
        let relative = validate_relative_path(&artifact.file_name)?;
        let bytes = remote.blob(&artifact.digest)?;
        staged.push((artifact, relative, bytes));
    }
    let stdout = remote.blob(&record.stdout.digest)?;
    let stderr = remote.blob(&record.stderr.digest)?;
    if stdout.len() as u64 != record.stdout.len || stderr.len() as u64 != record.stderr.len {
        bail!("declared stream length does not match manifest")
    }
    for (artifact, relative, bytes) in staged {
        let destination = safe_destination(workspace, &relative)?;
        atomic_write(&destination, &bytes)?;
        set_file_executable(&destination, artifact.executable)?;
    }
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
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
        "captured workspace snapshot {name} ({} packages)",
        snapshot.packages.len()
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
        println!("Bellows compiler-aware impact: {before} -> {after}");
        println!(
            "  source changed: {}",
            display_names(&report.source_changed)
        );
        println!(
            "  syntactic surface changed: {}",
            display_names(&report.syntactic_surface_changed)
        );
        println!(
            "  private/relink research candidates: {}",
            display_names(&report.private_implementation_candidates)
        );
        println!(
            "  affected downstream: {}",
            display_names(&report.affected_downstream)
        );
        println!("  caveat: {}", report.caveat);
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

    #[test]
    fn prior_protocol_candidates_are_cleanly_rejected() {
        let workspace = std::env::temp_dir().join(format!("bellows-protocol-test-{}", now_ms()));
        let identity = Identity {
            static_key: digest_bytes(b"identity"),
            normalizer: PathNormalizer::new(vec![("$WORKSPACE".into(), workspace.clone())]),
            workspace,
        };
        let candidate = ActionCandidate {
            protocol: PROTOCOL_VERSION - 1,
            static_key: identity.static_key.clone(),
            action_key: digest_bytes(b"action"),
            crate_name: "fixture".into(),
            created_ms: 0,
            files: vec![],
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
        };
        assert_eq!(
            validate_candidate(&candidate, &identity).unwrap_err(),
            format!("unsupported candidate protocol {}", PROTOCOL_VERSION - 1)
        );
    }

    #[test]
    fn captured_stream_lengths_describe_normalized_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("a-very-long-workspace-name");
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
            format!("{rmeta_name}: {}/src/../src/lib.rs\n", workspace.display()),
        )
        .unwrap();
        let invocation = Invocation {
            rustc: PathBuf::from("rustc"),
            args: vec![],
            crate_name: "fixture".into(),
            out_dir: out_dir.clone(),
            expected_names: BTreeSet::from([dep_name.into(), rmeta_name.into()]),
            explicit_inputs: vec![source],
        };
        let identity = Identity {
            static_key: digest_bytes(b"identity"),
            normalizer: PathNormalizer::new(vec![("$WORKSPACE".into(), workspace.clone())]),
            workspace: workspace.clone(),
        };
        let stdout = format!("compiled {}", workspace.display()).into_bytes();
        let stderr = format!("warning in {}", workspace.display()).into_bytes();
        let captured =
            capture_outputs(&invocation, &identity, stdout.clone(), stderr.clone()).unwrap();
        assert_eq!(captured.candidate.files[0].path, "$WORKSPACE/src/lib.rs");
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

    #[test]
    fn link_arguments_do_not_make_linked_crates_cacheable() {
        for crate_type in ["bin", "rlib,cdylib"] {
            let raw = vec![
                OsString::from("rustc"),
                OsString::from("--crate-name=demo"),
                OsString::from(format!("--crate-type={crate_type}")),
                OsString::from("-C"),
                OsString::from("link-arg=-fuse-ld=lld"),
            ];
            assert!(Invocation::analyze(&raw).unwrap_err().contains("linked"));
        }
        let repeated = vec![
            OsString::from("rustc"),
            OsString::from("--crate-name=demo"),
            OsString::from("--crate-type"),
            OsString::from("rlib"),
            OsString::from("--crate-type"),
            OsString::from("cdylib"),
            OsString::from("-C"),
            OsString::from("link-arg=-fuse-ld=lld"),
        ];
        assert!(
            Invocation::analyze(&repeated)
                .unwrap_err()
                .contains("linked crate type rlib,cdylib")
        );
        let default_bin = vec![
            OsString::from("rustc"),
            OsString::from("--crate-name=demo"),
            OsString::from("-Clink-arg=-fuse-ld=lld"),
        ];
        assert!(
            Invocation::analyze(&default_bin)
                .unwrap_err()
                .contains("linked")
        );
    }

    #[test]
    fn bypasses_linked_and_incremental_invocations() {
        let bin = vec![
            OsString::from("rustc"),
            OsString::from("--crate-name"),
            OsString::from("demo"),
            OsString::from("src/main.rs"),
            OsString::from("--crate-type"),
            OsString::from("bin"),
        ];
        assert!(Invocation::analyze(&bin).unwrap_err().contains("linked"));
        let incremental = vec![
            OsString::from("rustc"),
            OsString::from("-C"),
            OsString::from("incremental=/tmp/x"),
        ];
        assert!(
            Invocation::analyze(&incremental)
                .unwrap_err()
                .contains("incremental")
        );
    }

    #[test]
    fn identifies_unmodeled_native_inputs() {
        assert!(has_native_or_external_codegen_inputs(&[
            "-L".into(),
            "native=/opt/sdk".into()
        ]));
        assert!(has_native_or_external_codegen_inputs(&[
            "-C".into(),
            "profile-use=/tmp/default.profdata".into()
        ]));
        for input in [
            vec!["-l".into(), "foo".into()],
            vec!["-lstatic=foo".into()],
            vec!["-Lnative=/opt/sdk".into()],
            vec!["-C".into(), "linker-plugin-lto=/opt/plugin".into()],
            vec!["-Cllvm-plugins=/opt/plugin".into()],
            vec!["-Clinker=mold".into()],
        ] {
            assert!(has_native_or_external_codegen_inputs(&input));
        }
        assert!(!has_native_or_external_codegen_inputs(&[
            "-L".into(),
            "dependency=/tmp/target".into()
        ]));
        assert!(!has_native_or_external_codegen_inputs(&[
            "-C".into(),
            "link-arg=-fuse-ld=lld".into()
        ]));
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

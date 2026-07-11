use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 2;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn digest_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn digest_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(digest_bytes(&bytes))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileInput {
    pub path: String,
    pub digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvInput {
    pub name: String,
    pub value_digest: Option<String>,
}

impl EnvInput {
    pub fn capture(name: impl Into<String>, value: Option<&str>) -> Self {
        Self {
            name: name.into(),
            value_digest: value.map(|v| digest_bytes(v.as_bytes())),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub file_name: String,
    pub digest: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamArtifact {
    pub digest: String,
    pub len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionCandidate {
    pub protocol: u32,
    pub static_key: String,
    pub action_key: String,
    pub crate_name: String,
    pub created_ms: u64,
    pub files: Vec<FileInput>,
    pub env: Vec<EnvInput>,
    pub artifacts: Vec<Artifact>,
    pub stdout: StreamArtifact,
    pub stderr: StreamArtifact,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateIndex {
    pub candidates: Vec<ActionCandidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub client_id: String,
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LeaseResponse {
    Owned {
        token: String,
        expires_ms: u64,
    },
    Wait {
        retry_after_ms: u64,
        expires_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub service: String,
    pub protocol: u32,
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerStats {
    pub blobs: u64,
    pub blob_bytes: u64,
    pub action_indexes: u64,
    pub candidates: u64,
    pub active_leases: u64,
    #[serde(default)]
    pub declared_actions: u64,
    #[serde(default)]
    pub archives: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GcReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub records_evicted: u64,
    pub blobs_evicted: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcRequest {
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlatformIdentity {
    pub rustc: String,
    pub cargo: String,
    pub os: String,
    pub arch: String,
    pub path_digest: String,
    pub rustup_home_digest: String,
    pub rustup_toolchain: Option<String>,
    pub host_cc: Option<String>,
    pub host_ld: Option<String>,
    pub host_ar: Option<String>,
}

impl PlatformIdentity {
    pub fn detect() -> Result<Self> {
        Ok(Self {
            rustc: command_version("rustc")?,
            cargo: command_version("cargo")?,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            path_digest: digest_bytes(std::env::var("PATH").unwrap_or_default().as_bytes()),
            rustup_home_digest: digest_bytes(rustup_home().to_string_lossy().as_bytes()),
            rustup_toolchain: std::env::var("RUSTUP_TOOLCHAIN")
                .ok()
                .or_else(active_rustup_toolchain),
            host_cc: optional_command_version("cc"),
            host_ld: optional_command_version("ld"),
            host_ar: optional_command_version("ar"),
        })
    }
}

fn optional_command_version(command: &str) -> Option<String> {
    let output = Command::new(command).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut identity = output.stdout;
    identity.extend_from_slice(&output.stderr);
    String::from_utf8(identity)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn active_rustup_toolchain() -> Option<String> {
    let output = Command::new("rustup")
        .args(["show", "active-toolchain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

pub fn rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))
        .unwrap_or_else(|| PathBuf::from(".rustup"))
}

fn command_version(command: &str) -> Result<String> {
    let output = Command::new(command)
        .arg("-vV")
        .output()
        .with_context(|| format!("run {command} -vV"))?;
    if !output.status.success() {
        bail!("{command} -vV failed")
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredActionRecord {
    pub protocol: u32,
    pub key: String,
    pub name: String,
    pub created_ms: u64,
    pub platform: PlatformIdentity,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<Artifact>,
    pub output_paths: Vec<String>,
    pub outputs: Vec<Artifact>,
    pub stdout: StreamArtifact,
    pub stderr: StreamArtifact,
    pub duration_ms: u64,
    pub executor: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveManifest {
    pub protocol: u32,
    pub name: String,
    pub tree_digest: String,
    pub created_ms: u64,
    pub producer_action: Option<String>,
    pub files: Vec<Artifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub key: String,
    pub name: String,
    pub platform: PlatformIdentity,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<Artifact>,
    pub outputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub cache_hit: bool,
    pub record: DeclaredActionRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub timestamp_ms: u64,
    pub kind: String,
    pub crate_name: String,
    pub static_key: Option<String>,
    pub action_key: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("actions"))?;
        fs::create_dir_all(root.join("declared"))?;
        fs::create_dir_all(root.join("archives"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn checked_key<'a>(&self, key: &'a str) -> Result<&'a str> {
        if key.len() < 8 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid content key")
        }
        Ok(key)
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        let digest = self.checked_key(digest)?;
        Ok(self.root.join("blobs").join(&digest[..2]).join(digest))
    }

    pub fn action_path(&self, static_key: &str) -> Result<PathBuf> {
        let key = self.checked_key(static_key)?;
        Ok(self
            .root
            .join("actions")
            .join(&key[..2])
            .join(format!("{key}.json")))
    }

    pub fn declared_path(&self, key: &str) -> Result<PathBuf> {
        let key = self.checked_key(key)?;
        Ok(self
            .root
            .join("declared")
            .join(&key[..2])
            .join(format!("{key}.json")))
    }

    pub fn archive_path(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            bail!("invalid archive name")
        }
        Ok(self.root.join("archives").join(format!("{name}.json")))
    }

    pub fn put_blob(&self, expected: &str, bytes: &[u8]) -> Result<bool> {
        let actual = digest_bytes(bytes);
        if actual != expected {
            bail!("blob digest mismatch: expected {expected}, got {actual}")
        }
        let path = self.blob_path(expected)?;
        if path.exists() {
            if self.read_blob(expected).is_ok() {
                return Ok(false);
            }
            fs::remove_file(&path).with_context(|| format!("remove corrupt blob {expected}"))?;
        }
        let parent = path.parent().context("blob path has no parent")?;
        fs::create_dir_all(parent)?;
        atomic_write(&path, bytes)?;
        Ok(true)
    }

    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        let bytes = fs::read(&path).with_context(|| format!("read blob {digest}"))?;
        let actual = digest_bytes(&bytes);
        if actual != digest {
            bail!("corrupt blob {digest}: content hashes to {actual}")
        }
        Ok(bytes)
    }

    pub fn read_candidates(&self, static_key: &str) -> Result<CandidateIndex> {
        let path = self.action_path(static_key)?;
        if !path.exists() {
            return Ok(CandidateIndex::default());
        }
        let bytes = fs::read(&path)?;
        let index: CandidateIndex =
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
        Ok(index)
    }

    pub fn put_candidate(&self, candidate: ActionCandidate, max_candidates: usize) -> Result<()> {
        if candidate.protocol != PROTOCOL_VERSION {
            bail!("unsupported candidate protocol {}", candidate.protocol)
        }
        let path = self.action_path(&candidate.static_key)?;
        fs::create_dir_all(path.parent().context("action path has no parent")?)?;
        let mut index = self.read_candidates(&candidate.static_key)?;
        index
            .candidates
            .retain(|c| c.action_key != candidate.action_key);
        index.candidates.insert(0, candidate);
        index.candidates.truncate(max_candidates.max(1));
        atomic_write(&path, &serde_json::to_vec_pretty(&index)?)
    }

    pub fn read_declared(&self, key: &str) -> Result<Option<DeclaredActionRecord>> {
        let path = self.declared_path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
    }

    pub fn put_declared(&self, record: &DeclaredActionRecord) -> Result<bool> {
        let path = self.declared_path(&record.key)?;
        if let Some(existing) = self.read_declared(&record.key)? {
            if existing == *record {
                return Ok(false);
            }
            bail!("declared action key already exists with different content")
        }
        fs::create_dir_all(path.parent().context("declared path has no parent")?)?;
        atomic_write(&path, &serde_json::to_vec_pretty(record)?)?;
        Ok(true)
    }

    pub fn read_archive(&self, name: &str) -> Result<Option<ArchiveManifest>> {
        let path = self.archive_path(name)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
    }

    pub fn put_archive(&self, manifest: &ArchiveManifest) -> Result<bool> {
        let path = self.archive_path(&manifest.name)?;
        if let Some(existing) = self.read_archive(&manifest.name)? {
            if existing.tree_digest == manifest.tree_digest {
                return Ok(false);
            }
            bail!("archive name is already bound to a different tree")
        }
        atomic_write(&path, &serde_json::to_vec_pretty(manifest)?)?;
        Ok(true)
    }

    pub fn gc(&self, max_bytes: u64) -> Result<GcReport> {
        let mut report = GcReport {
            bytes_before: directory_bytes(&self.root.join("blobs"))?,
            ..GcReport::default()
        };
        let mut records = Vec::new();
        for directory in ["actions", "declared", "archives"] {
            collect_paths(&self.root.join(directory), &mut records)?;
        }
        records.sort_by_key(|path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH)
        });
        remove_unreferenced_blobs(self, &mut report)?;
        let mut current = directory_bytes(&self.root.join("blobs"))?;
        for record in records {
            if current <= max_bytes {
                break;
            }
            if record.exists() {
                fs::remove_file(&record)?;
                report.records_evicted += 1;
                remove_unreferenced_blobs(self, &mut report)?;
                current = directory_bytes(&self.root.join("blobs"))?;
            }
        }
        if current > max_bytes {
            let mut blobs = Vec::new();
            collect_paths(&self.root.join("blobs"), &mut blobs)?;
            blobs.sort_by_key(|path| {
                fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH)
            });
            for blob in blobs {
                if current <= max_bytes {
                    break;
                }
                let len = fs::metadata(&blob)?.len();
                fs::remove_file(blob)?;
                report.blobs_evicted += 1;
                current = current.saturating_sub(len);
            }
        }
        report.bytes_after = directory_bytes(&self.root.join("blobs"))?;
        Ok(report)
    }
}

fn remove_unreferenced_blobs(store: &Store, report: &mut GcReport) -> Result<()> {
    let referenced = referenced_blobs(store)?;
    let mut blobs = Vec::new();
    collect_paths(&store.root.join("blobs"), &mut blobs)?;
    for blob in blobs {
        let Some(name) = blob.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !referenced.contains(name) {
            fs::remove_file(blob)?;
            report.blobs_evicted += 1;
        }
    }
    Ok(())
}

fn referenced_blobs(store: &Store) -> Result<BTreeSet<String>> {
    let mut referenced = BTreeSet::new();
    let mut files = Vec::new();
    collect_paths(&store.root.join("actions"), &mut files)?;
    for path in files {
        if let Ok(index) = serde_json::from_slice::<CandidateIndex>(&fs::read(path)?) {
            for candidate in index.candidates {
                referenced.extend(
                    candidate
                        .artifacts
                        .into_iter()
                        .map(|artifact| artifact.digest),
                );
                referenced.insert(candidate.stdout.digest);
                referenced.insert(candidate.stderr.digest);
            }
        }
    }
    files = Vec::new();
    collect_paths(&store.root.join("declared"), &mut files)?;
    for path in files {
        if let Ok(record) = serde_json::from_slice::<DeclaredActionRecord>(&fs::read(path)?) {
            referenced.extend(record.inputs.into_iter().map(|artifact| artifact.digest));
            referenced.extend(record.outputs.into_iter().map(|artifact| artifact.digest));
            referenced.insert(record.stdout.digest);
            referenced.insert(record.stderr.digest);
        }
    }
    files = Vec::new();
    collect_paths(&store.root.join("archives"), &mut files)?;
    for path in files {
        if let Ok(manifest) = serde_json::from_slice::<ArchiveManifest>(&fs::read(path)?) {
            referenced.extend(manifest.files.into_iter().map(|artifact| artifact.digest));
        }
    }
    Ok(referenced)
}

fn collect_paths(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_paths(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn directory_bytes(root: &Path) -> Result<u64> {
    let mut files = Vec::new();
    collect_paths(root, &mut files)?;
    files.into_iter().try_fold(0u64, |total, path| {
        Ok(total.saturating_add(fs::metadata(path)?.len()))
    })
}

pub fn validate_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("path must be a non-empty relative path: {value}")
    }
    if value
        .split(['/', '\\'])
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        bail!("path contains a forbidden segment: {value}")
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("path contains a forbidden component: {value}")
        }
    }
    Ok(path.to_path_buf())
}

pub fn tree_digest(files: &[Artifact]) -> String {
    let mut files = files.to_vec();
    files.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    digest_bytes(&serde_json::to_vec(&files).unwrap_or_default())
}

pub fn declared_action_key(
    name: &str,
    platform: &PlatformIdentity,
    command: &[String],
    environment: &BTreeMap<String, String>,
    inputs: &[Artifact],
    outputs: &[String],
) -> String {
    let mut inputs = inputs.to_vec();
    inputs.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    let mut outputs = outputs.to_vec();
    outputs.sort();
    digest_bytes(
        &serde_json::to_vec(&(name, platform, command, environment, inputs, outputs))
            .unwrap_or_default(),
    )
}

pub fn validate_declared_command(command: &[String]) -> Result<()> {
    let program = command.first().context("declared command is empty")?;
    if !matches!(program.as_str(), "cargo" | "rustc") {
        bail!("cacheable declared command must use bare cargo or rustc")
    }
    if program == "cargo"
        && (!command.iter().any(|argument| argument == "--locked")
            || !command.iter().any(|argument| argument == "--offline"))
    {
        bail!("cacheable Cargo commands require --locked --offline")
    }
    for argument in &command[1..] {
        let payload = argument
            .split_once('=')
            .map_or(argument.as_str(), |(_, value)| value);
        let windows_absolute =
            payload.as_bytes().get(1) == Some(&b':') || payload.starts_with("\\\\");
        if Path::new(payload).is_absolute()
            || windows_absolute
            || payload.split(['/', '\\']).any(|segment| segment == "..")
        {
            bail!("declared command argument escapes the sandbox: {argument}")
        }
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        now_ms(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) if path.exists() => {
            let _ = fs::remove_file(&tmp);
            match fs::read(path) {
                Ok(existing) if existing == bytes => Ok(()),
                _ => Err(error.into()),
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error.into())
        }
    }
}

#[derive(Clone, Debug)]
pub struct PathNormalizer {
    bases: Vec<(String, String)>,
}

impl PathNormalizer {
    pub fn new(mut bases: Vec<(String, PathBuf)>) -> Self {
        let mut rendered = Vec::new();
        for (token, path) in bases.drain(..) {
            let value = path.to_string_lossy().trim_end_matches('/').to_owned();
            if !value.is_empty() && !rendered.iter().any(|(_, p)| p == &value) {
                rendered.push((token, value));
            }
        }
        rendered.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        Self { bases: rendered }
    }

    pub fn normalize(&self, value: &str) -> String {
        self.bases
            .iter()
            .fold(value.to_owned(), |text, (token, path)| {
                text.replace(path, token)
            })
    }

    pub fn localize(&self, value: &str) -> String {
        self.bases
            .iter()
            .fold(value.to_owned(), |text, (token, path)| {
                text.replace(token, path)
            })
    }

    pub fn normalize_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        replace_bytes(
            bytes,
            &self
                .bases
                .iter()
                .map(|(t, p)| (p.as_bytes(), t.as_bytes()))
                .collect::<Vec<_>>(),
        )
    }

    pub fn localize_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        replace_bytes(
            bytes,
            &self
                .bases
                .iter()
                .map(|(t, p)| (t.as_bytes(), p.as_bytes()))
                .collect::<Vec<_>>(),
        )
    }
}

fn replace_bytes(input: &[u8], replacements: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut value = input.to_vec();
    for (from, to) in replacements {
        if from.is_empty() {
            continue;
        }
        let mut output = Vec::with_capacity(value.len());
        let mut offset = 0;
        while let Some(pos) = value[offset..].windows(from.len()).position(|w| w == *from) {
            let absolute = offset + pos;
            output.extend_from_slice(&value[offset..absolute]);
            output.extend_from_slice(to);
            offset = absolute + from.len();
        }
        output.extend_from_slice(&value[offset..]);
        value = output;
    }
    value
}

pub fn parse_dep_info(text: &str) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let mut files = BTreeSet::new();
    let mut env = BTreeSet::new();
    let logical = text.replace("\\\n", "");
    for line in logical.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            let (name, value) = rest
                .split_once('=')
                .map_or((rest, None), |(name, value)| (name, Some(value.to_owned())));
            env.insert((name.to_owned(), value));
            continue;
        }
        let Some((_, dependencies)) = line.split_once(": ") else {
            continue;
        };
        for word in split_makefile_words(dependencies) {
            if !word.is_empty() {
                files.insert(word);
            }
        }
    }
    (files.into_iter().collect(), env.into_iter().collect())
}

fn split_makefile_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_longest_base_first_and_round_trips() {
        let n = PathNormalizer::new(vec![
            ("$HOME".into(), PathBuf::from("/home/alice")),
            ("$WORKSPACE".into(), PathBuf::from("/home/alice/code/app")),
        ]);
        let normalized = n.normalize("/home/alice/code/app/src/lib.rs:/home/alice/.cargo");
        assert_eq!(normalized, "$WORKSPACE/src/lib.rs:$HOME/.cargo");
        assert_eq!(
            n.localize(&normalized),
            "/home/alice/code/app/src/lib.rs:/home/alice/.cargo"
        );
    }

    #[test]
    fn parses_files_and_environment_from_dep_info() {
        let dep = "target/foo.rlib: src/lib.rs src/a\\ b.rs \\\n src/nested.rs\n\n# env-dep:MODE=fast\n# env-dep:OPTIONAL\n";
        let (files, env) = parse_dep_info(dep);
        assert_eq!(files, vec!["src/a b.rs", "src/lib.rs", "src/nested.rs"]);
        assert_eq!(
            env,
            vec![
                ("MODE".into(), Some("fast".into())),
                ("OPTIONAL".into(), None)
            ]
        );
    }

    #[test]
    fn store_rejects_corrupt_blob() {
        let root = std::env::temp_dir().join(format!("bellows-core-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let digest = digest_bytes(b"good");
        store.put_blob(&digest, b"good").unwrap();
        fs::write(store.blob_path(&digest).unwrap(), b"bad").unwrap();
        assert!(
            store
                .read_blob(&digest)
                .unwrap_err()
                .to_string()
                .contains("corrupt")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_archive_traversal_paths() {
        assert!(validate_relative_path("bin/test").is_ok());
        assert!(validate_relative_path("../escape").is_err());
        assert!(validate_relative_path("/absolute").is_err());
        assert!(validate_relative_path("a/./b").is_err());
    }

    #[test]
    fn archive_names_are_publish_once() {
        let root = std::env::temp_dir().join(format!("bellows-archive-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let first = ArchiveManifest {
            protocol: PROTOCOL_VERSION,
            name: "tests-v1".into(),
            tree_digest: digest_bytes(b"one"),
            created_ms: 1,
            producer_action: None,
            files: vec![],
        };
        assert!(store.put_archive(&first).unwrap());
        assert!(!store.put_archive(&first).unwrap());
        let mut second = first.clone();
        second.tree_digest = digest_bytes(b"two");
        assert!(store.put_archive(&second).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn declared_keys_ignore_manifest_order() {
        let platform = PlatformIdentity {
            rustc: "rustc test".into(),
            cargo: "cargo test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            path_digest: digest_bytes(b"path"),
            rustup_home_digest: digest_bytes(b"rustup"),
            rustup_toolchain: None,
            host_cc: None,
            host_ld: None,
            host_ar: None,
        };
        let first = Artifact {
            file_name: "a".into(),
            digest: digest_bytes(b"a"),
            executable: false,
        };
        let second = Artifact {
            file_name: "b".into(),
            digest: digest_bytes(b"b"),
            executable: false,
        };
        let one = declared_action_key(
            "demo",
            &platform,
            &["cargo".into()],
            &BTreeMap::new(),
            &[first.clone(), second.clone()],
            &["out/a".into(), "out/b".into()],
        );
        let two = declared_action_key(
            "demo",
            &platform,
            &["cargo".into()],
            &BTreeMap::new(),
            &[second, first],
            &["out/b".into(), "out/a".into()],
        );
        assert_eq!(one, two);
    }

    #[test]
    fn gc_removes_records_before_referenced_blobs() {
        let root = std::env::temp_dir().join(format!("bellows-gc-test-{}", now_ms()));
        let store = Store::open(&root).unwrap();
        let bytes = b"archive payload";
        let digest = digest_bytes(bytes);
        store.put_blob(&digest, bytes).unwrap();
        store
            .put_archive(&ArchiveManifest {
                protocol: PROTOCOL_VERSION,
                name: "gc-test".into(),
                tree_digest: tree_digest(&[Artifact {
                    file_name: "payload".into(),
                    digest: digest.clone(),
                    executable: false,
                }]),
                created_ms: now_ms(),
                producer_action: None,
                files: vec![Artifact {
                    file_name: "payload".into(),
                    digest: digest.clone(),
                    executable: false,
                }],
            })
            .unwrap();
        let report = store.gc(0).unwrap();
        assert_eq!(report.bytes_after, 0);
        assert!(store.read_archive("gc-test").unwrap().is_none());
        assert!(store.read_blob(&digest).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn declared_commands_cannot_escape_the_sandbox() {
        assert!(
            validate_declared_command(&[
                "cargo".into(),
                "build".into(),
                "--locked".into(),
                "--offline".into(),
                "--manifest-path=fixture/Cargo.toml".into(),
            ])
            .is_ok()
        );
        for command in [
            vec![
                "cargo".into(),
                "build".into(),
                "--locked".into(),
                "--offline".into(),
                "--manifest-path=../../Cargo.toml".into(),
            ],
            vec!["rustc".into(), "/tmp/ambient.rs".into()],
            vec!["/tmp/cargo".into(), "--locked".into(), "--offline".into()],
        ] {
            assert!(validate_declared_command(&command).is_err());
        }
    }
}

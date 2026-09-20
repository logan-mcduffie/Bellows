//! Preserve Cargo's flag selection: append remapping at rustc invocation time,
//! rather than replacing RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS.
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitStatus};

pub const REMAP_ENV: &str = "BELLOWS_DECLARED_REMAP";
const INNER_WRAPPER_ENV: &str = "BELLOWS_DECLARED_INNER_WRAPPER";

// These are needed for Windows SDK/linker discovery and a writable temp root
// after env_clear(). They are included in PlatformIdentity's environment digest
// so declared results cannot cross differing host-tool configurations.
pub fn platform_environment() -> BTreeMap<String, String> {
    #[cfg(windows)]
    {
        [
            "SystemRoot",
            "WINDIR",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
            "ALLUSERSPROFILE",
            "CommonProgramFiles",
            "CommonProgramFiles(x86)",
            "CommonProgramW6432",
            "SystemDrive",
            "VCINSTALLDIR",
            "VSINSTALLDIR",
            "VisualStudioVersion",
            "VSCMD_ARG_TGT_ARCH",
            "VSCMD_ARG_VCVARS_SPECTRE",
            "VCToolsVersion",
            "WindowsSdkDir",
            "WindowsSDKVersion",
            "LIB",
            "LIBPATH",
            "INCLUDE",
            "TEMP",
            "TMP",
        ]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.into(), value)))
        .collect()
    }
    #[cfg(not(windows))]
    BTreeMap::new()
}

pub fn platform_path_digest() -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    #[cfg(windows)]
    {
        crate::digest_bytes(&serde_json::to_vec(&(path, platform_environment())).unwrap())
    }
    #[cfg(not(windows))]
    crate::digest_bytes(path.as_bytes())
}

pub fn configure_cargo_remapping(
    command: &mut Command,
    workspace: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    command
        .env("RUSTC_WRAPPER", std::env::current_exe()?)
        .env(REMAP_ENV, workspace)
        .env_remove(INNER_WRAPPER_ENV);
    if let Some(wrapper) = environment.get("RUSTC_WRAPPER").filter(|s| !s.is_empty()) {
        command.env(INNER_WRAPPER_ENV, wrapper);
    }
    Ok(())
}

pub fn remap_compiler(raw: &[OsString]) -> Result<ExitStatus> {
    let (compiler, args) = raw.split_first().context("missing declared compiler")?;
    let workspace = std::env::var_os(REMAP_ENV).context("missing declared remap root")?;
    let mut command = if let Some(wrapper) = std::env::var_os(INNER_WRAPPER_ENV) {
        let mut command = Command::new(wrapper);
        command.arg(compiler);
        command
    } else {
        Command::new(compiler)
    };
    command
        .args(args)
        .env_remove(REMAP_ENV)
        .env_remove(INNER_WRAPPER_ENV);
    // Probes need no remapping, and some tools require an exact probe command.
    if !args.iter().any(|arg| {
        arg == "-vV" || arg == "--version" || arg.to_string_lossy().starts_with("--print")
    }) {
        let mut mapping = workspace;
        mapping.push("=/bellows/action");
        command.arg("--remap-path-prefix").arg(mapping);
    }
    command.status().context("run declared compiler")
}

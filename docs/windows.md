# Native Windows validation

The Windows CI job runs on Windows Server 2022, x64 MSVC, Rust 1.92.0. A WSL
run exercises Linux filesystem and process behavior and does not replace this
native Windows check. Windows GNU, ARM64, network shares, and arbitrary declared
remote execution are not covered by this x64 qualification.

## Run on your PC

Install Git, PowerShell 7.2 or newer, Rustup, and the Microsoft C++ build tools
with a Windows SDK. See the [Rustup MSVC prerequisites](https://rust-lang.github.io/rustup/installation/windows-msvc.html)
for the compiler/linker components. Use a normal PowerShell 7 window; the tests
use NTFS junctions and do not require enabling symlink privileges.

```powershell
git clone --branch codex/production-readiness-20260919 https://github.com/logan-mcduffie/Bellows.git
cd Bellows
rustup toolchain install 1.92.0-x86_64-pc-windows-msvc --profile minimal --component rustfmt,clippy
pwsh -NoProfile -File .\scripts\windows-check.ps1
```

The script runs formatting, strict Clippy, the Rust tests, and a release build.
It then creates a separate workspace and cache with spaces in their paths and
checks executable output through cold/local cache hits, a source edit, cold/L1/
remote service use, service restart, and offline fallback. The functional suite
also checks environment and flag changes, output restoration, diagnostic
rotation/concurrent writers, lease reacquisition, and NTFS junction handling.
The Linux-only C-archive fixture is not run on Windows.

The script restores its temporary environment changes, stops its test daemon,
and prints the retained report directory on success or failure. `checks.log`,
`results.json`, per-phase JSONL diagnostics, and server logs are saved there.
To choose a report location, pass `-ReportDirectory C:\bellows-report`; use a
new directory for each run. CI uploads the same evidence as an artifact.
A failing native command stops qualification and returns failure.

The additional `scripts/windows-extended.ps1 -ReportDirectory <new-directory>`
suite exercises native archives, declared local/nested actions, authenticated
remote execution in a separate disposable service, compiler and execution
single-flight, bounded lease waits, corrupt-cache recovery, advisory analysis,
and non-ASCII paths. It requires the release binaries from `windows-check.ps1`
and `nightly-2026-01-15` for a real compiler-version invalidation comparison.
CI runs both suites and retains their command logs and machine-readable results.
Its remote executor is temporary; this does not enable execution on daily-use
services or qualify arbitrary host tools.

## Exercise Manifold on the actual machine

After this check passes, point your usual Manifold workflow at the built
`target\release\bellows.exe`. Use a disposable Manifold worktree and a dedicated
cache/target directory for a baseline/cold/warm comparison, preserving the same
compiler, flags, feature set, and source revision. Keep exact-diagnostic contract
lanes wrapper-free as required by Manifold. Compare test outcomes and runtime
behavior, not only cache-hit totals.

For dev-profile cache testing, set `CARGO_INCREMENTAL=0` in the test shell;
incremental invocations intentionally bypass Bellows. Final executables and
procedural macro consumers also bypass where their inputs are not modeled.
Run `bellows.exe explain --local --latest --json` from the workspace to inspect
miss reasons. Prefer a local NTFS cache; this pass does not qualify SMB locking
or antivirus-specific behavior on your machine.

Windows paths and Rustup home discovery received fixes in this pass. The
compiler identity now includes a dependency-parser revision so compiler records
captured by the earlier backslash parser cannot be reused. Expect an initial
cold compiler build after updating; the protocol remains version 5.

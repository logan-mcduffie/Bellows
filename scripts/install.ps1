#requires -Version 7.2
param(
    [string]$InstallRoot = $(if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }),
    [switch]$IncludeServer
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
foreach ($program in @('rustup', 'cargo')) {
    if (-not (Get-Command $program -ErrorAction SilentlyContinue)) {
        throw 'Install Rust with rustup from https://rustup.rs, then reopen the terminal and run this script again.'
    }
}
if ($IsWindows) {
    $finder = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    $installation = if (Test-Path $finder) { & $finder -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath }
    if (-not $installation) {
        throw 'Install Visual Studio Build Tools with Desktop development with C++ and a Windows SDK, then run this script again.'
    }
}
$InstallRoot = [IO.Path]::GetFullPath($InstallRoot)
& rustup toolchain install 1.92.0 --profile minimal
if ($LASTEXITCODE -ne 0) { throw 'Rust toolchain installation failed.' }
$saved = @{}
foreach ($name in @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    [Environment]::SetEnvironmentVariable($name, $null, 'Process')
}
try {
    $packages = @('bellows-cli')
    if ($IncludeServer) { $packages += 'bellows-server' }
    foreach ($package in $packages) {
        & cargo +1.92.0 install --locked --path (Join-Path $repo "crates/$package") --root $InstallRoot --force
        if ($LASTEXITCODE -ne 0) { throw "Installation failed for $package. The compiler output above identifies missing prerequisites." }
    }
    $suffix = if ($IsWindows) { '.exe' } else { '' }
    $binary = Join-Path $InstallRoot "bin/bellows$suffix"
    & $binary --version
    if ($LASTEXITCODE -ne 0) { throw 'The installed Bellows executable did not start.' }
    Write-Host "Installed to $(Join-Path $InstallRoot 'bin'). Ensure this directory is on PATH."
    Write-Host 'From your project: bellows cargo build --release'
    Write-Host 'No server, token, administrator access, or Cargo configuration is required for local caching.'
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}

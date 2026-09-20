#requires -Version 7.2
[CmdletBinding()]
param(
    [string]$Toolchain = '1.92.0-x86_64-pc-windows-msvc',
    [string]$ReportDirectory = (Join-Path ([IO.Path]::GetTempPath()) ('bellows-windows-' + [guid]::NewGuid().ToString('N')))
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if (-not $IsWindows) { throw 'Run this check on native Windows, not WSL.' }
$repo = Split-Path $PSScriptRoot -Parent
$report = [IO.Path]::GetFullPath($ReportDirectory)
New-Item -ItemType Directory -Force $report | Out-Null
$log = Join-Path $report 'checks.log'
$originalEnvironment = @{}
$originalLocation = Get-Location
$serverProcess = $null
$results = [Collections.Generic.List[object]]::new()

function Set-TestEnvironment([string]$Name, [AllowNull()][string]$Value) {
    if (-not $originalEnvironment.ContainsKey($Name)) {
        $originalEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process')
    }
    [Environment]::SetEnvironmentVariable($Name, $Value, 'Process')
}
function Invoke-Checked([string]$Program, [string[]]$CommandArgs) {
    "`n> $Program $($CommandArgs -join ' ')" | Tee-Object -FilePath $log -Append | Write-Host
    & $Program @CommandArgs 2>&1 | Tee-Object -FilePath $log -Append | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "$Program exited with $LASTEXITCODE. See $log" }
}
function Write-Utf8([string]$Path, [string]$Text) {
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}
function Stop-TestServer {
    if ($script:serverProcess -and -not $script:serverProcess.HasExited) {
        Stop-Process -Id $script:serverProcess.Id -Force
        $script:serverProcess.WaitForExit()
    }
    $script:serverProcess = $null
}
function Start-TestServer {
    $script:serverProcess = Start-Process -FilePath $bellowsd -PassThru -NoNewWindow `
        -ArgumentList @('--listen', "127.0.0.1:$port", '--data-dir', ('"' + $store + '"')) `
        -RedirectStandardOutput (Join-Path $report 'server.stdout.log') `
        -RedirectStandardError (Join-Path $report 'server.stderr.log')
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        if ($script:serverProcess.HasExited) { throw 'bellowsd exited before becoming ready.' }
        try {
            $health = Invoke-RestMethod "$server/v1/health" -Headers @{ Authorization = "Bearer $token" }
            $health | ConvertTo-Json | Set-Content (Join-Path $report 'health.json')
            $unauthorized = Invoke-WebRequest "$server/v1/health" -SkipHttpErrorCheck
            if ($unauthorized.StatusCode -ne 401) { throw 'Unauthenticated request was accepted.' }
            return
        } catch {
            if ($attempt -eq 99) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
}
function Run-Phase([string]$Name, [string]$Mode, [string]$ExpectedEvent, [int]$Value, [switch]$AllowMiss) {
    if (Test-Path $target) { Remove-Item -Recurse -Force $target }
    $events = Join-Path $report "$Name.jsonl"
    Set-TestEnvironment 'BELLOWS_EVENT_LOG' $events
    Set-TestEnvironment 'CARGO_TARGET_DIR' $target
    if ($Mode -eq 'local') {
        Invoke-Checked $bellows @('local', '--cache-dir', $cache, '--', 'cargo', 'build', '--release', '--offline')
    } else {
        Invoke-Checked $bellows @('run', '--server', $server, '--', 'cargo', 'build', '--release', '--offline')
    }
    $actual = & (Join-Path $target 'release/fixture.exe')
    if ($LASTEXITCODE -ne 0 -or "$actual".Trim() -ne "$Value") { throw "$Name produced '$actual', expected $Value." }
    $counts = @{}
    Get-Content $events | ForEach-Object {
        $event = $_ | ConvertFrom-Json
        $counts[$event.kind] = 1 + [int]$counts[$event.kind]
    }
    if ([int]$counts[$ExpectedEvent] -lt 1) { throw "$Name had no $ExpectedEvent event." }
    if (-not $AllowMiss -and [int]$counts['miss'] -ne 0) { throw "$Name unexpectedly missed." }
    if ($ExpectedEvent -ne 'fallback' -and [int]$counts['fallback'] -ne 0) { throw "$Name unexpectedly fell back." }
    if ($ExpectedEvent -eq 'hit' -and [int]$counts['l1_hit'] -ne 0) { throw "$Name used L1 during a remote-only check." }
    $results.Add(@{ phase = $Name; value = $Value; events = $counts })
    $results | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $report 'results.json')
    Write-Host "PASS $Name"
}

try {
    # Isolate this qualification from the caller's cache/wrapper configuration.
    foreach ($item in @(Get-ChildItem Env:)) {
        if ($item.Name -match '^(BELLOWS_|CARGO_PROFILE_|CARGO_TARGET_|RUSTFLAGS$|CARGO_ENCODED_RUSTFLAGS$|RUSTC_WRAPPER$|RUSTC_WORKSPACE_WRAPPER$|RUSTC$)') {
            Set-TestEnvironment $item.Name $null
        }
    }
    Set-TestEnvironment 'RUSTUP_TOOLCHAIN' $Toolchain
    Set-TestEnvironment 'CARGO_INCREMENTAL' '0'
    Set-TestEnvironment 'BELLOWS_COLOR' 'never'
    Set-TestEnvironment 'CARGO_TARGET_DIR' (Join-Path $repo 'target')
    Set-Location $repo
    Invoke-Checked 'rustc' @('-vV')
    Invoke-Checked 'cargo' @('--version')
    Invoke-Checked 'cargo' @('fmt', '--all', '--', '--check')
    Invoke-Checked 'cargo' @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
    Invoke-Checked 'cargo' @('test', '--workspace', '--locked', '--no-fail-fast')
    Invoke-Checked 'cargo' @('build', '--workspace', '--bins', '--release', '--locked')
    $bellows = Join-Path $repo 'target/release/bellows.exe'
    $bellowsd = Join-Path $repo 'target/release/bellowsd.exe'
    $workspace = Join-Path $report 'workspace with spaces'
    $target = Join-Path $report 'target with spaces'
    $cache = Join-Path $report 'local cache'
    $store = Join-Path $report 'remote store'
    New-Item -ItemType Directory -Force (Join-Path $workspace 'src') | Out-Null
    Write-Utf8 (Join-Path $workspace 'Cargo.toml') "[package]`nname = 'fixture'`nversion = '0.1.0'`nedition = '2024'`n[workspace]`n"
    Write-Utf8 (Join-Path $workspace 'src/lib.rs') 'mod value; pub fn value() -> u32 { value::get() }'
    Write-Utf8 (Join-Path $workspace 'src/main.rs') 'fn main() { println!("{}", fixture::value()); }'
    Write-Utf8 (Join-Path $workspace 'src/value.rs') 'pub fn get() -> u32 { 42 }'
    Set-Location $workspace
    Run-Phase 'local-cold' 'local' 'miss' 42 -AllowMiss
    Run-Phase 'local-warm' 'local' 'l1_hit' 42
    Write-Utf8 (Join-Path $workspace 'src/value.rs') 'pub fn get() -> u32 { 43 }'
    Run-Phase 'source-edit' 'local' 'miss' 43 -AllowMiss
    Write-Utf8 (Join-Path $workspace 'src/value.rs') 'pub fn get() -> u32 { 42 }'

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $server = "http://127.0.0.1:$port"
    $token = [guid]::NewGuid().ToString('N')
    Set-TestEnvironment 'BELLOWS_AUTH_TOKEN' $token
    Set-TestEnvironment 'BELLOWS_STATE_DIR' (Join-Path $report 'client state')
    Set-TestEnvironment 'BELLOWS_L1' '1'
    Set-TestEnvironment 'BELLOWS_CONNECT_TIMEOUT_MS' '250'
    Set-TestEnvironment 'BELLOWS_REQUEST_TIMEOUT_MS' '1000'
    Start-TestServer
    Run-Phase 'remote-cold' 'remote' 'miss' 42 -AllowMiss
    Run-Phase 'remote-l1' 'remote' 'l1_hit' 42
    Set-TestEnvironment 'BELLOWS_L1' '0'
    Run-Phase 'remote-only' 'remote' 'hit' 42
    Stop-TestServer
    Start-TestServer
    Run-Phase 'remote-restarted' 'remote' 'hit' 42
    Stop-TestServer
    Run-Phase 'offline-fallback' 'remote' 'fallback' 42 -AllowMiss
    Write-Host "`nAll Windows checks passed. Evidence: $report"
} finally {
    Stop-TestServer
    Set-Location $originalLocation
    foreach ($name in $originalEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $originalEnvironment[$name], 'Process')
    }
    Write-Host "Logs retained at $report"
}

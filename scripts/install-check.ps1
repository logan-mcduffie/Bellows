#requires -Version 7.2
param([Parameter(Mandatory)][string]$ReportDirectory)
$ErrorActionPreference='Stop'
$report=[IO.Path]::GetFullPath($ReportDirectory)
if(Test-Path $report){throw 'Use a new report directory'}
New-Item -ItemType Directory $report | Out-Null
$repo=Split-Path $PSScriptRoot -Parent
$saved=@{};Get-ChildItem Env: | ForEach-Object {$saved[$_.Name]=$_.Value}
$location=Get-Location
try {
    Get-ChildItem Env: | Where-Object Name -Match '^(BELLOWS_|CARGO_TARGET_|RUSTC_WRAPPER$|RUSTC_WORKSPACE_WRAPPER$)' | ForEach-Object {Remove-Item "Env:$($_.Name)"}
    & "$PSScriptRoot/install.ps1" -InstallRoot "$report/install" *> "$report/install.log"
    $suffix=if($IsWindows){'.exe'}else{''}
    $b="$report/install/bin/bellows$suffix"
    Copy-Item "$repo/demo" "$report/project with spaces" -Recurse
    Set-Location "$report/project with spaces"
    $env:BELLOWS_STATE_DIR="$report/cache";$env:BELLOWS_COLOR='never'
    $results=@()
    foreach($phase in 'cold','warm') {
        & $b cargo build --release --offline *> "$report/$phase.log"
        if($LASTEXITCODE -ne 0){throw "Installed client build failed: $phase"}
        $output=& "./target/release/forge-cli$suffix"
        if($LASTEXITCODE -ne 0 -or $output -notmatch '42'){throw 'Installed client produced wrong output'}
        if($phase -eq 'warm' -and (Get-Content "$report/$phase.log" -Raw) -notmatch 'LOCAL HIT'){throw 'Installed client did not restore'}
        $results+=@{phase=$phase;exit_code=0;output=$output;log="$report/$phase.log"}
        $results | ConvertTo-Json | Set-Content "$report/results.json"
        # Fixed child of the newly created fixture, never a caller target/cache.
        Remove-Item -LiteralPath (Join-Path $report 'project with spaces/target') -Recurse -Force
    }
    Write-Host 'Installed client cold/warm build passed.'
} finally {
    Set-Location $location
    Get-ChildItem Env: | Where-Object {-not $saved.ContainsKey($_.Name)} | ForEach-Object {Remove-Item "Env:$($_.Name)"}
    foreach($name in $saved.Keys){[Environment]::SetEnvironmentVariable($name,$saved[$name],'Process')}
}

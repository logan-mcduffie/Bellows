#requires -Version 7.2
param([Parameter(Mandatory)][string]$ReportDirectory)
$ErrorActionPreference='Stop'
$PSNativeCommandUseErrorActionPreference=$false
$repo=Split-Path $PSScriptRoot -Parent
$report=[IO.Path]::GetFullPath($ReportDirectory)
if(Test-Path $report){throw 'A new report directory is required'}
New-Item -ItemType Directory $report | Out-Null
$suffix=if($IsWindows){'.exe'}else{''}
$b=Join-Path $repo "target/release/bellows$suffix"
$d=Join-Path $repo "target/release/bellowsd$suffix"
$original=@{}; Get-ChildItem Env: | ForEach-Object {$original[$_.Name]=$_.Value}
$location=Get-Location
$results=[Collections.Generic.List[object]]::new()
$processes=[Collections.Generic.List[object]]::new()
$script:number=0
function Assert($condition,$message){if(-not $condition){throw $message}}
function WriteFile($path,$value){[IO.File]::WriteAllText($path,$value,[Text.UTF8Encoding]::new($false))}
function RemoveTest($path){
  $resolved=[IO.Path]::GetFullPath($path)
  Assert ($resolved.StartsWith($report+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) "Unsafe deletion $resolved"
  if(Test-Path -LiteralPath $resolved){Remove-Item -LiteralPath $resolved -Recurse -Force}
}
function Call($program,[string[]]$arguments,[int]$expected=0){
  $script:number++
  $log=Join-Path $report ('command-{0:D3}.log' -f $script:number)
  $text=(& $program @arguments 2>&1 | Out-String)
  $code=$LASTEXITCODE
  WriteFile $log $text
  @{command="$program $($arguments -join ' ')";exit_code=$code;log=$log} | ConvertTo-Json -Compress | Add-Content (Join-Path $report 'commands.jsonl')
  Assert (($expected -eq 0 -and $code -eq 0) -or ($expected -ne 0 -and $code -ne 0)) "Command exit $code : $log : $text"
  return $text
}
function Case($name,[scriptblock]$body){
  try{ & $body; $results.Add(@{name=$name;status='passed'}); Write-Host "PASS $name" }
  catch{ $results.Add(@{name=$name;status='failed';error="$_"}); Write-Host "FAIL $name : $_" }
  $results | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $report 'results.json')
}
function BeginClient($name,[string[]]$arguments,$output,$working=$workspace){
  $info=[Diagnostics.ProcessStartInfo]::new($b)
  $info.UseShellExecute=$false;$info.CreateNoWindow=$true
  $info.RedirectStandardOutput=$true;$info.RedirectStandardError=$true
  $info.WorkingDirectory=$working
  foreach($arg in $arguments){$info.ArgumentList.Add($arg)}
  if($output){$info.Environment['CARGO_TARGET_DIR']=$output}
  $proc=[Diagnostics.Process]::Start($info);$processes.Add($proc)
  return @{proc=$proc;stdout=$proc.StandardOutput.ReadToEndAsync();stderr=$proc.StandardError.ReadToEndAsync();name=$name}
}
function EndClient($client){
  Assert ($client.proc.WaitForExit(60000)) 'Client exceeded 60 seconds'
  $text=$client.stdout.GetAwaiter().GetResult()+$client.stderr.GetAwaiter().GetResult()
  WriteFile (Join-Path $report ($client.name+'.log')) $text
  Assert ($client.proc.ExitCode -eq 0) "Client failed: $text"
  return $text
}
function StartServer($name,[switch]$Execution){
  $listener=[Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0);$listener.Start();$port=$listener.LocalEndpoint.Port;$listener.Stop()
  $store=Join-Path $report $name
  $args=@('--listen',"127.0.0.1:$port",'--data-dir',('"'+$store+'"'))
  if($Execution){$args+='--enable-execution'}
  $windowOptions=if($IsWindows){@{WindowStyle='Hidden'}}else{@{}}
  $proc=Start-Process -FilePath $d -ArgumentList $args @windowOptions -PassThru -RedirectStandardOutput "$store.stdout.log" -RedirectStandardError "$store.stderr.log"
  $processes.Add($proc)
  for($i=0;$i -lt 100;$i++){
    try{Invoke-RestMethod "http://127.0.0.1:$port/live" | Out-Null;return @{url="http://127.0.0.1:$port";process=$proc;store=$store}}catch{Start-Sleep -Milliseconds 100}
  };throw 'server did not start'
}
try{
  Get-ChildItem Env: | Where-Object Name -Match '^(BELLOWS_|CARGO_TARGET_|CARGO_PROFILE_|RUSTFLAGS$|RUSTC_WRAPPER$|RUSTC_WORKSPACE_WRAPPER$|CARGO_ENCODED_RUSTFLAGS$)' | ForEach-Object {Remove-Item "Env:$($_.Name)"}
  $env:RUSTUP_TOOLCHAIN='1.92.0';$env:CARGO_INCREMENTAL='0';$env:BELLOWS_COLOR='never'
  $env:BELLOWS_STATE_DIR=Join-Path $report 'state';$env:BELLOWS_AUTH_TOKEN=[guid]::NewGuid().ToString('N')
  $env:BELLOWS_CONNECT_TIMEOUT_MS='250';$env:BELLOWS_REQUEST_TIMEOUT_MS='1000'
  $workspace=Join-Path $report 'workspace café with spaces'
  Copy-Item -LiteralPath (Join-Path $repo 'demo') -Destination $workspace -Recurse
  Set-Location $workspace
  $target=Join-Path $workspace 'target';$exe=Join-Path $target "release/forge-cli$suffix"
  $server=StartServer 'cache-service';$env:BELLOWS_SERVER=$server.url
  Case 'doctor-auth-and-ownership' {
    Call $b @('doctor') | Out-Null
    foreach($headers in @(@{},@{Authorization='Bearer wrong'})){
      Assert ((Invoke-WebRequest "$($server.url)/v1/health" -Headers $headers -SkipHttpErrorCheck).StatusCode -eq 401) 'Authentication accepted'
    }
    $text=Call $d @('--data-dir',$server.store) 1;Assert ($text -match 'already owned') 'Missing ownership reason'
  }
  Case 'local-shorthand-no-network-noop-and-nonascii' {
    $sentinel=[Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0);$sentinel.Start()
    try{
      $env:BELLOWS_SERVER="http://127.0.0.1:$($sentinel.LocalEndpoint.Port)"
      Call $b @('cargo','build','--release','--offline') | Out-Null
      Assert (-not $sentinel.Pending()) 'Local mode connected to network'
      Assert ((Call $exe @()) -match '42') 'Wrong cold output'
      Call $b @('cargo','build','--release','--offline') | Out-Null
      $events=Call $b @('explain','--local','--latest','--json') | ConvertFrom-Json
      Assert (@($events | Where-Object kind -In @('miss','hit','l1_hit','bypass')).Count -eq 0) 'No-op invoked compiler'
      RemoveTest $target
      $text=Call $b @('cargo','build','--release','--offline');Assert ($text -match 'LOCAL HIT') 'No warm hit'
      Assert ((Call $exe @()) -match '42') 'Wrong restored output'
    }finally{$sentinel.Stop();$env:BELLOWS_SERVER=$server.url}
  }
  Case 'incremental-bypass' {
    $env:CARGO_INCREMENTAL='1'
    try{ $text=Call $b @('cargo','build','--offline');Assert ($text -match 'incremental') 'No incremental bypass' }finally{$env:CARGO_INCREMENTAL='0'}
  }
  Case 'concurrent-local-and-remote-single-flight' {
    $clients=@();foreach($name in @('local-a','local-b')){$clients+=BeginClient $name @('cargo','build','--release','--offline') (Join-Path $report $name)}
    foreach($client in $clients){EndClient $client | Out-Null;Assert ((Call "$report/$($client.name)/release/forge-cli$suffix" @()) -match '42') 'Concurrent local output wrong'}
    $env:BELLOWS_L1='0';$env:BELLOWS_DEMO_COMPILE_DELAY_MS='2000'
    try{
      $clients=@();foreach($name in @('remote-a','remote-b')){$clients+=BeginClient $name @('run','--','cargo','build','--release','--offline') (Join-Path $report $name)}
      $combined='';foreach($client in $clients){$combined+=EndClient $client;Assert ((Call "$report/$($client.name)/release/forge-cli$suffix" @()) -match '42') 'Concurrent remote output wrong'}
      Assert ($combined -match 'SHARED HIT') 'No shared single-flight hit'
      $events=Get-Content "$env:BELLOWS_STATE_DIR/events.jsonl" | ForEach-Object {$_ | ConvertFrom-Json}
      Assert (@($events | Where-Object {$_.kind -eq 'store' -and $_.crate_name -eq 'forge_core'}).Count -eq 1) 'Identical library published more than once'
    }finally{$env:BELLOWS_DEMO_COMPILE_DELAY_MS=$null;$env:BELLOWS_L1='1'}
  }
  Case 'source-dependency-and-branch-return' {
    $source=Join-Path $workspace 'crates/forge-core/src/temperature.rs';$before=Get-Content $source -Raw
    try{
      WriteFile $source ($before.Replace('42','46'));RemoveTest $target
      $text=Call $b @('cargo','build','--release','--offline')
      Assert ($text -match 'temperature.rs') 'Source cause missing'
      Assert ($text -match 'forge_engine') 'Dependent action not evaluated'
      Assert ((Call $exe @()) -match '46') 'Stale dependency output'
      WriteFile $source $before;RemoveTest $target
      Assert ((Call $b @('cargo','build','--release','--offline')) -match 'LOCAL HIT') 'Prior source version did not restore'
      Assert ((Call $exe @()) -match '42') 'Branch-return output wrong'
    }finally{WriteFile $source $before}
  }
  Case 'archive-two-executors-and-publish-once' {
    $archive=Join-Path $report 'archive';New-Item -ItemType Directory $archive | Out-Null
    Call rustc @('--edition','2024','--test','crates/forge-core/src/lib.rs','-o',"$archive/tests$suffix") | Out-Null
    Call "$archive/tests$suffix" @() | Out-Null
    Call $b @('archive','publish','tests-v1',$archive) | Out-Null
    foreach($destination in @('executor-a','executor-b')){
      Call $b @('archive','restore','tests-v1',"$report/$destination") | Out-Null
      Call "$report/$destination/tests$suffix" @() | Out-Null
      Assert ((Get-FileHash "$archive/tests$suffix").Hash -eq (Get-FileHash "$report/$destination/tests$suffix").Hash) 'Archive hash mismatch'
    }
    WriteFile "$archive/tests$suffix" 'different';Call $b @('archive','publish','tests-v1',$archive) 1 | Out-Null
  }
  Case 'bounded-lease-timeout-and-release' {
    $env:BELLOWS_L1='0';RemoveTest $target
    Call $b @('run','--','cargo','build','--release','--offline') | Out-Null
    $events=Get-Content "$env:BELLOWS_STATE_DIR/events.jsonl" | ForEach-Object {$_ | ConvertFrom-Json}
    $key=($events | Where-Object {$_.kind -eq 'store' -and $_.crate_name -eq 'forge_core'} | Select-Object -Last 1).static_key
    $headers=@{Authorization="Bearer $env:BELLOWS_AUTH_TOKEN"}
    $lease=Invoke-RestMethod "$($server.url)/v1/leases/$key" -Method Post -Headers $headers -ContentType 'application/json' -Body '{"client_id":"qualification-holder","ttl_ms":60000}'
    Assert ($lease.status -eq 'owned') 'Could not acquire test lease'
    $source=Join-Path $workspace 'crates/forge-core/src/temperature.rs';$before=Get-Content $source -Raw
    try{
      WriteFile $source ($before.Replace('42','47'));RemoveTest $target;$env:BELLOWS_MAX_WAIT_MS='0'
      $watch=[Diagnostics.Stopwatch]::StartNew();$text=Call $b @('run','--','cargo','build','--release','--offline')
      Assert ($text -match 'wait timed out' -and $watch.Elapsed.TotalSeconds -lt 30) 'Lease timeout not bounded and explained'
      Assert ((Call $exe @()) -match '47') 'Timeout fallback produced stale code'
    }finally{
      Invoke-RestMethod "$($server.url)/v1/leases/$key/$($lease.token)" -Method Delete -Headers $headers | Out-Null
      WriteFile $source $before;$env:BELLOWS_MAX_WAIT_MS=$null;$env:BELLOWS_L1='1'
    }
  }
  Case 'compiler-version-invalidation' {
    RemoveTest $target;Call $b @('cargo','build','--release','--offline') | Out-Null
    $toolchain=$env:RUSTUP_TOOLCHAIN
    try{
      $env:RUSTUP_TOOLCHAIN='nightly-2026-01-15-x86_64-pc-windows-msvc';RemoveTest $target
      $text=Call $b @('cargo','build','--release','--offline')
      Assert ($text -match 'compiler version changed') 'Compiler identity change not explained'
      Assert ((Call $exe @()) -match '42') 'Changed compiler output wrong'
    }finally{$env:RUSTUP_TOOLCHAIN=$toolchain}
  }
  Case 'unsafe-and-malformed-service-configuration' {
    $token=$env:BELLOWS_AUTH_TOKEN;$env:BELLOWS_AUTH_TOKEN=$null
    try{Assert ((Call $d @('--listen','0.0.0.0:0','--data-dir',"$report/unsafe") 1) -match 'refusing unauthenticated') 'Unsafe listener not rejected'}finally{$env:BELLOWS_AUTH_TOKEN=$token}
    Assert ((Call $d @('--max-requests','0','--data-dir',"$report/invalid") 1) -match 'greater than zero') 'Invalid request ceiling accepted'
    $env:BELLOWS_SERVER='://not-a-url';$env:BELLOWS_L1='0'
    try{RemoveTest $target;$text=Call $b @('run','--','cargo','build','--release','--offline');Assert ($text -match 'invalid remote configuration') 'Malformed URL not explained';Assert ((Call $exe @()) -match '42') 'Malformed service blocked ordinary compilation'}finally{$env:BELLOWS_SERVER=$server.url;$env:BELLOWS_L1='1'}
  }
  $action=@('action','run','--local','--name','final-link','--input','Cargo.toml','--input','Cargo.lock','--input','crates','--output','target','--','cargo','build','--locked','--offline','--release','-p','forge-cli')
  Case 'declared-final-link-replacement-and-missing-blob' {
    RemoveTest $target;Call $b $action | Out-Null
    $hash=(Get-FileHash $exe).Hash
    WriteFile "$target/obsolete" 'obsolete'
    $text=Call $b $action;Assert ($text -match 'HIT') 'Declared action missed'
    Assert (-not (Test-Path "$target/obsolete")) 'Obsolete output survived'
    Assert ((Get-FileHash $exe).Hash -eq $hash) 'Restored executable changed'
    $record=Get-ChildItem "$env:BELLOWS_STATE_DIR/store-v5/declared" -Recurse -Filter '*.json' | Select-Object -First 1
    $digest=(Get-Content $record.FullName -Raw | ConvertFrom-Json).outputs[0].digest
    RemoveTest "$env:BELLOWS_STATE_DIR/store-v5/blobs/$($digest.Substring(0,2))/$digest"
    $text=Call $b $action;Assert ($text -match 'stale cached result will be rebuilt') 'Missing blob not diagnosed'
    Assert ((Call $exe @()) -match '42') 'Rebuilt output wrong'
    Call $b @('gc','--local','--max-mb','0') | Out-Null
    Call $b $action | Out-Null
    Assert ((Call $exe @()) -match '42') 'GC recovery wrong'
  }
  Case 'nested-cargo-generated-output' {
    Push-Location (Join-Path $workspace 'phase3-fixture')
    try{
      $nested=@('action','run','--local','--name','nested','--input','Cargo.toml','--input','Cargo.lock','--input','build.rs','--input','src','--input','generator','--output','target','--','cargo','build','--locked','--offline')
      Call $b $nested | Out-Null
      Assert ((Call "./target/debug/phase3-fixture$suffix" @()) -match 'nested Cargo') 'Generated content wrong'
      RemoveTest (Join-Path (Get-Location) 'target')
      Assert ((Call $b $nested) -match 'HIT') 'Nested output did not restore'
      Assert ((Call "./target/debug/phase3-fixture$suffix" @()) -match 'nested Cargo') 'Restored generated content wrong'
    }finally{Pop-Location}
  }
  Case 'advisory-private-public-analysis' {
    Call $b @('analyze','snapshot','before') | Out-Null
    $source=Join-Path $workspace 'crates/forge-core/src/temperature.rs';$before=Get-Content $source -Raw
    try{
      WriteFile $source ($before.Replace('42','45'));Call $b @('analyze','snapshot','private') | Out-Null
      $private=Call $b @('analyze','compare','before','private','--json') | ConvertFrom-Json
      Assert ($private.syntactic_surface_changed.Count -eq 0 -and $private.private_implementation_candidates -contains 'forge-core') 'Private analysis wrong'
      WriteFile $source ($before.Replace('-> u32','-> u64'));Call $b @('analyze','snapshot','public') | Out-Null
      $public=Call $b @('analyze','compare','before','public','--json') | ConvertFrom-Json
      Assert ($public.affected_downstream -contains 'forge-cli') 'Downstream closure wrong'
    }finally{WriteFile $source $before}
  }
  Case 'authenticated-remote-execution-and-failure' {
    $env:BELLOWS_REQUEST_TIMEOUT_MS='60000'
    $script:executor=StartServer 'execution-service' -Execution
    $remote=@('remote','run','--server',$executor.url,'--name','remote-final','--input','Cargo.toml','--input','Cargo.lock','--input','crates','--output','target','--','cargo','build','--release','--locked','--offline','-p','forge-cli')
    RemoveTest $target; $text=Call $b $remote;Assert ($text -match 'EXECUTED') 'Remote action not executed'
    Assert ((Call $exe @()) -match '42') 'Remote binary wrong'
    RemoveTest $target;Assert ((Call $b $remote) -match 'HIT') 'Remote action not restored'
    $bad=@('remote','run','--server',$executor.url,'--name','invalid','--input','Cargo.toml','--input','Cargo.lock','--input','crates','--output','target','--','cargo','build','--locked','--offline','-p','nonexistent')
    Call $b $bad 1 | Out-Null
    $disabled=$remote.Clone();$disabled[3]=$server.url;Call $b $disabled 1 | Out-Null
    $oldToolchain=$env:RUSTUP_TOOLCHAIN
    try{
      $env:RUSTUP_TOOLCHAIN='nightly-2026-01-15'
      $text=Call $b $remote 1;Assert ($text -match '412 Precondition Failed') 'Toolchain mismatch did not return 412'
    }finally{$env:RUSTUP_TOOLCHAIN=$oldToolchain}
    $record=Get-ChildItem "$($executor.store)/declared" -Recurse -Filter '*.json' | Select-Object -First 1 | Get-Content -Raw | ConvertFrom-Json
    $record.platform.rustc='deliberate-mismatch'
    $request=@{key=$record.key;name=$record.name;platform=$record.platform;command=$record.command;environment=$record.environment;inputs=$record.inputs;outputs=$record.output_paths}
    $response=Invoke-WebRequest "$($executor.url)/v1/execute" -Method Post -ContentType 'application/json' -Headers @{Authorization="Bearer $env:BELLOWS_AUTH_TOKEN"} -Body ($request | ConvertTo-Json -Depth 20) -SkipHttpErrorCheck
    Assert ($response.StatusCode -eq 412 -and $response.Content -match 'toolchain identity') 'Executor did not explain mismatch'
    WriteFile "$report/toolchain-mismatch.txt" $response.Content
  }
  Case 'remote-flags-environment-and-output-replacement' {
    $flags=Join-Path $report 'flags';New-Item -ItemType Directory "$flags/src","$flags/.cargo" | Out-Null
    WriteFile "$flags/Cargo.toml" "[package]`nname='flag-fixture'`nversion='0.1.0'`nedition='2024'`n[workspace]`n"
    WriteFile "$flags/src/main.rs" 'fn main(){assert!(cfg!(audit_flag));assert_eq!(env!("AUDIT_VALUE"),"declared");}'
    Push-Location $flags
    try{
      Call cargo @('generate-lockfile','--offline') | Out-Null
      $env:AUDIT_VALUE='declared'
      foreach($kind in @('RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','config')){
        $args=@('remote','run','--server',$executor.url,'--name',"flags-$kind",'--input','Cargo.toml','--input','Cargo.lock','--input','src','--output','output','--env','AUDIT_VALUE')
        if($kind -eq 'config'){WriteFile "$flags/.cargo/config.toml" "[build]`nrustflags=['--cfg','audit_flag']";$args+=@('--input','.cargo')}
        else{[Environment]::SetEnvironmentVariable($kind,($(if($kind -eq 'RUSTFLAGS'){'--cfg audit_flag'}else{"--cfg$([char]31)audit_flag"})),'Process');$args+=@('--env',$kind)}
        $args+=@('--','cargo','build','--release','--locked','--offline','--target-dir','output')
        Call $b $args | Out-Null;Call "./output/release/flag-fixture$suffix" @() | Out-Null
        WriteFile "$flags/output/stale" 'stale';Assert ((Call $b $args) -match 'HIT') 'Remote flag action missed'
        Assert (-not(Test-Path "$flags/output/stale")) 'Remote obsolete output survived';Call "./output/release/flag-fixture$suffix" @() | Out-Null
        if($kind -ne 'config'){[Environment]::SetEnvironmentVariable($kind,$null,'Process')}
      }
    }finally{$env:RUSTFLAGS=$null;$env:CARGO_ENCODED_RUSTFLAGS=$null;$env:AUDIT_VALUE=$null;Pop-Location}
  }
  Case 'remote-execution-single-flight' {
    $env:BELLOWS_ACTION_DELAY_MS='1500'
    try{
      $clients=@()
      foreach($name in @('execution-a','execution-b')){
        $destination=Join-Path $report $name;New-Item -ItemType Directory $destination | Out-Null
        Copy-Item Cargo.toml,Cargo.lock $destination;Copy-Item crates $destination -Recurse
        $args=@('remote','run','--server',$executor.url,'--name','remote-race','--input','Cargo.toml','--input','Cargo.lock','--input','crates','--output','target','--env','BELLOWS_ACTION_DELAY_MS','--','cargo','build','--release','--locked','--offline','-p','forge-cli')
        $clients+=BeginClient $name $args $null $destination
      }
      $combined='';foreach($client in $clients){$combined+=EndClient $client;Assert ((Call "$report/$($client.name)/target/release/forge-cli$suffix" @()) -match '42') 'Remote concurrent output wrong'}
      Assert (([regex]::Matches($combined,'EXECUTED remote-race')).Count -eq 1 -and $combined -match 'CACHE HIT') 'Remote execution did not single-flight'
    }finally{$env:BELLOWS_ACTION_DELAY_MS=$null}
  }
  Case 'remote-corruption-and-complete-cache-loss' {
    $env:BELLOWS_L1='0';RemoveTest $target;Call $b @('run','--','cargo','build','--release','--offline') | Out-Null
    Get-ChildItem "$($server.store)/blobs" -File -Recurse | ForEach-Object {WriteFile $_.FullName 'corrupt'}
    RemoveTest $target;$text=Call $b @('run','--','cargo','build','--release','--offline')
    Assert ($text -match 'REJECTED') 'Corruption not rejected';Assert ((Call $exe @()) -match '42') 'Corrupt output used'
    Stop-Process -Id $server.process.Id -Force;$server.process.WaitForExit();RemoveTest $server.store
    $server=StartServer 'cache-service';$env:BELLOWS_SERVER=$server.url
    RemoveTest $target;Call $b @('run','--','cargo','build','--release','--offline') | Out-Null
    Assert ((Call $exe @()) -match '42') 'Cache loss recovery wrong'
  }
  Case 'stats-explain-json-and-gc' {
    Call $b @('stats','--json') | ConvertFrom-Json | Out-Null
    Call $b @('explain','--latest','--crate','forge_core','--json') | ConvertFrom-Json | Out-Null
    Call $b @('gc','--max-mb','0') | Out-Null
  }
  Case 'corrupt-l1-index-and-offline-service' {
    $env:BELLOWS_L1='1';RemoveTest $target
    Call $b @('run','--','cargo','build','--release','--offline') | Out-Null
    $indexes=Get-ChildItem "$env:BELLOWS_STATE_DIR/l1-v5/actions" -Recurse -Filter '*.json'
    Assert ($indexes.Count -gt 0) 'No L1 indexes to corrupt'
    foreach($index in $indexes){WriteFile $index.FullName '{not-json'}
    # The current server is one created by this script, tracked by its PID.
    foreach($proc in $processes){if(-not $proc.HasExited){Stop-Process -Id $proc.Id -Force;$proc.WaitForExit()}}
    RemoveTest $target;$env:BELLOWS_REQUEST_TIMEOUT_MS='1000'
    $text=Call $b @('run','--','cargo','build','--release','--offline')
    Assert ($text -match 'L1 index is unavailable or corrupt' -and $text -match 'remote unavailable') 'Corrupt index/offline reasons missing'
    Assert ((Call $exe @()) -match '42') 'Offline corrupt-index fallback failed'
  }
}finally{
  foreach($proc in $processes){if(-not $proc.HasExited){Stop-Process -Id $proc.Id -Force;$proc.WaitForExit()}}
  Set-Location $location
  Get-ChildItem Env: | Where-Object {-not $original.ContainsKey($_.Name)} | ForEach-Object {Remove-Item "Env:$($_.Name)"}
  foreach($name in $original.Keys){[Environment]::SetEnvironmentVariable($name,$original[$name],'Process')}
}
if(@($results | Where-Object status -eq 'failed').Count){exit 1}


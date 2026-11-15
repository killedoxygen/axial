param(
    [Parameter(Mandatory=$true)][string]$Base,
    [Parameter(Mandatory=$true)][string]$Output,
    [int]$Samples = 40,
    [string]$Instance,
    [int]$Launches = 0,
    [int]$SettleSeconds = 15
)
$ErrorActionPreference = 'Stop'
if ($Samples -lt 1 -or $Samples -gt 200 -or $Launches -lt 0 -or $Launches -gt 20) { throw 'Invalid sample count' }
$bootstrap = Invoke-RestMethod "$Base/api/v1/transport/bootstrap" -Method Post -Headers @{Origin=$Base} -ContentType 'application/json' -Body '{}'
$headers = @{'x-axial-capability'=$bootstrap.capability}
function Request([string]$Method, [string]$Path, $Body=$null) {
    $requestOptions = @{Uri="$Base$Path";Method=$Method;Headers=$headers;TimeoutSec=180}
    if ($null -ne $Body) { $requestOptions.ContentType='application/json'; $requestOptions.Body=($Body | ConvertTo-Json -Depth 20 -Compress) }
    $watch=[Diagnostics.Stopwatch]::StartNew()
    try { $response=Invoke-RestMethod @requestOptions; $status=200 } catch { $watch.Stop(); throw }
    $watch.Stop()
    return @{http_status=$status;elapsed_ms=$watch.Elapsed.TotalMilliseconds;response=$response}
}
function Record([string]$Kind,[int]$Index,$Result) {
    $Result.kind=$Kind; $Result.index=$Index; $Result.at=[DateTime]::UtcNow.ToString('o')
    $Result | ConvertTo-Json -Depth 40 -Compress | Add-Content -LiteralPath $Output -Encoding UTF8
    $elapsedLabel = if ($null -eq $Result.elapsed_ms) { 'untimed' } else { [Math]::Round($Result.elapsed_ms,3) }
    Write-Host $Kind $Index $elapsedLabel
}
for ($index=0;$index -lt $Samples;$index++) { Record 'instances' $index (Request 'GET' '/api/v1/instances') }
if ($Launches -and -not $Instance) { throw 'Launches require Instance' }
for ($index=0;$index -lt $Launches;$index++) {
    $session=$null; $reconciled=$false; $intentKey=[Guid]::NewGuid().ToString()
    Record 'launch_request' $index @{response=@{intent_key=$intentKey;instance_id=$Instance};http_status=0;elapsed_ms=0}
    try {
        try { $result=Request 'POST' '/api/v1/launch' @{instance_id=$Instance;intent_key=$intentKey} }
        catch {
            $deadline=[DateTime]::UtcNow.AddSeconds(120)
            do {
                $intent=Request 'GET' "/api/v1/launch/intents/$intentKey"
                Record 'intent_reconciliation' $index $intent
                if ($intent.response.state -eq 'accepted') { $result=@{response=$intent.response.session;http_status=200;elapsed_ms=0};$session=$result.response.session_id;$reconciled=$true;break }
                if ($intent.response.state -in @('rejected','interrupted')) { throw 'Launch intent refused; evidence retained' }
                Start-Sleep -Milliseconds 250
            } while ([DateTime]::UtcNow -lt $deadline)
            if (-not $session) { throw 'Launch outcome unknown; retain intent and do not relaunch' }
        }
        if ($reconciled) { $result.elapsed_ms=$null;Record 'launch_reconciled' $index $result } else { Record 'launch_admission' $index $result }
        $state=$result.response; $session=$state.session_id; $deadline=[DateTime]::UtcNow.AddSeconds(120)
        while (-not $state.boot_observed -and -not $state.view_model.terminal) {
            if ([DateTime]::UtcNow -ge $deadline) { throw 'Startup observation timed out' }
            Start-Sleep -Milliseconds 250
            $state=(Request 'GET' "/api/v1/launch/$session/status").response
        }
        Record 'boot_observed_not_title_ready' $index @{response=$state;http_status=200;elapsed_ms=0}
        if ($state.process_alive) { Start-Sleep -Seconds $SettleSeconds }
    } finally {
        if ($session) {
            try { Record 'owner_stop' $index (Request 'POST' "/api/v1/launch/$session/kill" @{}) }
            catch { Record 'stop_response_lost' $index @{response=@{session_id=$session};http_status=0;elapsed_ms=0} }
            $deadline=[DateTime]::UtcNow.AddSeconds(60)
            do {
                $state=(Request 'GET' "/api/v1/launch/$session/status").response
                if ($state.view_model.terminal -and $state.tree_settled -and $state.output_drained) { break }
                Start-Sleep -Milliseconds 250
            } while ([DateTime]::UtcNow -lt $deadline)
            if (-not $state.view_model.terminal -or -not $state.tree_settled -or -not $state.output_drained) { Record 'unsettled' $index @{response=$state;http_status=200;elapsed_ms=0};throw 'Original owner remains unsettled' }
            Record 'settled' $index @{response=$state;http_status=200;elapsed_ms=0}
            Record 'report' $index (Request 'GET' "/api/v1/launch/reports/$session")
        }
    }
}

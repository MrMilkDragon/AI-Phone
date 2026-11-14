param(
    [Parameter(Mandatory=$true)][string]$AcceptanceRoot,
    [Parameter(Mandatory=$true)][int]$ClientPid
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path $AcceptanceRoot).Path
$Install = Join-Path $Root 'installed'
$Program = Join-Path $Install 'ai-phone-client.exe'
$Profile = Join-Path $Root 'profile'
$Home = Join-Path $Profile 'dev.aiphone.client\host'
$Owner = Join-Path $Home 'user\data\android\native\session.pb'
$Ports = @(17860, 17861, 15037, 5565)
$Client = Get-Process -Id $ClientPid
if ($Client.Path -ne $Program -or (Split-Path $Root -Leaf) -ne "AImanity-QA-$env:GITHUB_RUN_ID") {
    throw 'Use the installer acceptance process and explicit disposable profile.'
}

function Get-Listeners() {
    $active = [Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()
    @($active | Where-Object { $_.Port -in $Ports } | Select-Object -ExpandProperty Port -Unique)
}

if ((Get-Listeners).Count -ne $Ports.Count -or -not (Test-Path $Owner)) {
    throw 'Run this after the two-device acceptance checks.'
}

$timer = [Diagnostics.Stopwatch]::StartNew()
if (-not $Client.CloseMainWindow() -or -not $Client.WaitForExit(110000)) {
    throw 'The app did not complete normal window-close shutdown.'
}
if ($Client.ExitCode -ne 0 -or (Get-Listeners).Count -ne 0 -or (Test-Path $Owner)) {
    throw "App exit=$($Client.ExitCode), listening ports=$(Get-Listeners), Android session record present=$(Test-Path $Owner)"
}
Write-Host "Graceful Windows shutdown released both devices in $($timer.ElapsedMilliseconds) ms"

$env:APPDATA = $Profile
$env:LOCALAPPDATA = $Profile
$env:WEBVIEW2_USER_DATA_FOLDER = Join-Path $Root 'webview'
$timer.Restart()
$Reopened = Start-Process -FilePath $Program -WorkingDirectory $Install -PassThru
$Deadline = [DateTime]::UtcNow.AddMinutes(8)
do {
    $Reopened.Refresh()
    if ($Reopened.HasExited) { throw "Reopened client exited $($Reopened.ExitCode)" }
    if ((Get-Listeners).Count -eq $Ports.Count -and (Test-Path $Owner) -and $Reopened.MainWindowTitle -eq 'AImanity') {
        $Response = & curl.exe --noproxy '*' --silent --max-time 2 --head 'http://127.0.0.1:17860/phone'
        if (($Response -match '^HTTP/\S+ 200') -and ($Response -match '^content-type: text/html')) {
            $Adb = Get-ChildItem -Path (Join-Path $Home 'components\android\*\sdk\platform-tools\adb.exe') | Select-Object -First 1
            $Uid = & $Adb.FullName '-P' '15037' '-s' '127.0.0.1:5565' shell 'id -u'
            if ($LASTEXITCODE -eq 0 -and $Uid.Trim() -eq '0') {
                Write-Host "Reopened Windows workbench and rooted Android in $($timer.ElapsedMilliseconds) ms (PID $($Reopened.Id))"
                exit 0
            }
        }
    }
    Start-Sleep -Milliseconds 500
} while ([DateTime]::UtcNow -lt $Deadline)
throw 'The native workbench and rooted Android did not reconnect after a normal quit.'

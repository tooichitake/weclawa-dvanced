# Local end-to-end test for weclawbot.
# Builds, installs, restarts daemon, verifies HTTP endpoints + config + send.
# Usage:  scripts\test.ps1            # full cycle
#         scripts\test.ps1 -SkipBuild # use existing target/release binary
#         scripts\test.ps1 -Echo      # turn echo mode on for live reply test

param(
    [switch]$SkipBuild,
    [switch]$Echo,
    [string]$WebhookUrl = "",
    [string]$BinDir = "$env:USERPROFILE\bin"
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$BinName = "weclawbot.exe"
$ReleaseExe = Join-Path $ProjectRoot "target\release\$BinName"
$InstalledExe = Join-Path $BinDir $BinName
$StateDir = "$env:USERPROFILE\.weclawbot"
$Port = 18011

function Step($msg) { Write-Host "==>" $msg -ForegroundColor Cyan }
function Pass($msg) { Write-Host "  OK" $msg -ForegroundColor Green }
function Fail($msg) { Write-Host "  FAIL" $msg -ForegroundColor Red; throw $msg }

# 1. Build
if (-not $SkipBuild) {
    Step "Building release binary"
    Push-Location $ProjectRoot
    try {
        # cargo writes progress to stderr; don't merge streams (PS would error on stderr writes)
        $prevEAP = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        & cargo build --release | Out-Null
        $code = $LASTEXITCODE
        $ErrorActionPreference = $prevEAP
        if ($code -ne 0) { Fail "cargo build exit code $code" }
        Pass "Build complete"
    } finally { Pop-Location }
}

if (-not (Test-Path $ReleaseExe)) { Fail "Binary missing: $ReleaseExe" }

# 2. Stop existing
Step "Stopping any running instance"
if (Test-Path $InstalledExe) {
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $InstalledExe stop 2>&1 | Out-Null
    $ErrorActionPreference = $prevEAP
}
# Belt-and-suspenders: kill any lingering weclawbot.exe processes
Get-Process -Name "weclawbot" -ErrorAction SilentlyContinue | ForEach-Object {
    Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
}
# Wait for file lock release (Windows can hold lock briefly after process exit)
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 250
    if (-not (Test-Path $InstalledExe)) { break }
    try {
        $fs = [System.IO.File]::Open($InstalledExe, 'Open', 'Write', 'None')
        $fs.Close()
        break
    } catch { }
}

# 3. Install
Step "Installing to $InstalledExe"
if (-not (Test-Path $BinDir)) { New-Item -ItemType Directory -Path $BinDir -Force | Out-Null }
Copy-Item $ReleaseExe $InstalledExe -Force
$version = & $InstalledExe version
Pass $version

# 4. Apply config
Step "Configuring (echo=$Echo, webhook=$WebhookUrl)"
& $InstalledExe config echo $(if ($Echo) { "true" } else { "false" }) | Out-Null
if ($WebhookUrl) {
    & $InstalledExe config webhook $WebhookUrl | Out-Null
} else {
    & $InstalledExe config webhook "" | Out-Null
}
$cfg = & $InstalledExe config show
Write-Host $cfg

# 5. Start
Step "Starting daemon"
& $InstalledExe start
Start-Sleep -Seconds 2

$status = (& $InstalledExe status) -join "`n"
Write-Host $status
if ($status -notmatch "is running") { Fail "daemon failed to start" }
Pass "Daemon running"

# 6. Verify HTTP endpoints
Step "Verifying HTTP endpoints on port $Port"
try {
    $health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/health" -UseBasicParsing -TimeoutSec 5
    if (-not $health.ok) { Fail "health.ok = false" }
    Pass "GET /api/health -> ok=true accounts=$($health.accounts) pid=$($health.pid)"

    $accts = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/accounts" -UseBasicParsing -TimeoutSec 5
    Pass "GET /api/accounts -> $($accts.accounts.Count) accounts"
} catch { Fail "HTTP check: $_" }

# 7. Show recent log
Step "Recent log (last 20 lines)"
$logDir = Join-Path $StateDir "logs"
$latest = Get-ChildItem $logDir -Filter "*.log" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($latest) {
    Get-Content $latest.FullName -Tail 20 -Encoding UTF8
} else {
    Write-Host "  (no log file yet)"
}

# 8. Live tail prompt
Step "Setup complete"
Write-Host ""
Write-Host "Now scan WeChat QR (if needed) and send a message to the bot."
Write-Host "  Tail log:    Get-Content '$($latest.FullName)' -Wait -Encoding UTF8"
Write-Host "  Open GUI:    $InstalledExe console"
Write-Host "  Stop:        $InstalledExe stop"

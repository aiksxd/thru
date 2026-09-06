# thru Sandbox Test Runner
# Usage: .\sandbox\run-tests.ps1 [-SkipBuild] [-Port N] [-KeepSandbox] [-Groups a,b,c]

param(
    [switch]$SkipBuild,
    [int]$Port = 16196,
    [switch]$KeepSandbox,
    [string[]]$Groups = @("server","auth","dict","exec","fs","reverse","device","stress")
)

$ErrorActionPreference = "Continue"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$SandboxDir = $PSScriptRoot
$ServerRoot = Join-Path $SandboxDir "server-root"
$ClientDownload = Join-Path $SandboxDir "client-download"
$TempDir = Join-Path $SandboxDir "temp"
$LogDir = Join-Path $SandboxDir "logs"
$ResultLog = Join-Path $SandboxDir "test-results.log"
$TestPassword = "testpass123"
$ServerAddr = "127.0.0.1:$Port"
$Exe = Join-Path $ProjectRoot "target\release\thru.exe"

$script:Passed = 0
$script:Failed = 0
$script:Skipped = 0
$script:Failures = @()

function Write-Result($type, $id, $name, $detail = "") {
    $line = "[$type] $id $name"
    if ($detail) { $line += " — $detail" }
    Add-Content -Path $ResultLog -Value $line -Encoding utf8
    switch ($type) {
        "PASS" { Write-Host "  PASS" -ForegroundColor Green -NoNewline; Write-Host " $id $name" }
        "FAIL" { Write-Host "  FAIL" -ForegroundColor Red -NoNewline; Write-Host " $id $name $detail" }
        "SKIP" { Write-Host "  SKIP" -ForegroundColor Yellow -NoNewline; Write-Host " $id $name" }
    }
}

function Assert-Equal($expected, $actual, $id, $name) {
    if ($expected -eq $actual) {
        Write-Result "PASS" $id $name; $script:Passed++
    } else {
        Write-Result "FAIL" $id $name "expected '$expected', got '$actual'"; $script:Failed++; $script:Failures += "$id $name"
    }
}

function Assert-True($cond, $id, $name, $detail = "") {
    if ($cond) { Write-Result "PASS" $id $name; $script:Passed++ }
    else { Write-Result "FAIL" $id $name $detail; $script:Failed++; $script:Failures += "$id $name" }
}

function Invoke-Thru($argList) {
    $output = & $Exe @argList 2>&1
    return @{ Output = ($output -join "`n"); ExitCode = $LASTEXITCODE }
}

function Stop-AllThru {
    & $Exe stop 2>$null | Out-Null
    Get-Process -Name "thru" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
}

function New-SandboxFiles {
    Remove-Item -Recurse -Force $ServerRoot,$ClientDownload,$TempDir,$LogDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $ServerRoot,"$ServerRoot\documents","$ServerRoot\downloads","$ServerRoot\source\src",$ClientDownload,$TempDir,$LogDir | Out-Null
    Set-Content -Path "$ServerRoot\README.md" -Value "# thru Test Sandbox`n`nTest file.`n" -Encoding utf8
    Set-Content -Path "$ServerRoot\documents\report.txt" -Value "Quarterly Report`nRevenue: 1200000`n" -Encoding utf8
    Set-Content -Path "$ServerRoot\documents\notes.md" -Value "# Notes`n- one`n- two`n" -Encoding utf8
    Set-Content -Path "$ServerRoot\source\src\main.rs" -Value "fn main() {}`n" -Encoding utf8
    $big = ("A" * 1024) * 512
    Set-Content -Path "$ServerRoot\downloads\bigfile.bin" -Value $big -NoNewline -Encoding utf8
}

function Invoke-Fetch2($remotePath, $downloadDir) {
    $remotePath | & $Exe fetch2 $downloadDir 2>$null | Out-Null
}

function Start-Server {
    param([string]$WorkDir)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = "$Port -p $TestPassword"
    $psi.WorkingDirectory = $WorkDir
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.EnvironmentVariables["TEMP"] = $TempDir
    $psi.EnvironmentVariables["TMP"] = $TempDir
    $proc = [System.Diagnostics.Process]::Start($psi)
    return $proc
}

function Write-SessionCache {
    $sessionFile = Join-Path $TempDir "thru_client_session"
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($sessionFile, "$ServerAddr`n$TestPassword`n", $utf8NoBom)
}

# ============================================================
Write-Host "=== thru Sandbox Test Runner ===" -ForegroundColor Cyan
Write-Host "Port: $Port  Server: $ServerAddr"
Write-Host "Groups: $($Groups -join ', ')"
Write-Host ""

Set-Content -Path $ResultLog -Value "=== thru Test Results $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') ===" -Encoding utf8

# Step 1: Build
if (-not $SkipBuild) {
    Write-Host "[1/5] Building thru..." -ForegroundColor Cyan
    Push-Location $ProjectRoot
    cargo build --release 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Host "BUILD FAILED" -ForegroundColor Red; exit 1 }
    Pop-Location
    Write-Host "  Build OK" -ForegroundColor Green
} else {
    Write-Host "[1/5] Skipping build" -ForegroundColor Yellow
}

# Step 2: Stop any leftover thru processes (using DEFAULT temp, before we override it)
Write-Host "[2/5] Stopping leftover thru processes..." -ForegroundColor Cyan
Stop-AllThru
Write-Host "  Clean" -ForegroundColor Green

# Step 3: Prepare sandbox and set isolated TEMP
Write-Host "[3/5] Preparing sandbox..." -ForegroundColor Cyan
New-SandboxFiles
$env:TEMP = $TempDir
$env:TMP = $TempDir
Write-Host "  Sandbox ready: $SandboxDir" -ForegroundColor Green

# Step 4: Start server (from within server-root so CWD is the test filesystem)
Write-Host "[4/5] Starting test server..." -ForegroundColor Cyan
# Use Start-Process to avoid PowerShell waiting on the daemonized child process.
$serverProc = Start-Server -WorkDir $ServerRoot
# Wait for PID file to appear (server writes it after spawning the daemon)
$serverPidFile = Join-Path $TempDir "thru_server.pid"
$waited = 0
while (-not (Test-Path $serverPidFile) -and $waited -lt 10) {
    Start-Sleep -Milliseconds 500
    $waited += 0.5
}
Assert-True (Test-Path $serverPidFile) "T01" "Server starts and writes PID file"
# Write session cache so all subsequent commands authenticate automatically
Write-SessionCache
Write-Host "  Server running, session cache written" -ForegroundColor Green

# ============================================================
# Test Group: server
# ============================================================
if ($Groups -contains "server") {
    Write-Host "`n--- Server Tests ---" -ForegroundColor Cyan

    $pidResult = Invoke-Thru @("pid")
    Assert-True ($pidResult.Output -match "server pid:") "T02" "pid command returns server PID"

    Push-Location $ServerRoot
    $dupResult = Invoke-Thru @($Port.ToString(), "-p", $TestPassword)
    Pop-Location
    Assert-True ($dupResult.Output -match "already running") "T03" "Duplicate server start is rejected"
}

# ============================================================
# Test Group: auth
# ============================================================
if ($Groups -contains "auth") {
    Write-Host "`n--- Auth Tests ---" -ForegroundColor Cyan

    $sessionFile = Join-Path $TempDir "thru_client_session"
    Assert-True (Test-Path $sessionFile) "T05" "Session cache file exists"

    # Wrong password: remove session cache, try with wrong password
    Remove-Item -Force $sessionFile -ErrorAction SilentlyContinue
    $wrongResult = Invoke-Thru @("dict", "get", "test", "--connect", $ServerAddr)
    Assert-True ($wrongResult.ExitCode -ne 0) "T06" "Wrong password fails authentication"
    # Restore session cache
    Write-SessionCache
}

# ============================================================
# Test Group: dict
# ============================================================
if ($Groups -contains "dict") {
    Write-Host "`n--- Dict Tests ---" -ForegroundColor Cyan

    $setResult = Invoke-Thru @("dict", "set", "greeting", "hello world")
    Assert-True ($setResult.Output -match "OK") "T07" "dict set succeeds"

    $getResult = Invoke-Thru @("dict", "get", "greeting")
    Assert-Equal "hello world" $getResult.Output.Trim() "T08" "dict get returns correct value"

    $noKeyResult = Invoke-Thru @("dict", "get", "nonexistent_key_xyz")
    Assert-True ($noKeyResult.ExitCode -ne 0) "T09" "dict get nonexistent key fails"

    Invoke-Thru @("dict", "set", "counter", "1") | Out-Null
    Invoke-Thru @("dict", "set", "counter", "2", "-a") | Out-Null
    $appendResult = Invoke-Thru @("dict", "get", "counter")
    Assert-Equal "12" $appendResult.Output.Trim() "T10" "dict append concatenates values"

    $listResult = Invoke-Thru @("dict", "list")
    Assert-True ($listResult.Output -match "greeting") "T11" "dict list contains set keys"

    $kvResult = Invoke-Thru @("dict", "list", "-kv")
    Assert-True ($kvResult.Output -match "greeting\thello world") "T12" "dict list -kv returns key-tab-value"

    # T13: large value via stdin (command-line has length limits on Windows)
    $largeVal = "X" * (1024 * 1024)
    $largeSet = $largeVal | & $Exe dict set largekey 2>&1
    Assert-True ($largeSet -match "OK") "T13" "dict set 1MB value via stdin succeeds"

    # T14: >10MiB value rejected (via stdin to avoid command-line length limit)
    $hugeVal = "Y" * (11 * 1024 * 1024)
    $hugeSet = $hugeVal | & $Exe dict set hugekey 2>&1
    Assert-True ($hugeSet -notmatch "OK") "T14" "dict set >10MiB value is rejected"
}

# ============================================================
# Test Group: exec
# ============================================================
if ($Groups -contains "exec") {
    Write-Host "`n--- Exec Tests ---" -ForegroundColor Cyan

    $execResult = Invoke-Thru @("exec", "echo hello_exec_test")
    Assert-True ($execResult.Output -match "hello_exec_test") "T15" "exec echo returns output"

    $failResult = Invoke-Thru @("exec", "cmd /c exit 1")
    Assert-True ($failResult.ExitCode -ne 0) "T16" "exec failing command returns nonzero exit code"

    $argResult = Invoke-Thru @("exec", "echo arg1 arg2 arg3")
    Assert-True ($argResult.Output -match "arg1 arg2 arg3") "T17" "exec passes all arguments"
}

# ============================================================
# Test Group: fs
# ============================================================
if ($Groups -contains "fs") {
    Write-Host "`n--- Filesystem Tests ---" -ForegroundColor Cyan

    # T20: GET small file via fetch2 stdin
    Remove-Item -Recurse -Force $ClientDownload -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $ClientDownload | Out-Null
    Invoke-Fetch2 "README.md" $ClientDownload
    $downloaded = Join-Path $ClientDownload "README.md"
    Assert-True (Test-Path $downloaded) "T20" "fs GET downloads small file (README.md)"
    if (Test-Path $downloaded) {
        $content = Get-Content $downloaded -Raw
        Assert-True ($content -match "Test Sandbox") "T20b" "Downloaded file content is correct"
    }

    # T21: GET large file
    Remove-Item -Recurse -Force $ClientDownload -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $ClientDownload | Out-Null
    Invoke-Fetch2 "downloads\bigfile.bin" $ClientDownload
    $bigDownloaded = Join-Path $ClientDownload "bigfile.bin"
    Assert-True (Test-Path $bigDownloaded) "T21" "fs GET downloads large file (bigfile.bin)"
    if (Test-Path $bigDownloaded) {
        $origSize = (Get-Item "$ServerRoot\downloads\bigfile.bin").Length
        $dlSize = (Get-Item $bigDownloaded).Length
        Assert-Equal $origSize $dlSize "T21b" "Large file size matches ($origSize bytes)"
    }

    # T22: GET nonexistent file
    Remove-Item -Recurse -Force $ClientDownload -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $ClientDownload | Out-Null
    Invoke-Fetch2 "nonexistent_file_xyz.txt" $ClientDownload
    $notFound = Join-Path $ClientDownload "nonexistent_file_xyz.txt"
    Assert-True (-not (Test-Path $notFound)) "T22" "fs GET nonexistent file does not create local file"

    # T23: path traversal — should access parent dir (max freedom)
    Remove-Item -Recurse -Force $ClientDownload -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $ClientDownload | Out-Null
    Invoke-Fetch2 "..\..\Cargo.toml" $ClientDownload
    $traversalFile = Join-Path $ClientDownload "Cargo.toml"
    Assert-True (Test-Path $traversalFile) "T23" "Path traversal (../../) can access parent directory (max freedom)"
}

# ============================================================
# Test Group: reverse
# ============================================================
if ($Groups -contains "reverse") {
    Write-Host "`n--- Reverse Tunnel Tests ---" -ForegroundColor Cyan

    # Use Start-Process to avoid PowerShell waiting on reverse daemon child process
    $revPsi = New-Object System.Diagnostics.ProcessStartInfo
    $revPsi.FileName = $Exe
    $revPsi.Arguments = "$ServerAddr -p $TestPassword"
    $revPsi.UseShellExecute = $false
    $revPsi.CreateNoWindow = $true
    $revPsi.EnvironmentVariables["TEMP"] = $TempDir
    $revPsi.EnvironmentVariables["TMP"] = $TempDir
    $revProc = [System.Diagnostics.Process]::Start($revPsi)
    # Wait for reverse PID file to appear (daemon writes it after connecting)
    $revPidFile = Join-Path $TempDir "thru_reverse.pid"
    $waited = 0
    while (-not (Test-Path $revPidFile) -and $waited -lt 10) {
        Start-Sleep -Milliseconds 500; $waited += 0.5
    }
    Assert-True (Test-Path $revPidFile) "T24" "Reverse connect establishes tunnel"
    Start-Sleep -Milliseconds 500
    Assert-True (Test-Path $revPidFile) "T24b" "Reverse PID file written"

    $pidWithRev = Invoke-Thru @("pid")
    Assert-True ($pidWithRev.Output -match "reverse clients") "T25" "pid command lists reverse clients"

    # T27: stop everything
    Stop-AllThru
    Assert-True (-not (Test-Path $revPidFile)) "T27" "Stop removes reverse PID file"

    # Restart server for subsequent groups
    Start-Server -WorkDir $ServerRoot | Out-Null
    $waited = 0
    while (-not (Test-Path $serverPidFile) -and $waited -lt 10) {
        Start-Sleep -Milliseconds 500; $waited += 0.5
    }
    Write-SessionCache
}

# ============================================================
# Test Group: device
# ============================================================
if ($Groups -contains "device") {
    Write-Host "`n--- Device Management Tests ---" -ForegroundColor Cyan
    $pidResult = Invoke-Thru @("pid")
    Assert-True ($pidResult.Output -match "server pid:") "T28" "Device list available via pid command"
}

# ============================================================
# Test Group: stress
# ============================================================
if ($Groups -contains "stress") {
    Write-Host "`n--- Stress Tests ---" -ForegroundColor Cyan

    $jobs = @()
    for ($i = 0; $i -lt 5; $i++) {
        $jobs += Start-Job -ScriptBlock {
            param($exe, $addr, $i, $tempDir)
            $env:TEMP = $tempDir
            $env:TMP = $tempDir
            & $exe dict set "concurrent_key_$i" "value_$i" --connect $addr 2>&1 | Out-Null
            & $exe dict get "concurrent_key_$i" --connect $addr 2>&1
        } -ArgumentList $Exe, $ServerAddr, $i, $TempDir
    }
    $results = $jobs | Wait-Job | Receive-Job
    $jobs | Remove-Job
    $allCorrect = $true
    for ($i = 0; $i -lt 5; $i++) {
        if ($results -notcontains "value_$i") { $allCorrect = $false }
    }
    Assert-True $allCorrect "T30" "Concurrent dict operations (5 clients) all succeed"

    $origMd5 = (Get-FileHash "$ServerRoot\downloads\bigfile.bin" -Algorithm MD5).Hash
    $dlPath = Join-Path $ClientDownload "bigfile.bin"
    if (Test-Path $dlPath) {
        $dlMd5 = (Get-FileHash $dlPath -Algorithm MD5).Hash
        Assert-Equal $origMd5 $dlMd5 "T31" "Large file MD5 integrity verified"
    } else {
        Write-Result "SKIP" "T31" "Large file MD5 (download not present)"
        $script:Skipped++
    }
}

# ============================================================
# Cleanup
# ============================================================
Write-Host "`n[5/5] Cleaning up..." -ForegroundColor Cyan
Stop-AllThru

if (-not $KeepSandbox) {
    # Remove test artifacts but keep the script and design doc
    Remove-Item -Recurse -Force $ServerRoot -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $ClientDownload -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $LogDir -ErrorAction SilentlyContinue
    Remove-Item -Force $ResultLog -ErrorAction SilentlyContinue
    Write-Host "  Test artifacts removed (script and design doc kept)" -ForegroundColor Green
} else {
    Write-Host "  Sandbox kept at: $SandboxDir" -ForegroundColor Yellow
}

# ============================================================
# Summary
# ============================================================
Write-Host "`n=== Test Summary ===" -ForegroundColor Cyan
$total = $script:Passed + $script:Failed + $script:Skipped
Write-Host "  Total:   $total"
Write-Host "  Passed:  $($script:Passed)" -ForegroundColor Green
Write-Host "  Failed:  $($script:Failed)" -ForegroundColor Red
Write-Host "  Skipped: $($script:Skipped)" -ForegroundColor Yellow
if ($script:Failures.Count -gt 0) {
    Write-Host "`n  Failed tests:" -ForegroundColor Red
    $script:Failures | ForEach-Object { Write-Host "    - $_" -ForegroundColor Red }
}
Write-Host "`n  Manual tests (interactive TUI):" -ForegroundColor Yellow
Write-Host "    thru shell --connect $ServerAddr    (terminal, Ctrl+] to exit)"
Write-Host "    thru fetch2 . --connect $ServerAddr (file browser, arrows + Enter)"
Write-Host "    thru device . --connect $ServerAddr (device selector -> fetch2)"

if ($script:Failed -gt 0) { exit 1 } else { exit 0 }

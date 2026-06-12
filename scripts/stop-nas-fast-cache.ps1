param(
    [string]$ConfigPath = "",
    [string]$Mount = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

if ($ConfigPath) {
    $resolvedConfig = if ([System.IO.Path]::IsPathRooted($ConfigPath)) {
        $ConfigPath
    } else {
        Join-Path $repoRoot $ConfigPath
    }
    if (-not (Test-Path $resolvedConfig)) {
        throw "Config file not found: $resolvedConfig"
    }
    . $resolvedConfig
} else {
    $defaultConfig = Join-Path $repoRoot "config\local.ps1"
    if (Test-Path $defaultConfig) {
        . $defaultConfig
    }
}

if (-not $Mount) { $Mount = $env:NAS_FAST_CACHE_MOUNT }
if (-not $Mount) { throw "Mount is required. Use -Mount, config/local.ps1, or NAS_FAST_CACHE_MOUNT." }

$mountArg = "--mount $Mount"
$processes = Get-CimInstance Win32_Process |
    Where-Object {
        ($_.Name -eq "nas-fast-cache.exe" -and $_.CommandLine -like "*$mountArg*") -or
        ($_.Name -eq "powershell.exe" -and $_.CommandLine -like "*nas-fast-cache.exe*mount*" -and $_.CommandLine -like "*$mountArg*")
    }

foreach ($process in $processes) {
    Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
}

Start-Sleep -Seconds 1
$driveName = $Mount.TrimEnd(":")
$drive = Get-PSDrive -Name $driveName -ErrorAction SilentlyContinue
"stopped=$($processes.ProcessId -join ',') drive_present=$([bool]$drive)"

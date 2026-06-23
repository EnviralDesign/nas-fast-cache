param(
    [string]$ConfigPath = "",
    [string]$SourceRoot = "",
    [string]$CacheRoot = "",
    [string]$Mount = "",
    [string]$WinFspBin = "",
    [int]$Threads = 0,
    [int]$ChunkSizeMiB = 0,
    [double]$MaxCacheGB = 0,
    [double]$MaxAgeHours = 0,
    [double]$MinFreeGB = 0,
    [double]$MinEvictionAgeHours = 0,
    [int]$PruneIntervalSeconds = 0,
    [int]$StatsSeconds = 0,
    [switch]$DisableCacheWrites,
    [switch]$EnableSequentialConveyor,
    [switch]$EnableWrites,
    [string]$WritePrefix = "",
    [switch]$ReuseWriteHandles,
    [switch]$FlushAndPurgeOnCleanup,
    [switch]$Background
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$explicitParameters = @{}
foreach ($key in $PSBoundParameters.Keys) {
    $explicitParameters[$key] = $PSBoundParameters[$key]
}

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

foreach ($key in $explicitParameters.Keys) {
    Set-Variable -Name $key -Value $explicitParameters[$key]
}

if (-not $SourceRoot) { $SourceRoot = $env:NAS_FAST_CACHE_SOURCE_ROOT }
if (-not $CacheRoot) { $CacheRoot = $env:NAS_FAST_CACHE_CACHE_ROOT }
if (-not $Mount) { $Mount = $env:NAS_FAST_CACHE_MOUNT }
if (-not $WinFspBin) { $WinFspBin = $env:NAS_FAST_CACHE_WINFSP_BIN }
if (-not $WinFspBin) { $WinFspBin = "C:\Program Files (x86)\WinFsp\bin" }
if (-not $Threads) { $Threads = 8 }
if (-not $ChunkSizeMiB) { $ChunkSizeMiB = 8 }
if (-not $PruneIntervalSeconds) { $PruneIntervalSeconds = 300 }

if (-not $SourceRoot) { throw "SourceRoot is required. Use -SourceRoot, config/local.ps1, or NAS_FAST_CACHE_SOURCE_ROOT." }
if (-not $CacheRoot) { throw "CacheRoot is required. Use -CacheRoot, config/local.ps1, or NAS_FAST_CACHE_CACHE_ROOT." }
if (-not $Mount) { throw "Mount is required. Use -Mount, config/local.ps1, or NAS_FAST_CACHE_MOUNT." }
if ($EnableWrites -and -not $WritePrefix) {
    throw "-EnableWrites requires -WritePrefix to keep write scope explicit."
}

$exe = Join-Path $repoRoot "target\release\nas-fast-cache.exe"
if (-not (Test-Path $exe)) {
    throw "nas-fast-cache.exe not found. Build it with: cargo build --release"
}
if (-not (Test-Path $WinFspBin)) {
    throw "WinFsp bin path not found: $WinFspBin"
}

New-Item -ItemType Directory -Force $CacheRoot | Out-Null

$args = @(
    "mount",
    "--source-root", $SourceRoot,
    "--cache-root", $CacheRoot,
    "--mount", $Mount,
    "--threads", "$Threads",
    "--chunk-size-mib", "$ChunkSizeMiB"
)
if ($DisableCacheWrites) { $args += "--disable-cache-writes" }
if ($EnableSequentialConveyor) { $args += "--enable-sequential-conveyor" }
if ($MaxCacheGB -gt 0) {
    $args += "--max-cache-gb"
    $args += "$MaxCacheGB"
}
if ($MaxAgeHours -gt 0) {
    $args += "--max-age-hours"
    $args += "$MaxAgeHours"
}
if ($MinFreeGB -gt 0) {
    $args += "--min-free-gb"
    $args += "$MinFreeGB"
}
if ($MinEvictionAgeHours -gt 0) {
    $args += "--min-eviction-age-hours"
    $args += "$MinEvictionAgeHours"
}
$args += "--prune-interval-seconds"
$args += "$PruneIntervalSeconds"
if ($EnableWrites) {
    $args += "--enable-writes"
    $args += "--write-prefix"
    $args += $WritePrefix
}
if ($ReuseWriteHandles) { $args += "--reuse-write-handles" }
if ($FlushAndPurgeOnCleanup) { $args += "--flush-and-purge-on-cleanup" }

if ($Background) {
    $logDir = Join-Path $repoRoot "logs"
    New-Item -ItemType Directory -Force $logDir | Out-Null
    $log = Join-Path $logDir "nas-fast-cache-$($Mount.TrimEnd(':'))-mount.log"
    $encodedArgs = ($args | ForEach-Object { "'$($_.Replace("'", "''"))'" }) -join " "
    $cmd = "`$env:PATH = '$WinFspBin;' + `$env:PATH; `$env:NAS_CACHE_STATS_SECONDS = '$StatsSeconds'; & '$exe' $encodedArgs *> '$log'"
    $process = Start-Process -FilePath "powershell.exe" -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", $cmd) -WindowStyle Hidden -PassThru
    "started wrapper_pid=$($process.Id) mount=$Mount log=$log"
    return
}

$env:PATH = "$WinFspBin;$env:PATH"
$env:NAS_CACHE_STATS_SECONDS = "$StatsSeconds"
& $exe @args

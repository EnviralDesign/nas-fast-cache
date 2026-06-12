# nas-fast-cache

`nas-fast-cache` is a Windows-focused read-through cache for NAS-backed file
paths. It exposes a normal drive letter through WinFsp, fetches cold data from a
source share, and persists chunks into a local cache so repeated reads are served
from local storage.

The project is intentionally generic. Source shares, cache roots, mount letters,
and write prefixes are runtime configuration, not committed defaults.

## Status

This is purpose-built infrastructure, not a general cloud sync client.

- Fast cold reads depend on the upstream NAS/share throughput.
- Hot reads are served from local chunk files and should approach local disk speed.
- Writes are opt-in and can be restricted to a path prefix.
- The mounted filesystem requires WinFsp on Windows.

## Build

```powershell
cargo build --release
```

The executable will be:

```text
target\release\nas-fast-cache.exe
```

WinFsp must be installed for `mount`. The helper scripts default to the standard
WinFsp install path, but it can be overridden.

## Configure

Create an ignored local config:

```powershell
Copy-Item config\local.ps1.example config\local.ps1
notepad config\local.ps1
```

Required local values:

- `$SourceRoot`: UNC source root, for example `\\nas-host\share`.
- `$CacheRoot`: local cache root, ideally on a fast SSD.
- `$Mount`: drive letter to expose, for example `W:`.

Optional values:

- `$Threads`: WinFsp dispatcher threads. `8` is a good starting point.
- `$ChunkSizeMiB`: source/cache chunk size. `8` is the current default.
- `$MaxCacheGB`: prune oldest eligible cache groups when cache exceeds this size.
- `$MaxAgeHours`: prune cache groups older than this access age.
- `$MinFreeGB`: prune oldest eligible cache groups when the cache drive has less
  than this much free space.
- `$MinEvictionAgeHours`: protect recently accessed cache groups from automatic
  size/free-space pruning. `24` is a conservative starting point.
- `$PruneIntervalSeconds`: janitor wake interval. `300` gives a 5-minute cadence.
- `$EnableSequentialConveyor`: enables sequential read prefetching.
- `$EnableWrites`: enables writes through the mount.
- `$WritePrefix`: required when writes are enabled; set a subdirectory for scoped
  writes or `.` to explicitly allow writes across the whole source tree.
- `$ReuseWriteHandles`: keeps backing source write handles open across callbacks.

## Mount

```powershell
scripts\start-nas-fast-cache.ps1 -ConfigPath config\local.ps1 -Background
```

The helper writes logs under `logs/` when started in the background.

Stop the mount:

```powershell
scripts\stop-nas-fast-cache.ps1 -ConfigPath config\local.ps1
```

You can also pass parameters directly instead of using `config/local.ps1`:

```powershell
scripts\start-nas-fast-cache.ps1 `
  -SourceRoot '\\nas-host\share' `
  -CacheRoot 'C:\path\to\local-cache' `
  -Mount 'W:' `
  -Threads 8 `
  -EnableSequentialConveyor `
  -Background
```

## Windows Service

For a first-class persistent mount, install the foreground mount command under
NSSM from an elevated shell:

```powershell
scripts\install-service.ps1 `
  -ServiceName NasFastCache `
  -ConfigPath config\local.ps1 `
  -NssmPath 'C:\path\to\nssm.exe'
```

By default, NSSM installs services as LocalSystem. That is only suitable when the
configured `$SourceRoot` is readable by LocalSystem. If the source UNC path relies
on your Windows user's stored SMB credentials, configure the service to run as
that Windows user instead:

```powershell
scripts\install-service.ps1 `
  -ServiceName NasFastCache `
  -ConfigPath config\local.ps1 `
  -NssmPath 'C:\path\to\nssm.exe' `
  -ServiceUser '.\your-user' `
  -ServicePassword '<your-windows-password>'
```

The installer configures automatic startup, process restart on exit, and rotating
stdout/stderr logs under `logs/`.

Inspect the exact commands without changing anything:

```powershell
scripts\install-service.ps1 -NssmPath 'C:\path\to\nssm.exe' -DryRun
```

Remove the service:

```powershell
scripts\remove-service.ps1 -ServiceName NasFastCache -NssmPath 'C:\path\to\nssm.exe'
```

## Benchmark

Use a large file under the source root:

```powershell
target\release\nas-fast-cache.exe bench `
  --source-root '\\nas-host\share' `
  --cache-root 'C:\path\to\local-cache' `
  --path 'path\inside\share\large-file.bin' `
  --limit-mib 2048 `
  --passes 2 `
  --enable-sequential-conveyor
```

Pass 1 exercises cold source reads and cache writes. Pass 2 should read mostly
from local cache. Use `evict-file` to remove one file from cache before a clean
cold-read test.

## CLI

```text
nas-fast-cache bench
nas-fast-cache read
nas-fast-cache stat
nas-fast-cache evict-file
nas-fast-cache mount
```

Run `nas-fast-cache <command> --help` for command-specific flags.

## Cache Layout

Chunks are stored under:

```text
<cache-root>\chunks\<first-two-hash-chars>\<file-cache-key>\<chunk-index>.chunk
```

The cache key includes source path, source file size, source modified time, and
chunk size. If a source file changes, the cache naturally moves to a different
key.

## Pruning Policy

Automatic pruning is enabled when at least one of these policies is configured:

- `MaxCacheGB`: when cached chunks exceed this size, delete oldest eligible cache
  groups until the cache is back under the limit.
- `MaxAgeHours`: delete cache groups whose last access marker is older than this
  age.
- `MinFreeGB`: when the cache drive drops below this free-space threshold, delete
  oldest eligible cache groups until free space is back above the threshold.

Eviction happens at the source-file cache group level, not individual chunks.
Size and free-space pruning respect `MinEvictionAgeHours`, which protects very
recently accessed cache groups from being immediately evicted under normal
pressure. Age pruning uses the older of `MaxAgeHours` and `MinEvictionAgeHours`.

The janitor wakes every `PruneIntervalSeconds` and is also nudged after cache
chunk writes. Manual pruning is available without mounting:

```powershell
target\release\nas-fast-cache.exe prune `
  --cache-root 'C:\path\to\local-cache' `
  --max-cache-gb 200 `
  --max-age-hours 120 `
  --min-free-gb 100 `
  --min-eviction-age-hours 24
```

## Safety Notes

- Do not point `$CacheRoot` inside the mounted source tree.
- Keep private hostnames, IP addresses, share names, and local cache paths in
  `config/local.ps1` or your shell environment.
- Writes are disabled unless explicitly enabled.
- If writes are enabled, prefer a narrow `$WritePrefix`. Use `.` only when the
  mount is intentionally acting as the primary bidirectional path for the share.

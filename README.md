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

## Safety Notes

- Do not point `$CacheRoot` inside the mounted source tree.
- Keep private hostnames, IP addresses, share names, and local cache paths in
  `config/local.ps1` or your shell environment.
- Writes are disabled unless explicitly enabled.
- If writes are enabled, prefer a narrow `$WritePrefix`. Use `.` only when the
  mount is intentionally acting as the primary bidirectional path for the share.

# Performance Notes

These are implementation notes, not guaranteed benchmarks. Measure against your
own source share, cache drive, and workload.

Useful reference points:

- 2.5 GbE theoretical ceiling is about 298 MiB/s before protocol overhead.
- Raw SMB over a good 2.5 GbE direct link can land around 190-210 MiB/s for
  large sequential reads.
- A read-through mount should aim to keep cold reads near raw source throughput.
- Hot reads should be constrained mostly by local cache disk and filesystem
  overhead.

The current design uses:

- 8 MiB source/cache chunks by default.
- A per-open read window for small WinFsp callbacks.
- Optional sequential prefetch for source misses.
- Direct exact-range reads from cached chunk files for hot reads.
- Background atomic cache chunk writes using shared `Arc<[u8]>` buffers.

Known tradeoffs:

- Smaller chunks reduced cold-read performance in the original workload.
- Mounted cold reads may remain slower than raw SMB because WinFsp callback
  shape, cache writes, and source-side behavior all matter.
- Write support is intentionally conservative and should be tested with each
  target application before broad use.

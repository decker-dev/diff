# Benchmarks

Test repo: `../rebased` (IntelliJ + git integration), **505,988 commits**.
Machine: macOS 26.5, Apple Silicon (arm64). Build `--release` (thin LTO, opt 3).

## Engine: `log` (choosing the git engine)

Operation: read the N most recent commits from HEAD with author, summary, date and parents.
gix sorts by commit date (`ByCommitTime(NewestFirst)`); git2 uses the default order.

### log(1000)

| Engine | Time | vs libgit2 | vs git CLI |
|---|---|---|---|
| libgit2 (`git2`) | 417 ms | 1x | ~20x slower |
| gitoxide (`gix`) cold | 47 ms | 9x | ~2x slower |
| **gitoxide (`gix`) warm** | **17.4 ms** | **24x** | **matches/beats git (20 ms)** |
| git CLI (reference) | 20 ms | — | 1x |

### log(50,000)

| Engine | Time | vs libgit2 | vs git CLI |
|---|---|---|---|
| libgit2 (`git2`) | 1.48 s | 1x | 5x slower |
| **gitoxide (`gix`)** | **240 ms** (~208k commits/s) | **6.2x** | **1.25x FASTER than git (300 ms)** |
| git CLI (reference) | 300 ms | — | 1x |

**Takeaway:** Rust is not fast by itself — libgit2 was 18–150x slower than git.
The right engine (**gitoxide**) matches or beats the `git` CLI, and does so
**in-process** (no subprocess spawn like the JVM app). The 10x operation-latency
gate vs JVM Rebased is comfortably reachable.

### Key findings
- `Sort::TIME` in libgit2 costs ~8x over the default order.
- gix needs `object_cache_size` configured (the date walk looks up each commit twice).
- No commit-graph in the repo; gix still matches git. With a commit-graph it should improve further.

## GPUI shell (native window)

Native (Metal) window that loads the log with gix and renders it.

| Metric | rebased-rs | Rebased / IntelliJ (typical) | Factor |
|---|---|---|---|
| **RSS (real RAM)** | **~85 MB** (window) / ~225 MB (50k loaded) | ~700 MB – 1.5 GB | **~5–12x lighter** |
| **Binary size** | **7.8 MB** | 300 MB – 1 GB+ (bundles a JVM) | **~40–130x smaller** |
| **Idle CPU** | 0.0 % | JVM does background GC/indexing | — |

- Compiles and runs on stable Rust 1.89; gpui 0.2.2 from crates.io (no Zed repo needed).
- System requirement found: macOS 26 / Xcode 26 needs the *Metal Toolchain* separately
  (`xcodebuild -downloadComponent MetalToolchain`, ~688 MB, once).

## Diff of a commit (what happens on a click)

Diff of a commit vs its parent in the 505k-commit repo (200k+-file trees).

| Implementation | Time / click | Note |
|---|---|---|
| libgit2 `diff_tree_to_tree` | ~460 ms | slow object access (same as the log) |
| gix tree-diff (reopens repo) | ~83 ms | 5.5x better |
| **gix + warm cached repo** | **2–12 ms** (95 ms on the 1st click) | in-process; **faster than git CLI** (20 ms) |
| git CLI `git diff-tree -p` (ref) | ~20 ms | pays process startup each time |

**How:** gix does the tree-diff (which files changed) and reads the blobs; the line
diff uses `git2::Patch::from_buffers` over the bytes (in memory, no object store).
The gix repo is reused per thread with a warm object cache → clicks after the first
are instant. Keeping warm state in-process is exactly the advantage over spawning
`git` or running the JVM app.

### blame

| Implementation | Time | Note |
|---|---|---|
| gix blame, no commit-graph | ~2.0 s | walks the full file history |
| **gix blame + commit-graph** | **~0.73 s** | beats `git blame` (~0.94 s); computed off the UI thread |

## Pending
- **status**: with libgit2 it took ~4.16 s (git CLI ~3.83 s) → that's the cost of
  `stat`-ing the working tree (200k+ files), not the engine. The 10x here comes from
  **fsmonitor** (incremental status), not from changing libraries.

## Reproduce it yourself

```
cargo run -p app --example selftest --release -- /path/to/repo   # engine vs git CLI
./run.sh /path/to/repo                                           # the app (release)
```

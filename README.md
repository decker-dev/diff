# diff

A native, fast, lightweight git client in **Rust** — a from-scratch reimagining of
[Rebased](https://github.com/DetachHead/rebased) (the IntelliJ-platform git client),
aiming for the **same experience and capabilities** while being **dramatically
lighter and faster**.

- **Engine**: [gitoxide (`gix`)](https://github.com/GitoxideLabs/gitoxide) for the
  hot paths (log, diff, blame), `git2` for in-memory byte diffs and index ops.
- **UI**: [GPUI](https://www.gpui.rs/) (Zed's GPU-accelerated framework), Metal on macOS.
- **Architecture**: a `git-core` engine fully decoupled from the UI.

## Why

A real, fast git GUI without the weight of a full JVM IDE. Measured against the same
505k-commit repository:

| | diff | reference | factor |
|---|---|---|---|
| Binary size | **7.8 MB** | 300 MB–1 GB+ (JVM bundle) | **~40–130x smaller** |
| RAM | ~85–225 MB | ~1–2 GB (JVM) | **~5–12x lighter** |
| `log` 50k commits | **245 ms** | git CLI 379 ms | **1.5x faster than git** |
| commit diff (warm) | **~2 ms** | git CLI ~20 ms | in-process, instant |
| blame | **0.73 s** | git CLI 0.94 s | faster than git |

See [BENCHMARKS.md](BENCHMARKS.md) and [COMPARISON.md](COMPARISON.md). Headline lesson:
*Rust is not fast by itself* — performance comes from the right engine (gix +
commit-graph), keeping warm state in-process, and building in release.

## Features

| Area | Capabilities |
|---|---|
| **Start** | welcome screen, Open folder, clone from URL, recent repos, switch repo at runtime, window title + status bar |
| **History** | virtualized log graph, ref chips (branch/tag/HEAD), filter (message/author/hash) + go-to-hash, **file history** (`--follow`), **search in changes** (pickaxe `-S`/`-G`), right-click actions (checkout, new branch/tag here, reword, cherry-pick, revert, reset soft/mixed/hard, rebase from here, copy hash) |
| **Diff** | unified & side-by-side, **syntax highlighting** (12 languages, dependency-free), ignore-whitespace, prev/next change, per-hunk stage/unstage/revert, blame/annotate |
| **Commits** | status, stage/unstage (file and hunk), commit, amend, commit & push, sign-off, undo last, revert, cherry-pick, discard |
| **Branches/Tags** | list/create/checkout/delete/merge, rename, set upstream, rebase onto, tags (create/delete/push), worktrees |
| **Rebase 🎯** | interactive editor (pick/reword/squash/fixup/drop + reorder), autosquash, rebase onto, resume banner (continue/skip/abort) |
| **Remote** | fetch, fetch-all + prune, pull (rebase/merge), push (+ force-with-lease/upstream/tags), remotes add/remove |
| **GitHub** | clone, pull requests (list / checkout / open in browser), **in-app PR review**: threaded diff with inline comments, post comments/replies, approve / request changes / comment — via the `gh` CLI |
| **Conflicts** | 3-way viewer (base/ours/theirs) + take-ours/theirs |
| **Other** | stash (save/pop/apply/drop), reflog, submodules, built-in git console, settings, gitignore |

## Build & run

Requirements: Rust (stable), macOS with Xcode. On macOS 26 / Xcode 26 you also need
the Metal toolchain once: `xcodebuild -downloadComponent MetalToolchain`.

```sh
# Run the app (always release — the engine relies on optimization)
./run.sh /path/to/a/git/repo

# Headless engine self-test + benchmark vs the git CLI
cargo run -p app --example selftest --release -- /path/to/a/git/repo
```

## Status

Functional across all areas above, end-to-end: the engine is verified by the
self-test harness and wired into the UI. It now covers the day-to-day surface of
a full git client (open/clone/switch repos, log with filters and per-commit
actions, syntax-highlighted side-by-side diffs, hunk staging, file history and
pickaxe search, 3-way conflict resolution, rebase with resume, remotes,
stash/reflog/submodules, a git console, and full GitHub PR review with inline
comments via `gh`).

Remaining refinements: on-disk rebase abort/resume for in-app interactive rebase
(CLI rebases already resume via the banner), multi-line block-comment/string
highlighting carried across diff context boundaries, and continued visual polish.

Inspired by [Rebased](https://github.com/DetachHead/rebased); not affiliated with it
or JetBrains. Built natively from scratch.

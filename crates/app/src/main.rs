//! diff — native window (GPUI): log graph + diff viewer.
//! M2: virtualized log + DAG.  M3: click a commit → diff below.
//!
//! Usage:  diff [repo-path] [limit]   (default: . and 50000)

use git_core::blame::BlameLine;
use git_core::diff::{FileDiff, LineOrigin};
use git_core::graph::{compute_graph, RowGraph};
use git_core::rebase::{RebaseAction, RebaseResult, RebaseStep};
use git_core::{AheadBehind, BranchInfo, CommitInfo, RefKind, RefLabel, StatusEntry};
use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, ClipboardItem, Context,
    Entity, FocusHandle, KeyDownEvent, MouseButton, PathPromptOptions, Rgba, ScrollStrategy,
    SharedString, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use std::collections::HashMap;

const ROW_H: f32 = 24.0;
const LANE_W: f32 = 14.0;
const DOT: f32 = 8.0;
const MAX_LANES: usize = 14;
/// Row height in the diff/blame panels (monospace, virtualized).
const DIFF_ROW_H: f32 = 18.0;

/// IntelliJ "New UI" (dark) palette.
mod color {
    use gpui::{rgb, Rgba};
    pub fn bg() -> Rgba { rgb(0x1e1f22) }
    pub fn panel() -> Rgba { rgb(0x2b2d30) }
    pub fn line() -> Rgba { rgb(0x393b40) }
    pub fn row_line() -> Rgba { rgb(0x26282c) }
    pub fn fg() -> Rgba { rgb(0xbcbec4) }
    pub fn dim() -> Rgba { rgb(0x7a7e85) }
    pub fn accent() -> Rgba { rgb(0x548af7) }
    pub fn err() -> Rgba { rgb(0xff6b68) }
    pub fn sel() -> Rgba { rgb(0x2d4f7c) }
    pub fn hover() -> Rgba { rgb(0x2a2c31) }
    pub fn add_bg() -> Rgba { rgb(0x1d2e22) }
    pub fn add_fg() -> Rgba { rgb(0xa9c77d) }
    pub fn del_bg() -> Rgba { rgb(0x33232a) }
    pub fn del_fg() -> Rgba { rgb(0xe06c75) }
    pub fn ok() -> Rgba { rgb(0x59a869) }
    pub fn btn() -> Rgba { rgb(0x3c3f43) }
    pub fn tab_active() -> Rgba { rgb(0x1e1f22) }
}

/// Main app views (tabs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Log,
    Changes,
    Branches,
}

/// A row of the interactive rebase editor.
struct PlanRow {
    id: String,
    summary: String,
    action: RebaseAction,
}

/// Interactive rebase plan being edited.
struct RebasePlan {
    base: String,
    steps: Vec<PlanRow>,
}

/// A context menu open over a commit row in the log.
struct CommitMenu {
    ix: usize,
    pos: gpui::Point<gpui::Pixels>,
}

/// A modal text prompt (branch/tag name, reword message, go-to-hash, …).
struct Prompt {
    title: String,
    value: String,
    kind: PromptKind,
    focus: FocusHandle,
}

/// What a [`Prompt`] does on submit.
#[derive(Clone)]
enum PromptKind {
    BranchAt(String),
    TagAt(String),
    Reword(String),
    GoToHash,
    AddRemote,
    RenameBranch(String),
    SetUpstream(String),
}

/// A small colored chip for a ref (branch/tag/HEAD) on a commit row.
fn ref_chip(l: &RefLabel) -> impl IntoElement {
    let (bg, fg, prefix) = match l.kind {
        RefKind::Head => (color::accent(), rgb(0xffffff), "● "),
        RefKind::LocalBranch => (rgb(0x2f5b3f), rgb(0xcfe8c0), ""),
        RefKind::RemoteBranch => (rgb(0x3a3550), rgb(0xc8b8ec), ""),
        RefKind::Tag => (rgb(0x4a3f24), rgb(0xe6c46a), "⌗ "),
    };
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .whitespace_nowrap()
        .child(format!("{prefix}{}", l.name))
}

/// Graph branch colors (cycled by lane index).
fn branch_color(c: u32) -> Rgba {
    const P: [u32; 8] = [
        0x548af7, 0x59a869, 0xd9923a, 0xc9555f, 0x9876d6, 0x4aa3a3, 0xc2924a, 0xd06fb3,
    ];
    rgb(P[(c as usize) % P.len()])
}

/// A file's annotation (the bottom panel's blame mode).
struct BlameView {
    file: String,
    lines: Vec<BlameLine>,
    error: Option<String>,
    /// `true` while blame computes in the background (does not freeze the UI).
    loading: bool,
}

/// Flat diff row, for virtualizing with `uniform_list` (all ~same height).
enum DiffRow {
    /// File header (clickable → blame). `String` = path; the rest = label.
    File(String, String),
    Hunk(String),
    Line(LineOrigin, String),
}

/// Flattens the diff (files→hunks→lines) into rows for the virtualized list.
fn build_diff_rows(diff: &[FileDiff]) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for f in diff {
        let (add, del) = f.line_stats();
        let bin = if f.binary { "  [binario]" } else { "" };
        rows.push(DiffRow::File(
            f.path.clone(),
            format!("{}   +{add} −{del}{bin}   ⟶ blame", f.path),
        ));
        for h in &f.hunks {
            rows.push(DiffRow::Hunk(h.header.clone()));
            for l in &h.lines {
                rows.push(DiffRow::Line(l.origin, l.content.clone()));
            }
        }
    }
    rows
}

struct RebasedApp {
    repo_path: String,
    commits: Vec<CommitInfo>,
    graph: Vec<RowGraph>,
    graph_width: usize,
    error: Option<String>,
    selected: Option<usize>,
    diff: Vec<FileDiff>,
    /// Flattened diff for the virtualized list (derived from `diff`).
    diff_rows: Vec<DiffRow>,
    diff_error: Option<String>,
    /// If `Some`, the bottom panel shows blame instead of the diff.
    blame: Option<BlameView>,

    // ---- Views / navigation ----
    view: ViewMode,
    /// Toast with the result of the last operation (commit, push, etc.).
    op_msg: Option<String>,

    // ---- Local Changes (M4) ----
    status: Vec<StatusEntry>,
    status_error: Option<String>,
    /// Working-tree diff of the selected changed file.
    wt_rows: Vec<DiffRow>,
    wt_file: Option<String>,
    commit_msg: String,
    commit_focus: FocusHandle,

    // ---- Branches (M5) ----
    branches: Vec<BranchInfo>,
    new_branch: String,
    branch_focus: FocusHandle,

    // ---- Interactive rebase (M6) ----
    rebase: Option<RebasePlan>,

    // ---- Repo session / shell ----
    /// `false` until a repo is open (then the welcome screen shows).
    repo_loaded: bool,
    /// Display name of the open repo (window title + toolbar).
    repo_name: String,
    /// Set when the window title needs refreshing (applied during render).
    title_dirty: bool,
    /// Most-recently-opened repo paths (persisted to disk).
    recents: Vec<String>,
    /// Refs (branches/tags/HEAD) by commit id → colored chips in the log.
    refs: HashMap<String, Vec<RefLabel>>,
    /// Scroll handle for the log list (powers "go to hash"/"scroll to commit").
    log_scroll: UniformListScrollHandle,
    /// HEAD ahead/behind its upstream (status bar).
    ahead_behind: Option<AheadBehind>,

    // ---- Log: context menu, modal prompt, filter ----
    /// Right-click menu over a commit, if open.
    menu: Option<CommitMenu>,
    /// Modal text prompt, if open.
    prompt: Option<Prompt>,
    /// Set once after opening a prompt, to focus its input during render.
    focus_prompt: bool,
    /// Live log filter (substring of message/author/hash). Empty = no filter.
    log_filter: String,
    log_filter_focus: FocusHandle,
    /// Commit indices matching `log_filter` (only used when it's non-empty).
    filtered: Vec<usize>,
}

impl RebasedApp {
    /// Opens (or switches to) a repo at `path`, resetting all per-repo state.
    /// Synchronous: the log/graph/refs load is fast even on huge repos.
    fn load_repo(&mut self, path: &str) {
        let path = path.to_string();
        match git_core::gix_log(&path, 50_000) {
            Ok(commits) => {
                self.graph = compute_graph(&commits);
                self.graph_width = self
                    .graph
                    .iter()
                    .map(|r| r.width())
                    .max()
                    .unwrap_or(1)
                    .clamp(1, MAX_LANES);
                self.commits = commits;
                self.error = None;
                self.repo_loaded = true;
            }
            Err(e) => {
                self.commits.clear();
                self.graph.clear();
                self.error = Some(format!("Could not open repo: {e}"));
                self.repo_loaded = false;
                self.repo_path = path;
                return;
            }
        }
        self.repo_path = path.clone();

        // Refs (chips) + branches + ahead/behind.
        self.refs = git_core::Repo::open(&path)
            .and_then(|r| r.refs_by_commit())
            .unwrap_or_default();
        if let Ok(repo) = git_core::Repo::open(&path) {
            self.branches = repo.branches().unwrap_or_default();
            self.ahead_behind = repo.ahead_behind().ok();
        }

        // Reset per-commit / per-view state.
        self.selected = None;
        self.diff.clear();
        self.diff_rows.clear();
        self.diff_error = None;
        self.blame = None;
        self.status.clear();
        self.status_error = None;
        self.wt_file = None;
        self.wt_rows.clear();
        self.rebase = None;
        self.op_msg = None;
        self.view = ViewMode::Log;

        // Pre-warm the latest commit's diff and select it.
        if let Some(c) = self.commits.first() {
            let id = c.id.clone();
            if let Ok(files) = git_core::diff::commit_diff(&path, &id) {
                self.diff_rows = build_diff_rows(&files);
                self.diff = files;
                self.selected = Some(0);
            }
        }

        // Window title + recents.
        self.repo_name = repo_display_name(&path);
        self.title_dirty = true;
        let canon = std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.clone());
        self.recents.retain(|p| p != &canon);
        self.recents.insert(0, canon);
        self.recents.truncate(12);
        save_recents(&self.recents);
    }

    /// Native folder picker → open the chosen repo.
    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let path = p.to_string_lossy().into_owned();
                    let _ = this.update(cx, |t, cx| {
                        t.load_repo(&path);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Opens a repo from the recents list.
    fn open_recent(&mut self, path: String, cx: &mut Context<Self>) {
        self.load_repo(&path);
        cx.notify();
    }

    // ---- Log: context menu over a commit ----
    fn open_commit_menu(&mut self, ix: usize, pos: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.menu = Some(CommitMenu { ix, pos });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn copy_hash(&mut self, id: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
        self.op_msg = Some(format!("Copied {}", short(&id)));
        self.menu = None;
        cx.notify();
    }

    fn checkout_revision(&mut self, id: String, cx: &mut Context<Self>) {
        self.menu = None;
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.checkout_commit(&id))
            .map(|_| format!("checkout {}", short(&id)));
        self.branch_op(r, cx);
    }

    fn reset_here(&mut self, id: String, mode: git_core::ResetMode, cx: &mut Context<Self>) {
        self.menu = None;
        let label = format!("reset {} {}", reset_label(mode), short(&id));
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.reset(&id, mode))
            .map(|_| label);
        self.branch_op(r, cx);
    }

    fn cherry_pick_at(&mut self, id: String, cx: &mut Context<Self>) {
        self.menu = None;
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.cherry_pick(&id))
            .map(|_| format!("cherry-pick {} (staged)", short(&id)));
        self.branch_op(r, cx);
    }

    fn revert_at(&mut self, id: String, cx: &mut Context<Self>) {
        self.menu = None;
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.revert_commit(&id))
            .map(|_| format!("revert {} (staged)", short(&id)));
        self.branch_op(r, cx);
    }

    fn rebase_from(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.menu = None;
        self.start_rebase(ix, cx);
    }

    // ---- Modal text prompt (reused across the app) ----
    fn open_prompt(&mut self, title: &str, initial: &str, kind: PromptKind, cx: &mut Context<Self>) {
        self.prompt = Some(Prompt {
            title: title.to_string(),
            value: initial.to_string(),
            kind,
            focus: cx.focus_handle(),
        });
        self.focus_prompt = true;
        self.menu = None;
        cx.notify();
    }

    fn cancel_prompt(&mut self, cx: &mut Context<Self>) {
        self.prompt = None;
        cx.notify();
    }

    fn on_prompt_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" => {
                self.submit_prompt(cx);
                return;
            }
            "escape" => {
                self.prompt = None;
                cx.notify();
                return;
            }
            _ => {}
        }
        if let Some(p) = &mut self.prompt {
            edit_key(&mut p.value, ev, false);
        }
        cx.notify();
    }

    fn submit_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.prompt.take() else { return };
        let v = p.value.trim().to_string();
        match p.kind {
            PromptKind::BranchAt(id) => {
                if v.is_empty() {
                    return;
                }
                let r = git_core::Repo::open(&self.repo_path)
                    .and_then(|repo| repo.create_branch_at(&v, &id))
                    .map(|_| format!("branch {v}"));
                self.branch_op(r, cx);
            }
            PromptKind::TagAt(id) => {
                if v.is_empty() {
                    return;
                }
                let r = git_core::Repo::open(&self.repo_path)
                    .and_then(|repo| repo.create_tag(&v, &id, None))
                    .map(|_| format!("tag {v}"));
                self.branch_op(r, cx);
            }
            PromptKind::Reword(id) => self.reword_commit(&id, &v, cx),
            PromptKind::GoToHash => self.go_to_hash(&v, cx),
            PromptKind::AddRemote => {
                let mut it = v.split_whitespace();
                if let (Some(name), Some(url)) = (it.next(), it.next()) {
                    let r = git_core::Repo::open(&self.repo_path)
                        .and_then(|repo| repo.add_remote(name, url))
                        .map(|_| format!("remote add {name}"));
                    self.branch_op(r, cx);
                } else {
                    self.op_msg = Some("Format: <name> <url>".into());
                    cx.notify();
                }
            }
            PromptKind::RenameBranch(old) => {
                if v.is_empty() {
                    return;
                }
                let r = git_core::Repo::open(&self.repo_path)
                    .and_then(|repo| repo.rename_branch(&old, &v))
                    .map(|_| format!("rename {old} → {v}"));
                self.branch_op(r, cx);
            }
            PromptKind::SetUpstream(branch) => {
                let up = if v.is_empty() { None } else { Some(v.as_str()) };
                let r = git_core::Repo::open(&self.repo_path)
                    .and_then(|repo| repo.set_upstream(&branch, up))
                    .map(|_| format!("upstream {branch} → {v}"));
                self.branch_op(r, cx);
            }
        }
    }

    fn go_to_hash(&mut self, q: &str, cx: &mut Context<Self>) {
        let q = q.trim();
        if let Some(ix) = self.commits.iter().position(|c| c.id.starts_with(q)) {
            self.log_scroll.scroll_to_item(ix, ScrollStrategy::Center);
            self.select(ix, cx);
        } else {
            self.op_msg = Some(format!("No commit matching {q}"));
            cx.notify();
        }
    }

    fn reword_commit(&mut self, id: &str, msg: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.commits.iter().position(|c| c.id == id) else { return };
        let Some(base) = self.commits[ix].parents.first().cloned() else {
            self.op_msg = Some("Cannot reword the root commit".into());
            cx.notify();
            return;
        };
        let steps: Vec<RebaseStep> = self.commits[0..=ix]
            .iter()
            .rev()
            .map(|c| RebaseStep {
                commit: c.id.clone(),
                action: if c.id == id {
                    RebaseAction::Reword(msg.to_string())
                } else {
                    RebaseAction::Pick
                },
            })
            .collect();
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.rebase_interactive(&base, &steps));
        self.op_msg = Some(match r {
            Ok(RebaseResult::Done(h)) => format!("✓ reworded → {}", short(&h)),
            Ok(RebaseResult::Conflict(c)) => format!("✗ conflict at {}", short(&c)),
            Err(e) => format!("✗ {e}"),
        });
        self.reload_log();
        cx.notify();
    }

    // ---- Log filter / search ----
    fn on_filter_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "enter" {
            let q = self.log_filter.trim().to_string();
            if q.len() >= 4 && q.chars().all(|c| c.is_ascii_hexdigit()) {
                self.go_to_hash(&q, cx);
            }
            return;
        }
        if ev.keystroke.key == "escape" {
            self.log_filter.clear();
        } else {
            edit_key(&mut self.log_filter, ev, false);
        }
        self.recompute_filter();
        cx.notify();
    }

    fn recompute_filter(&mut self) {
        let q = self.log_filter.trim().to_lowercase();
        if q.is_empty() {
            self.filtered.clear();
            return;
        }
        self.filtered = self
            .commits
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.summary.to_lowercase().contains(&q)
                    || c.author.to_lowercase().contains(&q)
                    || c.id.starts_with(&q)
            })
            .map(|(i, _)| i)
            .collect();
    }

    /// Selects a commit and loads its diff (reopens the repo: ~1 ms).
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.selected == Some(ix) {
            return;
        }
        self.selected = Some(ix);
        self.blame = None; // switching commit returns to the diff view
        let id = self.commits[ix].id.clone();
        match git_core::diff::commit_diff(&self.repo_path, &id) {
            Ok(files) => {
                self.diff_rows = build_diff_rows(&files);
                self.diff = files;
                self.diff_error = None;
            }
            Err(e) => {
                self.diff.clear();
                self.diff_rows.clear();
                self.diff_error = Some(e);
            }
        }
        cx.notify();
    }

    /// Shows blame for `file` at the selected commit. Computes in the background
    /// (blame can take ~1s on deep history) so it does NOT freeze the UI.
    fn show_blame(&mut self, file: String, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let id = self.commits[ix].id.clone();
        let repo_path = self.repo_path.clone();

        // Immediate "loading" state (the UI stays responsive).
        self.blame = Some(BlameView {
            file: file.clone(),
            lines: Vec::new(),
            error: None,
            loading: true,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (path, id2, file2) = (repo_path.clone(), id.clone(), file.clone());
            let result = cx
                .background_executor()
                .spawn(async move { git_core::blame::blame_file(&path, &id2, &file2) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Apply only if we're still waiting for THIS file's blame.
                let waiting = matches!(&this.blame, Some(bv) if bv.file == file && bv.loading);
                if waiting {
                    this.blame = Some(match result {
                        Ok(lines) => BlameView { file, lines, error: None, loading: false },
                        Err(e) => BlameView { file, lines: Vec::new(), error: Some(e), loading: false },
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Returns from blame mode to the diff view.
    fn clear_blame(&mut self, cx: &mut Context<Self>) {
        self.blame = None;
        cx.notify();
    }

    // ---- View navigation ----
    fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        self.view = view;
        self.op_msg = None;
        match view {
            ViewMode::Changes => self.refresh_status(cx),
            ViewMode::Branches => self.refresh_branches(cx),
            ViewMode::Log => cx.notify(),
        }
    }

    // ---- Local Changes (M4) ----
    /// Loads status (+ the selected file's diff) IN THE BACKGROUND: on huge
    /// repos status scans 200k+ files (~4s) and must not freeze the UI.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let path = self.repo_path.clone();
        let wt_file = self.wt_file.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let repo = git_core::Repo::open(&path).map_err(|e| e.to_string())?;
                    let status = repo.status().map_err(|e| e.to_string())?;
                    let rows = match &wt_file {
                        Some(f) => {
                            let un = git_core::diff::workdir_diff(&path, false).unwrap_or_default();
                            let st = git_core::diff::workdir_diff(&path, true).unwrap_or_default();
                            un.iter()
                                .chain(st.iter())
                                .find(|x| &x.path == f)
                                .map(|x| build_diff_rows(std::slice::from_ref(x)))
                                .unwrap_or_default()
                        }
                        None => Vec::new(),
                    };
                    Ok::<(Vec<StatusEntry>, Vec<DiffRow>), String>((status, rows))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((s, rows)) => {
                        this.status = s;
                        this.status_error = None;
                        if this.wt_file.is_some() {
                            this.wt_rows = rows;
                        }
                    }
                    Err(e) => {
                        this.status.clear();
                        this.status_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_wt_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.wt_file = Some(path);
        self.refresh_status(cx);
    }

    fn toggle_stage(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path).and_then(|repo| {
            if staged {
                repo.unstage(&path)
            } else {
                repo.stage(&path)
            }
        });
        if let Err(e) = r {
            self.op_msg = Some(format!("✗ {e}"));
        }
        self.refresh_status(cx);
    }

    fn do_commit(&mut self, amend: bool, cx: &mut Context<Self>) {
        let msg = self.commit_msg.trim().to_string();
        if msg.is_empty() && !amend {
            self.op_msg = Some("Write a commit message".into());
            cx.notify();
            return;
        }
        let r = git_core::Repo::open(&self.repo_path).and_then(|repo| {
            if amend {
                repo.amend(&msg)
            } else {
                repo.commit(&msg)
            }
        });
        match r {
            Ok(id) => {
                self.op_msg = Some(format!("✓ committed {}", short(&id)));
                self.commit_msg.clear();
                self.reload_log();
            }
            Err(e) => self.op_msg = Some(format!("✗ {e}")),
        }
        self.refresh_status(cx);
    }

    fn on_commit_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        edit_key(&mut self.commit_msg, ev, true);
        cx.notify();
    }

    // ---- Branches (M5) ----
    fn refresh_branches(&mut self, cx: &mut Context<Self>) {
        match git_core::Repo::open(&self.repo_path).and_then(|r| r.branches()) {
            Ok(b) => self.branches = b,
            Err(e) => self.op_msg = Some(format!("✗ {e}")),
        }
        cx.notify();
    }

    fn branch_op(&mut self, r: Result<String, git_core::Error>, cx: &mut Context<Self>) {
        self.op_msg = Some(match r {
            Ok(m) => format!("✓ {m}"),
            Err(e) => format!("✗ {e}"),
        });
        self.reload_log();
        self.refresh_branches(cx);
    }

    fn do_checkout(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.checkout_branch(&name))
            .map(|_| format!("checkout {name}"));
        self.branch_op(r, cx);
    }

    fn do_merge(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.merge_branch(&name))
            .map(|o| format!("merge {name}: {o:?}"));
        self.branch_op(r, cx);
    }

    fn do_delete_branch(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.delete_branch(&name))
            .map(|_| format!("deleted {name}"));
        self.branch_op(r, cx);
    }

    fn do_create_branch(&mut self, cx: &mut Context<Self>) {
        let name = self.new_branch.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.new_branch.clear();
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.create_branch(&name))
            .map(|_| format!("created {name}"));
        self.branch_op(r, cx);
    }

    fn on_branch_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "enter" {
            self.do_create_branch(cx);
            return;
        }
        edit_key(&mut self.new_branch, ev, false);
        cx.notify();
    }

    /// Reloads the log after an operation that may have changed HEAD.
    fn reload_log(&mut self) {
        if let Ok(commits) = git_core::gix_log(&self.repo_path, 50_000) {
            self.graph = compute_graph(&commits);
            self.graph_width = self
                .graph
                .iter()
                .map(|r| r.width())
                .max()
                .unwrap_or(1)
                .clamp(1, MAX_LANES);
            self.commits = commits;
        }
        // Refresh ref chips + ahead/behind so the log and status bar stay current.
        self.refs = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.refs_by_commit())
            .unwrap_or_default();
        if let Ok(repo) = git_core::Repo::open(&self.repo_path) {
            self.ahead_behind = repo.ahead_behind().ok();
        }
    }

    // ---- Remote (M7) / Stash (M8) ----
    /// Runs a network operation in the background (does not freeze the UI).
    fn run_remote(&mut self, op: &'static str, cx: &mut Context<Self>) {
        self.op_msg = Some(format!("{op}…"));
        cx.notify();
        let path = self.repo_path.clone();
        let branch = self
            .branches
            .iter()
            .find(|b| b.is_head)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| "HEAD".into());
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let repo = git_core::Repo::open(&path).map_err(|e| e.to_string())?;
                    match op {
                        "fetch" => repo.fetch("origin"),
                        "pull" => repo.pull(),
                        "push" => repo.push("origin", &branch),
                        _ => Ok(String::new()),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.op_msg = Some(match res {
                    Ok(o) => format!("✓ {op}: {}", first_line(&o)),
                    Err(e) => format!("✗ {op}: {}", first_line(&e)),
                });
                this.reload_log();
                cx.notify();
            });
        })
        .detach();
    }

    fn do_stash(&mut self, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|mut repo| repo.stash_save("WIP (diff)"));
        self.op_msg = Some(match r {
            Ok(id) => format!("✓ stash {}", short(&id)),
            Err(e) => format!("✗ {e}"),
        });
        self.refresh_status(cx);
    }

    // ---- Interactive rebase (M6) ----
    /// Opens the rebase editor: replays commits from `ix`'s parent up to HEAD.
    fn start_rebase(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(ix) else { return };
        let Some(base) = commit.parents.first().cloned() else {
            self.op_msg = Some("Cannot rebase the root commit".into());
            cx.notify();
            return;
        };
        let steps = self.commits[0..=ix]
            .iter()
            .rev()
            .map(|c| PlanRow {
                id: c.id.clone(),
                summary: c.summary.clone(),
                action: RebaseAction::Pick,
            })
            .collect();
        self.rebase = Some(RebasePlan { base, steps });
        cx.notify();
    }

    /// Cycles a step's action: Pick → Squash → Fixup → Drop → Pick.
    fn rebase_cycle(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.rebase {
            if let Some(row) = p.steps.get_mut(i) {
                row.action = match row.action {
                    RebaseAction::Pick => RebaseAction::Squash,
                    RebaseAction::Squash => RebaseAction::Fixup,
                    RebaseAction::Fixup => RebaseAction::Drop,
                    _ => RebaseAction::Pick,
                };
            }
        }
        cx.notify();
    }

    /// Reorders a plan step.
    fn rebase_move(&mut self, i: usize, up: bool, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.rebase {
            let j = if up {
                i.checked_sub(1)
            } else if i + 1 < p.steps.len() {
                Some(i + 1)
            } else {
                None
            };
            if let Some(j) = j {
                p.steps.swap(i, j);
            }
        }
        cx.notify();
    }

    /// Runs the rebase plan.
    fn apply_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = self.rebase.take() else { return };
        let steps: Vec<RebaseStep> = plan
            .steps
            .iter()
            .map(|r| RebaseStep {
                commit: r.id.clone(),
                action: r.action.clone(),
            })
            .collect();
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.rebase_interactive(&plan.base, &steps));
        self.op_msg = Some(match r {
            Ok(RebaseResult::Done(id)) => format!("✓ rebase OK → {}", short(&id)),
            Ok(RebaseResult::Conflict(c)) => format!("✗ conflict applying {}", short(&c)),
            Err(e) => format!("✗ {e}"),
        });
        self.reload_log();
        cx.notify();
    }

    fn cancel_rebase(&mut self, cx: &mut Context<Self>) {
        self.rebase = None;
        cx.notify();
    }
}

/// Applies a key to a text buffer (minimal input).
fn edit_key(buf: &mut String, ev: &KeyDownEvent, multiline: bool) {
    match ev.keystroke.key.as_str() {
        "backspace" => {
            buf.pop();
        }
        "enter" => {
            if multiline {
                buf.push('\n');
            }
        }
        "space" => buf.push(' '),
        _ => {
            if let Some(c) = &ev.keystroke.key_char {
                let m = &ev.keystroke.modifiers;
                if !c.is_empty() && !m.platform && !m.control {
                    buf.push_str(c);
                }
            }
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("ok").to_string()
}

/// Last path component of the (canonicalized) repo path, for title/toolbar.
fn repo_display_name(path: &str) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    canon
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| canon.to_string_lossy().into_owned())
}

/// `~/.config/diff/recents.txt` — the persisted recent-repos list.
fn recents_file() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join(".config").join("diff");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("recents.txt"))
}

fn load_recents() -> Vec<String> {
    recents_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn save_recents(recents: &[String]) {
    if let Some(p) = recents_file() {
        let _ = std::fs::write(p, recents.join("\n"));
    }
}

fn short(id: &str) -> String {
    id.get(..7).unwrap_or(id).to_string()
}

fn reset_label(mode: git_core::ResetMode) -> &'static str {
    match mode {
        git_core::ResetMode::Soft => "--soft",
        git_core::ResetMode::Mixed => "--mixed",
        git_core::ResetMode::Hard => "--hard",
    }
}

/// Formats Unix seconds to `YYYY-MM-DD` (Hinnant's civil algorithm, no deps).
fn fmt_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

impl Render for RebasedApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Apply a pending window-title change (cheap; only when it changed).
        if self.title_dirty {
            let title = if self.repo_name.is_empty() {
                "diff".to_string()
            } else {
                format!("{} — diff", self.repo_name)
            };
            window.set_window_title(&title);
            self.title_dirty = false;
        }

        // No repo open yet → welcome screen.
        if !self.repo_loaded {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .bg(color::bg())
                .text_color(color::fg())
                .child(self.render_welcome(cx));
        }

        // Focus a freshly-opened modal prompt's input.
        if self.focus_prompt {
            if let Some(p) = &self.prompt {
                window.focus(&p.focus);
            }
            self.focus_prompt = false;
        }

        let toolbar = self.render_toolbar(cx);
        let body = if self.rebase.is_some() {
            self.render_rebase_editor(cx)
        } else {
            match self.view {
                ViewMode::Log => self.render_log(cx),
                ViewMode::Changes => self.render_changes(cx),
                ViewMode::Branches => self.render_branches(cx),
            }
        };
        let overlays = self.render_overlays(cx);
        let toast = self.op_msg.clone().map(|m| {
            div()
                .w_full()
                .px_3()
                .py_1()
                .bg(color::panel())
                .border_t_1()
                .border_color(color::line())
                .text_sm()
                .text_color(color::dim())
                .child(m)
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(color::bg())
            .text_color(color::fg())
            .child(toolbar)
            .child(body)
            .children(toast)
            .children(overlays)
    }
}

impl RebasedApp {
    /// Welcome screen (shown when no repo is open): logo, Open button, recents.
    fn render_welcome(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut recents = div().flex().flex_col().gap_1().w(px(620.0));
        if self.recents.is_empty() {
            recents = recents.child(
                div()
                    .px_3()
                    .text_color(color::dim())
                    .text_sm()
                    .child("No recent repositories"),
            );
        } else {
            for path in &self.recents {
                let p = path.clone();
                let e2 = e.clone();
                let name = repo_display_name(path);
                recents = recents.child(
                    div()
                        .id(SharedString::from(format!("recent-{path}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .w_full()
                        .px_3()
                        .h(px(38.0))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(color::hover()))
                        .on_click(move |_, _, app| {
                            let p = p.clone();
                            e2.update(app, |t, cx| t.open_recent(p, cx));
                        })
                        .child(div().flex_none().w(px(160.0)).whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(name))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(color::dim())
                                .text_sm()
                                .child(path.clone()),
                        ),
                );
            }
        }
        let err = self
            .error
            .clone()
            .map(|m| div().px_3().text_color(color::err()).text_sm().child(m));

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .size_full()
            .child(div().text_color(color::accent()).text_2xl().child("⎇ diff"))
            .child(div().text_color(color::dim()).child("A fast, native git client"))
            .child(
                div().mt_2().child(btn("welcome-open", "Open repository…", {
                    let e = e.clone();
                    move |app| {
                        e.update(app, |t, cx| t.open_dialog(cx));
                    }
                })),
            )
            .children(err)
            .child(div().mt_4().text_color(color::dim()).text_sm().child("Recent"))
            .child(recents)
            .into_any_element()
    }

    /// Floating overlays (commit context menu, modal prompt) drawn on top.
    fn render_overlays(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        if let Some(menu) = &self.menu {
            out.push(self.render_commit_menu(menu, cx));
        }
        if let Some(prompt) = &self.prompt {
            out.push(self.render_prompt(prompt, cx));
        }
        out
    }

    /// Right-click context menu over a commit (actions from the log).
    fn render_commit_menu(&self, menu: &CommitMenu, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let Some(commit) = self.commits.get(menu.ix) else {
            return div().into_any_element();
        };
        let id = commit.id.clone();
        let summary = commit.summary.clone();
        let ix = menu.ix;

        let sep = || div().w_full().h(px(1.0)).my_1().bg(color::line());

        let panel = div()
            .flex()
            .flex_col()
            .w(px(232.0))
            .bg(color::panel())
            .border_1()
            .border_color(color::line())
            .rounded_md()
            .py_1()
            .child(menu_row("m-co", "Checkout this commit", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.checkout_revision(id.clone(), cx))
            }))
            .child(menu_row("m-branch", "New branch here…", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.open_prompt("New branch at this commit", "", PromptKind::BranchAt(id.clone()), cx))
            }))
            .child(menu_row("m-tag", "New tag here…", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.open_prompt("New tag at this commit", "", PromptKind::TagAt(id.clone()), cx))
            }))
            .child(menu_row("m-reword", "Reword…", {
                let (e, id, summary) = (e.clone(), id.clone(), summary.clone());
                move |app| e.update(app, |t, cx| t.open_prompt("Reword commit message", &summary, PromptKind::Reword(id.clone()), cx))
            }))
            .child(sep())
            .child(menu_row("m-cp", "Cherry-pick onto current", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.cherry_pick_at(id.clone(), cx))
            }))
            .child(menu_row("m-revert", "Revert this commit", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.revert_at(id.clone(), cx))
            }))
            .child(menu_row("m-rebase", "Rebase from here…", {
                let e = e.clone();
                move |app| e.update(app, |t, cx| t.rebase_from(ix, cx))
            }))
            .child(sep())
            .child(menu_row("m-soft", "Reset --soft to here", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.reset_here(id.clone(), git_core::ResetMode::Soft, cx))
            }))
            .child(menu_row("m-mixed", "Reset --mixed to here", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.reset_here(id.clone(), git_core::ResetMode::Mixed, cx))
            }))
            .child(menu_row("m-hard", "Reset --hard to here", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.reset_here(id.clone(), git_core::ResetMode::Hard, cx))
            }))
            .child(sep())
            .child(menu_row("m-copy", "Copy commit hash", {
                let (e, id) = (e.clone(), id.clone());
                move |app| e.update(app, |t, cx| t.copy_hash(id.clone(), cx))
            }));

        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(MouseButton::Left, {
                let e = e.clone();
                move |_, _, app| e.update(app, |t, cx| t.close_menu(cx))
            })
            .on_mouse_down(MouseButton::Right, {
                let e = e.clone();
                move |_, _, app| e.update(app, |t, cx| t.close_menu(cx))
            });

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(gpui::anchored().position(menu.pos).child(panel))
            .into_any_element()
    }

    /// Modal text prompt (branch/tag name, reword, etc.).
    fn render_prompt(&self, p: &Prompt, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(rgb(0x000000))
            .opacity(0.4)
            .on_mouse_down(MouseButton::Left, {
                let e = e.clone();
                move |_, _, app| e.update(app, |t, cx| t.cancel_prompt(cx))
            });

        let panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(440.0))
            .bg(color::panel())
            .border_1()
            .border_color(color::line())
            .rounded_md()
            .p_3()
            .child(div().text_color(color::fg()).child(p.title.clone()))
            .child(
                div()
                    .id("prompt-input")
                    .w_full()
                    .min_h(px(30.0))
                    .px_2()
                    .py_1()
                    .bg(color::bg())
                    .rounded_md()
                    .border_1()
                    .border_color(color::accent())
                    .track_focus(&p.focus)
                    .key_context("prompt")
                    .on_key_down(cx.listener(Self::on_prompt_key))
                    .font_family("Menlo")
                    .text_sm()
                    .text_color(if p.value.is_empty() { color::dim() } else { color::fg() })
                    .child(if p.value.is_empty() {
                        "type…".to_string()
                    } else {
                        format!("{}▏", p.value)
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(btn("prompt-cancel", "Cancel", {
                        let e = e.clone();
                        move |app| e.update(app, |t, cx| t.cancel_prompt(cx))
                    }))
                    .child(btn("prompt-ok", "OK", {
                        let e = e.clone();
                        move |app| e.update(app, |t, cx| t.submit_prompt(cx))
                    })),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(backdrop)
            .child(panel)
            .into_any_element()
    }

    /// Top bar: tabs (Log/Changes/Branches) + network/stash actions.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let view = self.view;
        let tab = {
            let e = e.clone();
            move |label: &str, mode: ViewMode| {
                let e = e.clone();
                let active = view == mode;
                div()
                    .id(SharedString::from(format!("tab-{label}")))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .bg(if active { color::tab_active() } else { color::panel() })
                    .text_color(if active { color::accent() } else { color::dim() })
                    .hover(|s| s.bg(color::hover()))
                    .on_click(move |_, _, app| {
                        e.update(app, |t, cx| t.set_view(mode, cx));
                    })
                    .child(label.to_string())
            }
        };
        let cur = self
            .branches
            .iter()
            .find(|b| b.is_head)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let ab = self
            .ahead_behind
            .map(|a| {
                let mut s = String::new();
                if a.ahead > 0 {
                    s.push_str(&format!("↑{}", a.ahead));
                }
                if a.behind > 0 {
                    if !s.is_empty() {
                        s.push(' ');
                    }
                    s.push_str(&format!("↓{}", a.behind));
                }
                s
            })
            .unwrap_or_default();
        let head_label = if cur.is_empty() {
            String::new()
        } else {
            format!("⎇ {cur}   {ab}")
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .w_full()
            .h(px(40.0))
            .px_2()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .child(btn("tb-open", "Open", {
                let e = e.clone();
                move |app| {
                    e.update(app, |t, cx| t.open_dialog(cx));
                }
            }))
            .child(
                div()
                    .px_1()
                    .max_w(px(150.0))
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(color::accent())
                    .child(self.repo_name.clone()),
            )
            .child(tab("Log", ViewMode::Log))
            .child(tab("Changes", ViewMode::Changes))
            .child(tab("Branches", ViewMode::Branches))
            .child(div().flex_1().min_w_0().text_color(color::dim()).text_sm().px_2().whitespace_nowrap().text_ellipsis().child(head_label))
            .child(btn("tb-fetch", "Fetch", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.run_remote("fetch", cx)); } }))
            .child(btn("tb-pull", "Pull", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.run_remote("pull", cx)); } }))
            .child(btn("tb-push", "Push", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.run_remote("push", cx)); } }))
            .child(btn("tb-stash", "Stash", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.do_stash(cx)); } }))
            .into_any_element()
    }

    /// Log view: filter bar + virtualized log graph + diff/blame panel.
    fn render_log(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        let filtering = !self.log_filter.trim().is_empty();
        let count = if filtering { self.filtered.len() } else { self.commits.len() };

        let log = match &self.error {
            Some(e) => div().flex_1().p_3().text_color(color::err()).child(e.clone()).into_any_element(),
            None => div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "commit-log",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, _c| {
                            let filtering = !this.log_filter.trim().is_empty();
                            range
                                .filter_map(|pos| {
                                    let ix = if filtering {
                                        this.filtered.get(pos).copied()?
                                    } else {
                                        pos
                                    };
                                    let labels = this
                                        .refs
                                        .get(&this.commits[ix].id)
                                        .map(Vec::as_slice)
                                        .unwrap_or(&[]);
                                    Some(commit_row(
                                        &this.commits[ix],
                                        &this.graph[ix],
                                        this.graph_width,
                                        ix,
                                        this.selected == Some(ix),
                                        labels,
                                        entity.clone(),
                                    ))
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(self.log_scroll.clone())
                    .size_full(),
                )
                .into_any_element(),
        };

        let filter_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .h(px(32.0))
            .px_2()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .child(div().flex_none().text_color(color::dim()).text_sm().child("⌕"))
            .child(
                div()
                    .id("log-filter")
                    .flex_1()
                    .min_w_0()
                    .h(px(22.0))
                    .px_2()
                    .bg(color::bg())
                    .rounded_md()
                    .border_1()
                    .border_color(color::line())
                    .track_focus(&self.log_filter_focus)
                    .key_context("logfilter")
                    .on_key_down(cx.listener(Self::on_filter_key))
                    .on_click(cx.listener(|this, _, window, _| window.focus(&this.log_filter_focus)))
                    .text_sm()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if self.log_filter.is_empty() { color::dim() } else { color::fg() })
                    .child(if self.log_filter.is_empty() {
                        "Filter by message / author / hash — Enter on a hash to jump".to_string()
                    } else {
                        self.log_filter.clone()
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(color::dim())
                    .child(if filtering {
                        format!("{} / {}", count, self.commits.len())
                    } else {
                        format!("{} commits", self.commits.len())
                    }),
            );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(22.0))
            .px_2()
            .gap_2()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .text_xs()
            .text_color(color::dim())
            .child(div().flex_none().w(px(self.graph_width as f32 * LANE_W)))
            .child(div().flex_1().min_w_0().child("Message"))
            .child(div().flex_none().w(px(150.0)).child("Author"))
            .child(div().flex_none().w(px(88.0)).child("Date"))
            .child(div().flex_none().w(px(68.0)).child("Hash"));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(filter_bar)
            .child(header)
            .child(log)
            .child(self.render_diff_panel(cx))
            .into_any_element()
    }

    /// Local Changes view (M4): status + stage/unstage + WT diff + commit.
    fn render_changes(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut list = div()
            .id("status-list")
            .flex()
            .flex_col()
            .w_full()
            .h(px(180.0))
            .overflow_y_scroll()
            .border_b_1()
            .border_color(color::line());

        if let Some(err) = &self.status_error {
            list = list.child(div().p_3().text_color(color::err()).child(err.clone()));
        } else if self.status.is_empty() {
            list = list.child(div().p_3().text_color(color::dim()).child("No local changes ✓"));
        } else {
            for entry in &self.status {
                let path = entry.path.clone();
                let staged = entry.staged;
                let (e1, p1) = (e.clone(), path.clone());
                let (e2, p2) = (e.clone(), path.clone());
                let selected = self.wt_file.as_deref() == Some(path.as_str());
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .px_3()
                        .h(px(22.0))
                        .when(selected, |d| d.bg(color::sel()))
                        .hover(|s| s.bg(color::hover()))
                        .child(
                            div()
                                .id(SharedString::from(format!("ck-{path}")))
                                .cursor_pointer()
                                .text_color(if staged { color::ok() } else { color::dim() })
                                .on_click(move |_, _, app| {
                                    let p = p1.clone();
                                    e1.update(app, |t, cx| t.toggle_stage(p, staged, cx));
                                })
                                .child(if staged { "[✓]" } else { "[ ]" }),
                        )
                        .child(div().flex_none().w(px(74.0)).text_xs().text_color(color::dim()).child(format!("{:?}", entry.state)))
                        .child(
                            div()
                                .id(SharedString::from(format!("st-{path}")))
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .cursor_pointer()
                                .text_color(color::fg())
                                .on_click(move |_, _, app| {
                                    let p = p2.clone();
                                    e2.update(app, |t, cx| t.select_wt_file(p, cx));
                                })
                                .child(path.clone()),
                        ),
                );
            }
        }

        // working-tree diff of the selected file (virtualized)
        let diff_area = if self.wt_rows.is_empty() {
            div().flex_1().min_h_0().p_3().text_color(color::dim()).child("Select a file to view its diff").into_any_element()
        } else {
            let n = self.wt_rows.len();
            div()
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_xs()
                .child(
                    uniform_list(
                        "wt-rows",
                        n,
                        cx.processor(|this, range: std::ops::Range<usize>, _w, _c| {
                            range.filter_map(|i| this.wt_rows.get(i).map(wt_row_el)).collect::<Vec<_>>()
                        }),
                    )
                    .size_full(),
                )
                .into_any_element()
        };

        // commit box
        let commit_box = div()
            .flex()
            .flex_col()
            .w_full()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(
                div()
                    .id("commit-input")
                    .w_full()
                    .h(px(54.0))
                    .p_2()
                    .bg(color::bg())
                    .rounded_md()
                    .border_1()
                    .border_color(color::line())
                    .track_focus(&self.commit_focus)
                    .key_context("commit")
                    .on_key_down(cx.listener(Self::on_commit_key))
                    .on_click(cx.listener(|this, _, window, _| window.focus(&this.commit_focus)))
                    .font_family("Menlo")
                    .text_xs()
                    .text_color(if self.commit_msg.is_empty() { color::dim() } else { color::fg() })
                    .child(if self.commit_msg.is_empty() {
                        "Commit message… (click to type)".to_string()
                    } else {
                        self.commit_msg.clone()
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(btn("do-commit", "Commit", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.do_commit(false, cx)); } }))
                    .child(btn("do-amend", "Amend", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.do_commit(true, cx)); } })),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .child(diff_area)
            .child(commit_box)
            .into_any_element()
    }

    /// Branches view (M5): list with checkout/merge/delete + create branch.
    fn render_branches(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut list = div()
            .id("branch-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();

        for b in &self.branches {
            let name = b.name.clone();
            let (e1, n1) = (e.clone(), name.clone());
            let (e2, n2) = (e.clone(), name.clone());
            let (e3, n3) = (e.clone(), name.clone());
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .h(px(30.0))
                    .hover(|s| s.bg(color::hover()))
                    .child(div().flex_none().w(px(14.0)).text_color(color::accent()).child(if b.is_head { "●" } else { "" }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(if b.is_head { color::accent() } else { color::fg() })
                            .child(name.clone()),
                    )
                    .child(btn(&format!("co-{name}"), "checkout", move |app| { let n = n1.clone(); e1.update(app, |t, cx| t.do_checkout(n, cx)); }))
                    .child(btn(&format!("mg-{name}"), "merge", move |app| { let n = n2.clone(); e2.update(app, |t, cx| t.do_merge(n, cx)); }))
                    .child(btn(&format!("rm-{name}"), "delete", move |app| { let n = n3.clone(); e3.update(app, |t, cx| t.do_delete_branch(n, cx)); })),
            );
        }

        let create = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(
                div()
                    .id("newbranch-input")
                    .flex_1()
                    .h(px(28.0))
                    .px_2()
                    .py_1()
                    .bg(color::bg())
                    .rounded_md()
                    .border_1()
                    .border_color(color::line())
                    .track_focus(&self.branch_focus)
                    .key_context("newbranch")
                    .on_key_down(cx.listener(Self::on_branch_key))
                    .on_click(cx.listener(|this, _, window, _| window.focus(&this.branch_focus)))
                    .text_xs()
                    .text_color(if self.new_branch.is_empty() { color::dim() } else { color::fg() })
                    .child(if self.new_branch.is_empty() {
                        "new branch… (Enter to create)".to_string()
                    } else {
                        self.new_branch.clone()
                    }),
            )
            .child(btn("create-br", "Create", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.do_create_branch(cx)); } }));

        div().flex().flex_col().flex_1().min_h_0().child(list).child(create).into_any_element()
    }

    /// Visual interactive-rebase editor (M6).
    fn render_rebase_editor(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let plan = match &self.rebase {
            Some(p) => p,
            None => return div().into_any_element(),
        };
        let mut list = div()
            .id("rebase-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (i, row) in plan.steps.iter().enumerate() {
            let (e1, e2, e3) = (e.clone(), e.clone(), e.clone());
            let dropped = matches!(row.action, RebaseAction::Drop);
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .h(px(26.0))
                    .hover(|s| s.bg(color::hover()))
                    .child(btn(&format!("ract-{i}"), action_label(&row.action), move |app| {
                        e1.update(app, |t, cx| t.rebase_cycle(i, cx));
                    }))
                    .child(div().flex_none().w(px(56.0)).font_family("Menlo").text_color(color::accent()).child(short(&row.id)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(if dropped { color::dim() } else { color::fg() })
                            .child(row.summary.clone()),
                    )
                    .child(btn(&format!("rup-{i}"), "↑", move |app| {
                        e2.update(app, |t, cx| t.rebase_move(i, true, cx));
                    }))
                    .child(btn(&format!("rdn-{i}"), "↓", move |app| {
                        e3.update(app, |t, cx| t.rebase_move(i, false, cx));
                    })),
            );
        }
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(color::dim())
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(format!(
                        "base {} · {} steps · click the action to cycle pick→squash→fixup→drop",
                        short(&plan.base),
                        plan.steps.len()
                    )),
            )
            .child(btn("rb-apply", "Apply rebase", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.apply_rebase(cx)); } }))
            .child(btn("rb-cancel", "Cancel", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.cancel_rebase(cx)); } }));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py_1()
                    .bg(color::panel())
                    .border_b_1()
                    .border_color(color::line())
                    .text_color(color::accent())
                    .child("Interactive rebase 🎯 — top = oldest"),
            )
            .child(list)
            .child(footer)
            .into_any_element()
    }
}

/// Short label for a rebase action.
fn action_label(a: &RebaseAction) -> &'static str {
    match a {
        RebaseAction::Pick => "PICK",
        RebaseAction::Reword(_) => "REWORD",
        RebaseAction::Squash => "SQUASH",
        RebaseAction::Fixup => "FIXUP",
        RebaseAction::Drop => "DROP",
    }
}

/// A full-width left-aligned menu row (context menus).
fn menu_row(key: &str, label: &str, on: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(SharedString::from(key.to_string()))
        .w_full()
        .px_3()
        .py_1()
        .text_sm()
        .cursor_pointer()
        .text_color(color::fg())
        .hover(|s| s.bg(color::hover()))
        .on_click(move |_, _, app| on(app))
        .child(label.to_string())
}

/// Small UI button.
fn btn(id: &str, label: &str, on: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(SharedString::from(id.to_string()))
        .px_2()
        .py_1()
        .bg(color::btn())
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .hover(|s| s.bg(color::hover()))
        .on_click(move |_, _, app| on(app))
        .child(label.to_string())
}

/// Working-tree diff row (no blame action).
fn wt_row_el(row: &DiffRow) -> gpui::AnyElement {
    match row {
        DiffRow::File(_, label) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .bg(color::panel())
            .text_color(color::fg())
            .child(label.replace("   ⟶ blame", ""))
            .into_any_element(),
        DiffRow::Hunk(h) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .text_color(color::dim())
            .child(h.clone())
            .into_any_element(),
        DiffRow::Line(origin, content) => {
            let (fg, bg, sign) = match origin {
                LineOrigin::Add => (color::add_fg(), color::add_bg(), "+"),
                LineOrigin::Del => (color::del_fg(), color::del_bg(), "−"),
                LineOrigin::Context => (color::fg(), color::bg(), " "),
            };
            div()
                .flex()
                .items_center()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_3()
                .bg(bg)
                .text_color(fg)
                .whitespace_nowrap()
                .child(format!("{sign} {content}"))
                .into_any_element()
        }
    }
}

impl RebasedApp {
    /// Bottom panel: the commit diff, or a file's blame if active.
    /// Both lists are VIRTUALIZED (uniform_list) → smooth render and scroll
    /// even if the file has tens of thousands of lines.
    fn render_diff_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let container = div()
            .flex()
            .flex_col()
            .w_full()
            .h(px(340.0))
            .border_t_1()
            .border_color(color::line())
            .bg(color::bg());

        // ---- Blame mode ----
        if let Some(bv) = &self.blame {
            let head = {
                let e = cx.entity();
                let back = div()
                    .id("blame-back")
                    .px_2()
                    .cursor_pointer()
                    .text_color(color::accent())
                    .hover(|s| s.bg(color::hover()))
                    .on_click(move |_, _, app| e.update(app, |t, cx| t.clear_blame(cx)))
                    .child("← diff");
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_1()
                    .bg(color::panel())
                    .text_sm()
                    .child(back)
                    .child(div().font_family("Menlo").child(bv.file.clone()))
                    .child(div().text_color(color::dim()).child("· blame"))
            };

            let body = if bv.loading {
                div().flex_1().p_3().text_color(color::dim()).child("Loading blame…").into_any_element()
            } else if let Some(err) = &bv.error {
                div().flex_1().p_3().text_color(color::err()).child(err.clone()).into_any_element()
            } else {
                let n = bv.lines.len();
                div()
                    .flex_1()
                    .min_h_0()
                    .font_family("Menlo")
                    .text_xs()
                    .child(
                        uniform_list(
                            "blame-rows",
                            n,
                            cx.processor(|this, range: std::ops::Range<usize>, _w, _cx| {
                                let lines = this.blame.as_ref().map(|b| &b.lines);
                                range
                                    .filter_map(|i| lines.and_then(|l| l.get(i)).map(blame_line_el))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full(),
                    )
                    .into_any_element()
            };
            return container.child(head).child(body).into_any_element();
        }

        // ---- Diff mode ----
        let head = match self.selected {
            Some(ix) => {
                let c = &self.commits[ix];
                let short = c.id.get(..8).unwrap_or(&c.id).to_string();
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_1()
                    .bg(color::panel())
                    .text_sm()
                    .child(div().font_family("Menlo").text_color(color::accent()).child(short))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().child(c.summary.clone()))
                    .child(btn("rb-from", "⤵ rebase", {
                        let e = cx.entity();
                        move |app| {
                            e.update(app, |t, cx| t.start_rebase(ix, cx));
                        }
                    }))
            }
            None => div()
                .w_full()
                .px_3()
                .py_1()
                .bg(color::panel())
                .text_sm()
                .text_color(color::dim())
                .child("Select a commit to view its diff"),
        };

        let body = if let Some(err) = &self.diff_error {
            div().flex_1().p_3().text_color(color::err()).child(err.clone()).into_any_element()
        } else if self.diff_rows.is_empty() {
            div().flex_1().p_3().text_color(color::dim()).child("No changes in this commit").into_any_element()
        } else {
            let entity = cx.entity();
            let n = self.diff_rows.len();
            div()
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_xs()
                .child(
                    uniform_list(
                        "diff-rows",
                        n,
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, _cx| {
                            range
                                .filter_map(|i| this.diff_rows.get(i).map(|r| diff_row_el(r, &entity)))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .size_full(),
                )
                .into_any_element()
        };

        container.child(head).child(body).into_any_element()
    }
}

/// A virtualized diff row: file header (clickable→blame), hunk, or ± line.
fn diff_row_el(row: &DiffRow, entity: &Entity<RebasedApp>) -> gpui::AnyElement {
    match row {
        DiffRow::File(path, label) => {
            let e = entity.clone();
            let p = path.clone();
            div()
                .id(SharedString::from(format!("f:{path}")))
                .flex()
                .items_center()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_3()
                .bg(color::panel())
                .text_color(color::fg())
                .cursor_pointer()
                .hover(|s| s.bg(color::hover()))
                .on_click(move |_, _, app| {
                    let p = p.clone();
                    e.update(app, |t, cx| t.show_blame(p, cx));
                })
                .child(label.clone())
                .into_any_element()
        }
        DiffRow::Hunk(header) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .text_color(color::dim())
            .child(header.clone())
            .into_any_element(),
        DiffRow::Line(origin, content) => {
            let (fg, bg, sign) = match origin {
                LineOrigin::Add => (color::add_fg(), color::add_bg(), "+"),
                LineOrigin::Del => (color::del_fg(), color::del_bg(), "−"),
                LineOrigin::Context => (color::fg(), color::bg(), " "),
            };
            div()
                .flex()
                .items_center()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_3()
                .bg(bg)
                .text_color(fg)
                .whitespace_nowrap()
                .child(format!("{sign} {content}"))
                .into_any_element()
        }
    }
}

/// A virtualized blame row: line · commit · author · text.
fn blame_line_el(l: &BlameLine) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(DIFF_ROW_H))
        .w_full()
        .px_3()
        .child(div().flex_none().w(px(44.0)).text_color(color::dim()).child(format!("{}", l.line_no)))
        .child(div().flex_none().w(px(64.0)).text_color(color::accent()).child(l.commit.clone()))
        .child(
            div()
                .flex_none()
                .w(px(110.0))
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(color::dim())
                .child(l.author.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .text_color(color::fg())
                .child(l.content.clone()),
        )
}

/// A log row (clickable): graph gutter · summary · author · hash.
fn commit_row(
    c: &CommitInfo,
    g: &RowGraph,
    graph_width: usize,
    ix: usize,
    selected: bool,
    labels: &[RefLabel],
    entity: Entity<RebasedApp>,
) -> impl IntoElement {
    let short_id = c.id.get(..8).unwrap_or(&c.id).to_string();
    let chips_box = if labels.is_empty() {
        None
    } else {
        let mut b = div().flex().flex_row().items_center().gap_1().flex_none();
        for l in labels {
            b = b.child(ref_chip(l));
        }
        Some(b)
    };
    let e_sel = entity.clone();
    let e_menu = entity;
    let mut row = div()
        .id(ix)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(ROW_H))
        .px_2()
        .border_b_1()
        .border_color(color::row_line())
        .cursor_pointer()
        .hover(|s| s.bg(color::hover()))
        .on_click(move |_event, _window, app| {
            e_sel.update(app, |this, cx| this.select(ix, cx));
        })
        .on_mouse_down(MouseButton::Right, move |ev, _w, app| {
            let pos = ev.position;
            e_menu.update(app, |this, cx| this.open_commit_menu(ix, pos, cx));
        });
    if selected {
        row = row.bg(color::sel());
    }
    row.child(graph_gutter(g, graph_width))
        .children(chips_box)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(color::fg())
                .child(truncate(&c.summary, 120)),
        )
        .child(
            div()
                .flex_none()
                .w(px(150.0))
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(color::dim())
                .text_sm()
                .child(c.author.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(88.0))
                .whitespace_nowrap()
                .font_family("Menlo")
                .text_color(color::dim())
                .text_sm()
                .child(fmt_date(c.time)),
        )
        .child(
            div()
                .flex_none()
                .w(px(68.0))
                .whitespace_nowrap()
                .font_family("Menlo")
                .text_color(color::dim())
                .text_sm()
                .child(short_id),
        )
}

/// Graph gutter: one vertical line per active lane + the commit dot.
fn graph_gutter(g: &RowGraph, width: usize) -> impl IntoElement {
    let mut gutter = div()
        .relative()
        .flex_none()
        .h(px(ROW_H))
        .w(px(width as f32 * LANE_W));

    for (i, lane) in g.lanes.iter().enumerate().take(width) {
        if let Some(c) = lane {
            gutter = gutter.child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .h(px(ROW_H))
                    .left(px(i as f32 * LANE_W + LANE_W / 2.0 - 1.0))
                    .w(px(2.0))
                    .bg(branch_color(*c)),
            );
        }
    }

    let lane = g.lane.min(width.saturating_sub(1));
    gutter.child(
        div()
            .absolute()
            .left(px(lane as f32 * LANE_W + LANE_W / 2.0 - DOT / 2.0))
            .top(px(ROW_H / 2.0 - DOT / 2.0))
            .size(px(DOT))
            .rounded_full()
            .bg(branch_color(g.color)),
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn main() {
    // Optional repo path argument; otherwise the welcome screen shows.
    let initial = std::env::args().nth(1);
    let recents = load_recents();

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |cx| {
                    let mut app = RebasedApp {
                        repo_path: String::new(),
                        commits: Vec::new(),
                        graph: Vec::new(),
                        graph_width: 1,
                        error: None,
                        selected: None,
                        diff: Vec::new(),
                        diff_rows: Vec::new(),
                        diff_error: None,
                        blame: None,
                        view: ViewMode::Log,
                        op_msg: None,
                        status: Vec::new(),
                        status_error: None,
                        wt_rows: Vec::new(),
                        wt_file: None,
                        commit_msg: String::new(),
                        commit_focus: cx.focus_handle(),
                        branches: Vec::new(),
                        new_branch: String::new(),
                        branch_focus: cx.focus_handle(),
                        rebase: None,
                        repo_loaded: false,
                        repo_name: String::new(),
                        title_dirty: true,
                        recents,
                        refs: HashMap::new(),
                        log_scroll: UniformListScrollHandle::new(),
                        ahead_behind: None,
                        menu: None,
                        prompt: None,
                        focus_prompt: false,
                        log_filter: String::new(),
                        log_filter_focus: cx.focus_handle(),
                        filtered: Vec::new(),
                    };
                    if let Some(p) = initial {
                        app.load_repo(&p);
                    }
                    app
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

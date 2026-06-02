//! diff — native window (GPUI): log graph + diff viewer.
//! M2: virtualized log + DAG.  M3: click a commit → diff below.
//!
//! Usage:  diff [repo-path] [limit]   (default: . and 50000)

use git_core::blame::BlameLine;
use git_core::diff::{FileDiff, LineOrigin};
use git_core::graph::{compute_graph, EdgeKind, RowGraph};
use git_core::syntax::{self, Lang};
use git_core::rebase::{RebaseAction, RebaseResult, RebaseStep};
use git_core::{
    AheadBehind, BranchInfo, CommitInfo, ConflictSides, PrComment, PrDetail, RefKind, RefLabel,
    ReflogEntry, RemoteInfo, StashInfo, StatusEntry, SubmoduleInfo, TagInfo,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds,
    ClipboardItem, Context, Entity, FocusHandle, Hsla, KeyDownEvent, MouseButton, PathBuilder,
    PathPromptOptions, Rgba, ScrollStrategy, SharedString, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
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
    // Syntax-highlight palette (New UI dark).
    pub fn syn_kw() -> Rgba { rgb(0xcf8e6d) }   // keyword (orange)
    pub fn syn_str() -> Rgba { rgb(0x6aab73) }  // string (green)
    pub fn syn_num() -> Rgba { rgb(0x2aacb8) }  // number (teal)
    pub fn syn_cmt() -> Rgba { rgb(0x7a7e85) }  // comment (gray)
    pub fn syn_type() -> Rgba { rgb(0x9b86d6) } // type (purple)
    pub fn syn_func() -> Rgba { rgb(0x57aaeb) } // function (blue)
}

/// Main app views (tabs + the "More" menu).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Log,
    Changes,
    Branches,
    Conflicts,
    Stashes,
    Reflog,
    Remotes,
    Submodules,
    PullRequests,
    Console,
    Settings,
    /// History of a single file (`git log --follow`).
    FileHistory,
    /// Pickaxe search across history (`git log -S`/`-G`).
    Search,
}

/// What an ad-hoc [`CommitList`] represents (drives its header + diff scoping).
#[derive(Clone)]
enum ListKind {
    /// Commits touching this file path (the diff is scoped to the file).
    FileHistory(String),
    /// Pickaxe search results (full commit diff shown).
    Search,
}

/// A secondary commit list (file history or pickaxe search), shown above the
/// shared diff panel. Independent from the main log selection.
struct CommitList {
    kind: ListKind,
    label: String,
    commits: Vec<CommitInfo>,
    selected: Option<usize>,
    msg: Option<String>,
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
    cursor: usize,
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
    Clone,
}

/// A GitHub pull request (via the `gh` CLI).
struct PrInfo {
    number: String,
    title: String,
    branch: String,
    state: String,
    author: String,
}

/// The open PR detail/review session (diff + threaded comments).
struct PrView {
    detail: PrDetail,
    /// Flattened diff with conversation + inline comment threads interleaved.
    rows: Vec<PrRow>,
    loading: bool,
    error: Option<String>,
}

/// What the inline composer will post on submit.
#[derive(Clone)]
enum ComposeTarget {
    /// A new inline review comment at `path`:`line` on `side` (RIGHT/LEFT).
    Line { path: String, line: u32, side: String },
    /// A reply to an existing review-comment thread.
    Reply { comment_id: String, label: String },
    /// A general conversation comment on the PR.
    General,
    /// A review submission: kind ∈ {approve, request-changes, comment}.
    Review { kind: String },
}

impl ComposeTarget {
    /// Header label for the composer panel.
    fn label(&self) -> String {
        match self {
            ComposeTarget::Line { path, line, side } => format!("Comment on {path}:{line} ({side})"),
            ComposeTarget::Reply { label, .. } => format!("Reply to {label}"),
            ComposeTarget::General => "Comment on the conversation".into(),
            ComposeTarget::Review { kind } => match kind.as_str() {
                "approve" => "Approve — optional message".into(),
                "request-changes" => "Request changes — describe what's needed".into(),
                _ => "Review comment".into(),
            },
        }
    }
}

/// A row in the PR review view (conversation, diff structure, inline comments).
enum PrRow {
    /// A conversation/issue comment (or an orphaned inline comment).
    Conversation(PrComment),
    /// File header ("path  +a −d" label).
    File(String),
    Hunk(String),
    /// A diff line, carrying its anchor (path/line/side) for "add comment".
    Line {
        origin: LineOrigin,
        content: String,
        lang: Lang,
        path: String,
        line: u32,
        side: String,
    },
    /// An inline review comment attached under its line.
    Comment(PrComment),
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
    /// Hunk header. Carries enough context to stage/unstage this hunk alone.
    Hunk {
        file: String,
        index: usize,
        header: String,
        /// `true` if this hunk comes from the staged (index) diff.
        staged: bool,
    },
    /// A diff line: origin (+/-/ctx), content, and the file's language (for syntax).
    Line(LineOrigin, String, Lang),
}

/// Maps a syntax token kind to its color.
fn tok_color(t: syntax::Tok) -> Rgba {
    use syntax::Tok;
    match t {
        Tok::Text => color::fg(),
        Tok::Keyword => color::syn_kw(),
        Tok::Type => color::syn_type(),
        Tok::Str => color::syn_str(),
        Tok::Comment => color::syn_cmt(),
        Tok::Number => color::syn_num(),
        Tok::Func => color::syn_func(),
    }
}

/// Renders one line of code as syntax-colored spans (a gap-free monospace row).
/// With `on == false` or an unknown language, falls back to a single `fallback`-
/// colored run (e.g. the add/del tint) so nothing is lost.
fn line_spans(content: &str, lang: Lang, on: bool, fallback: Rgba) -> gpui::AnyElement {
    if !on || lang == Lang::Plain {
        return div()
            .min_w_0()
            .whitespace_nowrap()
            .text_color(fallback)
            .child(content.to_string())
            .into_any_element();
    }
    let mut row = div().flex().flex_row().min_w_0().whitespace_nowrap();
    for (kind, s) in syntax::highlight(content, lang) {
        if s.is_empty() {
            continue;
        }
        row = row.child(div().flex_none().text_color(tok_color(kind)).child(s));
    }
    row.into_any_element()
}

/// Flattens the diff (files→hunks→lines) into rows for the virtualized list.
fn build_diff_rows(diff: &[FileDiff]) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for f in diff {
        push_file_rows(&mut rows, f, false);
    }
    rows
}

/// Appends one file's rows (header, hunks, lines). `staged` flags whether the
/// hunks come from the index diff (controls the Stage/Unstage hunk action).
fn push_file_rows(rows: &mut Vec<DiffRow>, f: &FileDiff, staged: bool) {
    let (add, del) = f.line_stats();
    let bin = if f.binary { "  [binary]" } else { "" };
    let lang = syntax::lang_for_path(&f.path);
    rows.push(DiffRow::File(
        f.path.clone(),
        format!("{}   +{add} −{del}{bin}", f.path),
    ));
    for (i, h) in f.hunks.iter().enumerate() {
        rows.push(DiffRow::Hunk {
            file: f.path.clone(),
            index: i,
            header: h.header.clone(),
            staged,
        });
        for l in &h.lines {
            rows.push(DiffRow::Line(l.origin, l.content.clone(), lang));
        }
    }
}

/// One side of a side-by-side diff row.
struct SideCell {
    lineno: Option<u32>,
    text: String,
    kind: LineOrigin,
    lang: Lang,
}

/// A row of the side-by-side diff (file header, hunk header, or a left/right pair).
enum SideRow {
    File(String),
    Hunk(String),
    Pair(Option<SideCell>, Option<SideCell>),
}

/// Flushes accumulated deletions/additions as aligned left/right pairs.
fn flush_side<'a>(
    rows: &mut Vec<SideRow>,
    dels: &mut Vec<&'a git_core::diff::DiffLine>,
    adds: &mut Vec<&'a git_core::diff::DiffLine>,
    lang: Lang,
) {
    let n = dels.len().max(adds.len());
    for i in 0..n {
        let l = dels.get(i).map(|d| SideCell {
            lineno: d.old_lineno,
            text: d.content.clone(),
            kind: LineOrigin::Del,
            lang,
        });
        let r = adds.get(i).map(|a| SideCell {
            lineno: a.new_lineno,
            text: a.content.clone(),
            kind: LineOrigin::Add,
            lang,
        });
        rows.push(SideRow::Pair(l, r));
    }
    dels.clear();
    adds.clear();
}

/// Builds side-by-side rows from a structured diff (deletions left, additions right).
fn build_side_rows(diff: &[FileDiff]) -> Vec<SideRow> {
    let mut rows = Vec::new();
    for f in diff {
        let (add, del) = f.line_stats();
        let lang = syntax::lang_for_path(&f.path);
        rows.push(SideRow::File(format!("{}   +{add} −{del}", f.path)));
        for h in &f.hunks {
            rows.push(SideRow::Hunk(h.header.clone()));
            let mut dels: Vec<&git_core::diff::DiffLine> = Vec::new();
            let mut adds: Vec<&git_core::diff::DiffLine> = Vec::new();
            for l in &h.lines {
                match l.origin {
                    LineOrigin::Context => {
                        flush_side(&mut rows, &mut dels, &mut adds, lang);
                        rows.push(SideRow::Pair(
                            Some(SideCell { lineno: l.old_lineno, text: l.content.clone(), kind: LineOrigin::Context, lang }),
                            Some(SideCell { lineno: l.new_lineno, text: l.content.clone(), kind: LineOrigin::Context, lang }),
                        ));
                    }
                    LineOrigin::Del => dels.push(l),
                    LineOrigin::Add => adds.push(l),
                }
            }
            flush_side(&mut rows, &mut dels, &mut adds, lang);
        }
    }
    rows
}

/// Flattens a PR's diff + comments into review rows: conversation comments
/// first, then each file's diff with inline comment threads interleaved under
/// the lines they target. Inline comments that don't map to a visible line
/// (e.g. outdated) are appended as conversation-style cards.
fn build_pr_rows(diff: &[FileDiff], comments: &[PrComment], conversation: &[PrComment]) -> Vec<PrRow> {
    let mut rows = Vec::new();
    for c in conversation {
        rows.push(PrRow::Conversation(c.clone()));
    }
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for f in diff {
        let lang = syntax::lang_for_path(&f.path);
        let (add, del) = f.line_stats();
        rows.push(PrRow::File(format!("{}   +{add} −{del}", f.path)));
        for h in &f.hunks {
            rows.push(PrRow::Hunk(h.header.clone()));
            for l in &h.lines {
                let (side, line) = match l.new_lineno {
                    Some(n) => ("RIGHT", n),
                    None => ("LEFT", l.old_lineno.unwrap_or(0)),
                };
                rows.push(PrRow::Line {
                    origin: l.origin,
                    content: l.content.clone(),
                    lang,
                    path: f.path.clone(),
                    line,
                    side: side.to_string(),
                });
                for c in comments {
                    if c.path != f.path || used.contains(&c.id) {
                        continue;
                    }
                    let on_right = (c.side == "RIGHT" || c.side.is_empty()) && l.new_lineno == Some(c.line);
                    let on_left = c.side == "LEFT" && l.old_lineno == Some(c.line);
                    if on_right || on_left {
                        rows.push(PrRow::Comment(c.clone()));
                        used.insert(c.id.clone());
                    }
                }
            }
        }
    }
    // Orphaned inline comments (anchored outside the current diff window).
    for c in comments {
        if !used.contains(&c.id) {
            rows.push(PrRow::Conversation(c.clone()));
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
    /// Working-tree diff of the selected changed file (flat, for unified view).
    wt_rows: Vec<DiffRow>,
    /// Same diff as structured files (for the side-by-side view).
    wt_diff: Vec<FileDiff>,
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

    // ---- Diff viewer options ----
    /// Ignore whitespace in diffs.
    diff_ignore_ws: bool,
    /// Side-by-side (two columns) vs unified diff.
    diff_side_by_side: bool,
    /// Syntax-highlight diff/code lines.
    diff_syntax: bool,
    /// Scroll handle for the commit-diff list (prev/next change navigation).
    diff_scroll: UniformListScrollHandle,
    /// Row index of the current change (for prev/next navigation).
    diff_cursor: usize,

    // ---- Secondary views (engine-backed) ----
    stashes: Vec<StashInfo>,
    reflog: Vec<ReflogEntry>,
    remotes: Vec<RemoteInfo>,
    submodules: Vec<SubmoduleInfo>,
    tags: Vec<TagInfo>,
    conflicts: Vec<String>,
    conflict_file: Option<String>,
    conflict_sides: Option<ConflictSides>,
    /// Git console: command input + accumulated output.
    console_input: String,
    console_focus: FocusHandle,
    console_output: String,
    /// "More" views dropdown open?
    more_open: bool,
    /// Add a `Signed-off-by` trailer on commit.
    sign_off: bool,
    /// In-progress op (rebase/merge/cherry-pick/revert) → action banner.
    op_state: Option<String>,

    // ---- Text-input caret positions (byte indices) ----
    commit_cursor: usize,
    branch_cursor: usize,
    filter_cursor: usize,
    console_cursor: usize,

    // ---- GitHub (via `gh`) ----
    prs: Vec<PrInfo>,
    prs_msg: Option<String>,
    /// Open PR review session (None = show the PR list).
    pr_view: Option<PrView>,
    /// Inline comment composer: target + buffer + caret.
    pr_compose_target: Option<ComposeTarget>,
    pr_compose: String,
    pr_compose_focus: FocusHandle,
    pr_compose_cursor: usize,
    /// Set when opening the composer, to focus its input on next render.
    focus_pr_compose: bool,

    // ---- File history / pickaxe search ----
    /// Ad-hoc commit list (file history or search) feeding the shared diff panel.
    aux: Option<CommitList>,
    /// Pickaxe search term + caret.
    search_term: String,
    search_focus: FocusHandle,
    search_cursor: usize,
    /// Treat the search term as a regex (`-G`) instead of a literal (`-S`).
    search_regex: bool,

    /// Whether the commit-graph was refreshed for the current repo (once).
    graph_maintained: bool,
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
            self.op_state = repo.op_in_progress();
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
        self.wt_diff.clear();
        self.rebase = None;
        self.op_msg = None;
        self.view = ViewMode::Log;
        self.aux = None;
        self.search_term.clear();
        self.search_cursor = 0;
        self.pr_view = None;
        self.pr_compose_target = None;
        self.pr_compose.clear();
        self.pr_compose_cursor = 0;
        self.graph_maintained = false;

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
            cursor: initial.len(),
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
            edit_key(&mut p.value, &mut p.cursor, ev, false);
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
            PromptKind::Clone => self.do_clone(v, cx),
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
            self.filter_cursor = 0;
        } else {
            edit_key(&mut self.log_filter, &mut self.filter_cursor, ev, false);
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
        match git_core::diff::commit_diff_ws(&self.repo_path, &id, self.diff_ignore_ws) {
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

    // ---- File history & pickaxe search ----

    /// Opens the file-history view for `path` (commits touching it, following
    /// renames). Loads in the background; the diff panel scopes to the file.
    fn show_file_history(&mut self, path: String, cx: &mut Context<Self>) {
        self.view = ViewMode::FileHistory;
        self.more_open = false;
        self.blame = None;
        self.op_msg = None;
        self.aux = Some(CommitList {
            kind: ListKind::FileHistory(path.clone()),
            label: format!("History · {path}"),
            commits: Vec::new(),
            selected: None,
            msg: Some("Loading history…".into()),
        });
        self.diff.clear();
        self.diff_rows.clear();
        self.diff_error = None;
        cx.notify();

        let repo = self.repo_path.clone();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let res = cx
                .background_executor()
                .spawn(async move { git_core::file_history(&repo, &p, 1000) })
                .await;
            let _ = this.update(cx, |t, cx| {
                let still = matches!(&t.aux, Some(a) if matches!(&a.kind, ListKind::FileHistory(fp) if fp == &path));
                if !still {
                    return;
                }
                let aux = t.aux.as_mut().unwrap();
                match res {
                    Ok(commits) => {
                        aux.msg = if commits.is_empty() {
                            Some("No history for this file".into())
                        } else {
                            None
                        };
                        aux.commits = commits;
                        aux.selected = None;
                    }
                    Err(e) => {
                        aux.commits.clear();
                        aux.msg = Some(format!("history: {}", first_line(&e)));
                    }
                }
                if !t.aux.as_ref().unwrap().commits.is_empty() {
                    t.aux_select(0, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the pickaxe search view (preserving any prior results).
    fn open_search(&mut self, cx: &mut Context<Self>) {
        self.view = ViewMode::Search;
        self.more_open = false;
        self.op_msg = None;
        let has_search = matches!(&self.aux, Some(a) if matches!(a.kind, ListKind::Search));
        if !has_search {
            self.aux = Some(CommitList {
                kind: ListKind::Search,
                label: "Search in changes".into(),
                commits: Vec::new(),
                selected: None,
                msg: Some("Type a term and Run — finds commits whose diff adds/removes it.".into()),
            });
            self.diff.clear();
            self.diff_rows.clear();
            self.diff_error = None;
        }
        cx.notify();
    }

    /// Runs the pickaxe search over history (background).
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let term = self.search_term.trim().to_string();
        if term.is_empty() {
            return;
        }
        let regex = self.search_regex;
        self.aux = Some(CommitList {
            kind: ListKind::Search,
            label: format!("Search · {}{term}", if regex { "/" } else { "" }),
            commits: Vec::new(),
            selected: None,
            msg: Some("Searching…".into()),
        });
        self.diff.clear();
        self.diff_rows.clear();
        self.diff_error = None;
        cx.notify();

        let repo = self.repo_path.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { git_core::pickaxe(&repo, &term, regex, 500) })
                .await;
            let _ = this.update(cx, |t, cx| {
                let still = matches!(&t.aux, Some(a) if matches!(a.kind, ListKind::Search));
                if !still {
                    return;
                }
                let aux = t.aux.as_mut().unwrap();
                match res {
                    Ok(commits) => {
                        aux.msg = if commits.is_empty() { Some("No matches".into()) } else { None };
                        aux.commits = commits;
                        aux.selected = None;
                    }
                    Err(e) => {
                        aux.commits.clear();
                        aux.msg = Some(format!("search: {}", first_line(&e)));
                    }
                }
                if !t.aux.as_ref().unwrap().commits.is_empty() {
                    t.aux_select(0, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Selects a commit in the ad-hoc list and loads its diff (scoped to the
    /// file for file-history; full commit for search).
    fn aux_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(aux) = &self.aux else { return };
        let Some(c) = aux.commits.get(ix) else { return };
        let id = c.id.clone();
        let kind = aux.kind.clone();
        if let Some(a) = self.aux.as_mut() {
            a.selected = Some(ix);
        }
        self.blame = None;
        match git_core::diff::commit_diff_ws(&self.repo_path, &id, self.diff_ignore_ws) {
            Ok(mut files) => {
                if let ListKind::FileHistory(path) = &kind {
                    let scoped: Vec<FileDiff> = files
                        .iter()
                        .filter(|f| &f.path == path || f.old_path.as_deref() == Some(path.as_str()))
                        .cloned()
                        .collect();
                    // Pre-rename commits carry the old name; if scoping empties the
                    // diff, fall back to the whole commit rather than showing nothing.
                    if !scoped.is_empty() {
                        files = scoped;
                    }
                }
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

    fn on_search_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "enter" {
            self.run_search(cx);
            return;
        }
        edit_key(&mut self.search_term, &mut self.search_cursor, ev, false);
        cx.notify();
    }

    // ---- View navigation ----
    fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        self.view = view;
        self.op_msg = None;
        self.more_open = false;
        match view {
            ViewMode::Changes => self.refresh_status(cx),
            ViewMode::Branches => {
                self.refresh_tags();
                self.refresh_branches(cx);
            }
            ViewMode::Conflicts => self.refresh_conflicts(cx),
            ViewMode::Stashes => self.refresh_stashes(cx),
            ViewMode::Reflog => self.refresh_reflog(cx),
            ViewMode::Remotes => self.refresh_remotes(cx),
            ViewMode::Submodules => self.refresh_submodules(cx),
            ViewMode::PullRequests => self.refresh_prs(cx),
            ViewMode::Search => self.open_search(cx),
            ViewMode::Log | ViewMode::Console | ViewMode::Settings | ViewMode::FileHistory => {
                cx.notify()
            }
        }
    }

    fn toggle_more(&mut self, cx: &mut Context<Self>) {
        self.more_open = !self.more_open;
        cx.notify();
    }

    // ---- Secondary views: refresh + actions ----
    fn refresh_tags(&mut self) {
        self.tags = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.tags())
            .unwrap_or_default();
    }

    fn refresh_stashes(&mut self, cx: &mut Context<Self>) {
        self.stashes = git_core::Repo::open(&self.repo_path)
            .and_then(|mut r| r.stash_entries())
            .unwrap_or_default();
        cx.notify();
    }

    fn refresh_reflog(&mut self, cx: &mut Context<Self>) {
        self.reflog = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.reflog(200))
            .unwrap_or_default();
        cx.notify();
    }

    fn refresh_remotes(&mut self, cx: &mut Context<Self>) {
        self.remotes = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.remotes_detailed())
            .unwrap_or_default();
        cx.notify();
    }

    fn refresh_submodules(&mut self, cx: &mut Context<Self>) {
        self.submodules = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.submodules())
            .unwrap_or_default();
        cx.notify();
    }

    fn refresh_conflicts(&mut self, cx: &mut Context<Self>) {
        self.conflicts = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.conflicts())
            .unwrap_or_default();
        if self.conflict_file.as_ref().is_none_or(|f| !self.conflicts.contains(f)) {
            self.conflict_file = self.conflicts.first().cloned();
        }
        self.load_conflict_sides();
        self.op_state = git_core::Repo::open(&self.repo_path)
            .ok()
            .and_then(|r| r.op_in_progress());
        cx.notify();
    }

    fn load_conflict_sides(&mut self) {
        self.conflict_sides = match &self.conflict_file {
            Some(f) => git_core::Repo::open(&self.repo_path)
                .ok()
                .and_then(|r| r.conflict_sides(f).ok()),
            None => None,
        };
    }

    fn select_conflict(&mut self, file: String, cx: &mut Context<Self>) {
        self.conflict_file = Some(file);
        self.load_conflict_sides();
        cx.notify();
    }

    fn resolve_conflict(&mut self, take_ours: bool, cx: &mut Context<Self>) {
        if let Some(f) = self.conflict_file.clone() {
            let r = git_core::Repo::open(&self.repo_path)
                .map_err(|e| e.to_string())
                .and_then(|repo| repo.resolve_conflict(&f, take_ours));
            self.op_msg = Some(match r {
                Ok(_) => format!("resolved {f} ({})", if take_ours { "ours" } else { "theirs" }),
                Err(e) => format!("✗ {e}"),
            });
            self.conflict_file = None;
        }
        self.refresh_conflicts(cx);
    }

    // ---- Stash actions ----
    fn stash_pop_ix(&mut self, i: usize, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|mut repo| repo.stash_pop_index(i))
            .map(|_| format!("stash pop {i}"));
        self.note(r);
        self.reload_log();
        self.refresh_stashes(cx);
    }

    fn stash_apply_ix(&mut self, i: usize, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|mut repo| repo.stash_apply_index(i))
            .map(|_| format!("stash apply {i}"));
        self.note(r);
        self.refresh_stashes(cx);
    }

    fn stash_drop_ix(&mut self, i: usize, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|mut repo| repo.stash_drop_index(i))
            .map(|_| format!("stash drop {i}"));
        self.note(r);
        self.refresh_stashes(cx);
    }

    // ---- Tag actions ----
    fn delete_tag(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.delete_tag(&name))
            .map(|_| format!("deleted tag {name}"));
        self.note(r);
        self.refresh_tags();
        self.reload_log();
        cx.notify();
    }

    fn push_tag(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .map_err(|e| e.to_string())
            .and_then(|repo| repo.push_tag("origin", &name));
        self.note(r.map(|_| format!("pushed tag {name}")));
        cx.notify();
    }

    // ---- Remote actions ----
    fn remote_remove(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.remove_remote(&name))
            .map(|_| format!("removed remote {name}"));
        self.note(r);
        self.refresh_remotes(cx);
    }

    // ---- Git console ----
    fn run_console(&mut self, cx: &mut Context<Self>) {
        let cmd = self.console_input.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        let parts: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        let args: Vec<&str> = if parts.first().map(String::as_str) == Some("git") {
            parts[1..].iter().map(String::as_str).collect()
        } else {
            parts.iter().map(String::as_str).collect()
        };
        let res = git_core::Repo::open(&self.repo_path)
            .map_err(|e| e.to_string())
            .and_then(|repo| repo.git(&args));
        let body = match res {
            Ok(o) => o,
            Err(e) => format!("error: {e}"),
        };
        self.console_output
            .push_str(&format!("$ git {}\n{}\n", args.join(" "), body.trim_end()));
        self.console_input.clear();
        self.console_cursor = 0;
        self.reload_log();
        cx.notify();
    }

    // ---- GitHub (via `gh`) ----
    fn refresh_prs(&mut self, cx: &mut Context<Self>) {
        self.prs.clear();
        self.prs_msg = Some("Loading pull requests…".into());
        cx.notify();
        let path = self.repo_path.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { git_core::gh_pr_list(&path) })
                .await;
            let _ = this.update(cx, |t, cx| {
                match res {
                    Ok(out) => {
                        t.prs = out
                            .lines()
                            .filter_map(|l| {
                                let mut f = l.split('\t');
                                Some(PrInfo {
                                    number: f.next()?.to_string(),
                                    title: f.next().unwrap_or("").to_string(),
                                    branch: f.next().unwrap_or("").to_string(),
                                    state: f.next().unwrap_or("").to_string(),
                                    author: f.next().unwrap_or("").to_string(),
                                })
                            })
                            .collect();
                        t.prs_msg = if t.prs.is_empty() {
                            Some("No open pull requests".into())
                        } else {
                            None
                        };
                    }
                    Err(e) => {
                        t.prs.clear();
                        t.prs_msg = Some(format!("gh: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn pr_action(&mut self, number: String, web: bool, cx: &mut Context<Self>) {
        let path = self.repo_path.clone();
        self.op_msg = Some(if web {
            format!("opening PR #{number}…")
        } else {
            format!("checking out PR #{number}…")
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    if web {
                        git_core::gh_pr(&path, &["view", &number, "--web"])
                    } else {
                        git_core::gh_pr(&path, &["checkout", &number])
                    }
                })
                .await;
            let _ = this.update(cx, |t, cx| {
                t.op_msg = Some(match res {
                    Ok(_) => "✓ done".into(),
                    Err(e) => format!("✗ {}", first_line(&e)),
                });
                t.reload_log();
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the PR review session: loads detail + diff + comments (background).
    fn open_pr(&mut self, number: String, cx: &mut Context<Self>) {
        self.view = ViewMode::PullRequests;
        self.more_open = false;
        self.op_msg = None;
        self.pr_compose_target = None;
        self.pr_compose.clear();
        self.pr_view = Some(PrView {
            detail: PrDetail { number: number.clone(), ..Default::default() },
            rows: Vec::new(),
            loading: true,
            error: None,
        });
        cx.notify();

        let repo = self.repo_path.clone();
        cx.spawn(async move |this, cx| {
            let num = number.clone();
            let res = cx
                .background_executor()
                .spawn(async move {
                    let detail = git_core::gh_pr_detail(&repo, &num)?;
                    let diff_text = git_core::gh_pr_diff(&repo, &num).unwrap_or_default();
                    let diff = git_core::diff::parse_unified_diff(&diff_text);
                    let comments = git_core::gh_pr_review_comments(&repo, &num).unwrap_or_default();
                    let conversation = git_core::gh_pr_conversation(&repo, &num).unwrap_or_default();
                    Ok::<_, String>((detail, diff, comments, conversation))
                })
                .await;
            let _ = this.update(cx, |t, cx| {
                let still = matches!(&t.pr_view, Some(v) if v.detail.number == number);
                if !still {
                    return;
                }
                match res {
                    Ok((detail, diff, comments, conversation)) => {
                        let rows = build_pr_rows(&diff, &comments, &conversation);
                        t.pr_view = Some(PrView { detail, rows, loading: false, error: None });
                    }
                    Err(e) => {
                        if let Some(v) = t.pr_view.as_mut() {
                            v.loading = false;
                            v.error = Some(e);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Closes the PR review session, back to the PR list.
    fn close_pr(&mut self, cx: &mut Context<Self>) {
        self.pr_view = None;
        self.pr_compose_target = None;
        self.pr_compose.clear();
        cx.notify();
    }

    /// Reloads the currently open PR (after posting a comment/review).
    fn reload_pr(&mut self, cx: &mut Context<Self>) {
        if let Some(v) = &self.pr_view {
            let n = v.detail.number.clone();
            self.open_pr(n, cx);
        }
    }

    /// Opens the inline composer targeting `target`.
    fn pr_compose_to(&mut self, target: ComposeTarget, cx: &mut Context<Self>) {
        self.pr_compose_target = Some(target);
        self.pr_compose.clear();
        self.pr_compose_cursor = 0;
        self.focus_pr_compose = true;
        cx.notify();
    }

    fn pr_cancel_compose(&mut self, cx: &mut Context<Self>) {
        self.pr_compose_target = None;
        self.pr_compose.clear();
        self.pr_compose_cursor = 0;
        cx.notify();
    }

    /// Submits the composer (post inline comment / reply / conversation / review).
    fn pr_submit(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.pr_compose_target.clone() else { return };
        let Some(v) = &self.pr_view else { return };
        let number = v.detail.number.clone();
        let head_sha = v.detail.head_sha.clone();
        let body = self.pr_compose.clone();
        // Only an "approve" review may have an empty body.
        let optional_body = matches!(&target, ComposeTarget::Review { kind } if kind == "approve");
        if !optional_body && body.trim().is_empty() {
            self.op_msg = Some("✗ write something first".into());
            cx.notify();
            return;
        }
        let repo = self.repo_path.clone();
        self.op_msg = Some("Posting…".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    match target {
                        ComposeTarget::Line { path, line, side } => {
                            git_core::gh_pr_add_comment(&repo, &number, &head_sha, &path, line, &side, &body)
                        }
                        ComposeTarget::Reply { comment_id, .. } => {
                            git_core::gh_pr_reply(&repo, &number, &comment_id, &body)
                        }
                        ComposeTarget::General => git_core::gh_pr_comment_general(&repo, &number, &body),
                        ComposeTarget::Review { kind } => git_core::gh_pr_review(&repo, &number, &kind, &body),
                    }
                })
                .await;
            let _ = this.update(cx, |t, cx| {
                match res {
                    Ok(m) => {
                        t.op_msg = Some(format!("✓ {m}"));
                        t.pr_compose_target = None;
                        t.pr_compose.clear();
                        t.reload_pr(cx);
                    }
                    Err(e) => t.op_msg = Some(format!("✗ {}", first_line(&e))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_pr_compose_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        // ⌘↵ submits; plain Enter inserts a newline (comments are multi-line).
        if ev.keystroke.key == "enter" && ev.keystroke.modifiers.platform {
            self.pr_submit(cx);
            return;
        }
        edit_key(&mut self.pr_compose, &mut self.pr_compose_cursor, ev, true);
        cx.notify();
    }

    fn do_clone(&mut self, spec: String, cx: &mut Context<Self>) {
        let mut it = spec.split_whitespace();
        let Some(url) = it.next().map(str::to_string) else { return };
        let dir = it.next().map(str::to_string).unwrap_or_else(|| {
            let name = url.rsplit('/').next().unwrap_or("repo").trim_end_matches(".git");
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/{name}")
        });
        self.op_msg = Some(format!("cloning {url}…"));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let dir2 = dir.clone();
            let res = cx
                .background_executor()
                .spawn(async move { git_core::clone_repo(&url, &dir2) })
                .await;
            let _ = this.update(cx, |t, cx| {
                match res {
                    Ok(d) => {
                        t.op_msg = Some(format!("✓ cloned into {d}"));
                        t.load_repo(&d);
                    }
                    Err(e) => t.op_msg = Some(format!("✗ clone: {}", first_line(&e))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_console_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "enter" {
            self.run_console(cx);
            return;
        }
        edit_key(&mut self.console_input, &mut self.console_cursor, ev, false);
        cx.notify();
    }

    /// Sets `op_msg` from an operation result (no log reload).
    fn note<E: std::fmt::Display>(&mut self, r: Result<String, E>) {
        self.op_msg = Some(match r {
            Ok(m) => format!("✓ {m}"),
            Err(e) => format!("✗ {e}"),
        });
    }

    // ---- Local Changes (M4) ----
    /// Loads status (+ the selected file's diff) IN THE BACKGROUND: on huge
    /// repos status scans 200k+ files (~4s) and must not freeze the UI.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let path = self.repo_path.clone();
        let wt_file = self.wt_file.clone();
        let ignore_ws = self.diff_ignore_ws;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let repo = git_core::Repo::open(&path).map_err(|e| e.to_string())?;
                    let status = repo.status().map_err(|e| e.to_string())?;
                    let (rows, fdiff) = match &wt_file {
                        Some(f) => {
                            // Prefer the unstaged diff; fall back to the staged one.
                            let un = git_core::diff::workdir_diff_ws(&path, false, ignore_ws)
                                .unwrap_or_default();
                            let mut r = Vec::new();
                            let mut fdv = Vec::new();
                            if let Some(fd) = un.iter().find(|x| &x.path == f) {
                                push_file_rows(&mut r, fd, false);
                                fdv.push(fd.clone());
                            } else {
                                let st = git_core::diff::workdir_diff_ws(&path, true, ignore_ws)
                                    .unwrap_or_default();
                                if let Some(fd) = st.iter().find(|x| &x.path == f) {
                                    push_file_rows(&mut r, fd, true);
                                    fdv.push(fd.clone());
                                }
                            }
                            (r, fdv)
                        }
                        None => (Vec::new(), Vec::new()),
                    };
                    Ok::<(Vec<StatusEntry>, Vec<DiffRow>, Vec<FileDiff>), String>((status, rows, fdiff))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((s, rows, fdiff)) => {
                        this.status = s;
                        this.status_error = None;
                        if this.wt_file.is_some() {
                            this.wt_rows = rows;
                            this.wt_diff = fdiff;
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

    /// Stages a single hunk (or unstages it, if `staged`). Builds the exact
    /// patch from the working-tree diff and applies it to the index.
    fn stage_hunk(&mut self, file: String, index: usize, staged: bool, cx: &mut Context<Self>) {
        let diffs = git_core::diff::workdir_diff(&self.repo_path, staged).unwrap_or_default();
        let patch = diffs
            .iter()
            .find(|x| x.path == file)
            .and_then(|fd| git_core::diff::build_hunk_patch(fd, index));
        match patch {
            Some(p) => {
                let r = git_core::Repo::open(&self.repo_path)
                    .map_err(|e| e.to_string())
                    .and_then(|repo| repo.apply_hunk_to_index(&p, staged));
                self.op_msg = Some(match r {
                    Ok(()) => format!("{} hunk · {}", if staged { "unstaged" } else { "staged" }, file),
                    Err(e) => format!("✗ {e}"),
                });
            }
            None => self.op_msg = Some("Could not build the hunk patch".into()),
        }
        self.refresh_status(cx);
    }

    /// Discards a single working-tree hunk (reverse-applies it to the tree).
    fn revert_hunk(&mut self, file: String, index: usize, cx: &mut Context<Self>) {
        let diffs = git_core::diff::workdir_diff(&self.repo_path, false).unwrap_or_default();
        let patch = diffs
            .iter()
            .find(|x| x.path == file)
            .and_then(|fd| git_core::diff::build_hunk_patch(fd, index));
        match patch {
            Some(p) => {
                let r = git_core::Repo::open(&self.repo_path)
                    .map_err(|e| e.to_string())
                    .and_then(|repo| repo.apply_hunk_to_worktree(&p, true));
                self.op_msg = Some(match r {
                    Ok(()) => format!("reverted hunk · {file}"),
                    Err(e) => format!("✗ {e}"),
                });
            }
            None => self.op_msg = Some("Could not build the hunk patch".into()),
        }
        self.refresh_status(cx);
    }

    fn toggle_ignore_ws(&mut self, cx: &mut Context<Self>) {
        self.diff_ignore_ws = !self.diff_ignore_ws;
        if let Some(ix) = self.selected {
            self.selected = None;
            self.select(ix, cx);
        }
        if self.view == ViewMode::Changes {
            self.refresh_status(cx);
        }
        cx.notify();
    }

    fn toggle_side_by_side(&mut self, cx: &mut Context<Self>) {
        self.diff_side_by_side = !self.diff_side_by_side;
        cx.notify();
    }

    fn toggle_syntax(&mut self, cx: &mut Context<Self>) {
        self.diff_syntax = !self.diff_syntax;
        cx.notify();
    }

    /// Scrolls the commit-diff list to the next/previous file or hunk.
    fn diff_nav(&mut self, forward: bool, cx: &mut Context<Self>) {
        let is_change =
            |r: &DiffRow| matches!(r, DiffRow::File(..) | DiffRow::Hunk { .. });
        let n = self.diff_rows.len();
        if n == 0 {
            return;
        }
        let next = if forward {
            (self.diff_cursor + 1..n).find(|&i| is_change(&self.diff_rows[i]))
        } else {
            (0..self.diff_cursor).rev().find(|&i| is_change(&self.diff_rows[i]))
        };
        if let Some(i) = next {
            self.diff_cursor = i;
            self.diff_scroll.scroll_to_item(i, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn do_commit(&mut self, amend: bool, cx: &mut Context<Self>) -> bool {
        let mut msg = self.commit_msg.trim().to_string();
        if msg.is_empty() && !amend {
            self.op_msg = Some("Write a commit message".into());
            cx.notify();
            return false;
        }
        if self.sign_off {
            if let Some((n, em)) = git_core::Repo::open(&self.repo_path).ok().and_then(|r| r.user()) {
                msg.push_str(&format!("\n\nSigned-off-by: {n} <{em}>"));
            }
        }
        let r = git_core::Repo::open(&self.repo_path).and_then(|repo| {
            if amend {
                repo.amend(&msg)
            } else {
                repo.commit(&msg)
            }
        });
        let ok = r.is_ok();
        match r {
            Ok(id) => {
                self.op_msg = Some(format!("✓ committed {}", short(&id)));
                self.commit_msg.clear();
                self.commit_cursor = 0;
                self.reload_log();
            }
            Err(e) => self.op_msg = Some(format!("✗ {e}")),
        }
        self.refresh_status(cx);
        ok
    }

    /// Commit, then push if the commit succeeded.
    fn commit_and_push(&mut self, cx: &mut Context<Self>) {
        if self.do_commit(false, cx) {
            self.run_remote("push", cx);
        }
    }

    /// Undo the last commit, keeping its changes staged.
    fn undo_commit(&mut self, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .and_then(|repo| repo.uncommit())
            .map(|_| "undid last commit (changes kept staged)".to_string());
        self.note(r);
        self.reload_log();
        self.refresh_status(cx);
    }

    fn on_commit_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        edit_key(&mut self.commit_msg, &mut self.commit_cursor, ev, true);
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
        self.branch_cursor = 0;
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
        edit_key(&mut self.new_branch, &mut self.branch_cursor, ev, false);
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
        // Refresh ref chips + ahead/behind + in-progress op so the log,
        // status bar and action banner stay current.
        self.refs = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.refs_by_commit())
            .unwrap_or_default();
        if let Ok(repo) = git_core::Repo::open(&self.repo_path) {
            self.ahead_behind = repo.ahead_behind().ok();
            self.op_state = repo.op_in_progress();
        }
    }

    /// Continues/aborts/skips an in-progress rebase/merge/cherry-pick.
    fn op_action(&mut self, which: &str, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path)
            .map_err(|e| e.to_string())
            .and_then(|repo| match which {
                "rcont" => repo.rebase_continue(),
                "rabort" => repo.rebase_abort(),
                "rskip" => repo.rebase_skip(),
                "mabort" => repo.merge_abort(),
                "cabort" => repo.cherry_pick_abort(),
                _ => Ok(String::new()),
            });
        self.note(r.map(|o| {
            let o = o.trim();
            if o.is_empty() { "done".to_string() } else { first_line(o) }
        }));
        self.reload_log();
        self.refresh_status(cx);
        cx.notify();
    }

    /// Rebases the current branch onto another branch.
    fn do_rebase_onto(&mut self, name: String, cx: &mut Context<Self>) {
        let r = git_core::Repo::open(&self.repo_path).and_then(|repo| repo.rebase_onto(&name));
        self.op_msg = Some(match r {
            Ok(RebaseResult::Done(h)) => format!("✓ rebased onto {name} → {}", short(&h)),
            Ok(RebaseResult::Conflict(c)) => format!("✗ conflict at {}", short(&c)),
            Err(e) => format!("✗ {e}"),
        });
        self.reload_log();
        cx.notify();
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
                        "fetchall" => repo.fetch_all(true),
                        "pull" => repo.pull(),
                        "pullrebase" => repo.pull_rebase(),
                        "pullmerge" => repo.pull_merge(),
                        "push" => repo.push("origin", &branch),
                        "pushforce" => repo.push_opts("origin", &branch, true, false, false),
                        "pushupstream" => repo.push_opts("origin", &branch, false, true, false),
                        "pushtags" => repo.push_opts("origin", &branch, false, false, true),
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
        // Autosquash: commits whose subject starts with fixup!/squash! are
        // pre-marked accordingly (like `git rebase --autosquash`).
        let steps = self.commits[0..=ix]
            .iter()
            .rev()
            .map(|c| {
                let action = if c.summary.starts_with("fixup!") {
                    RebaseAction::Fixup
                } else if c.summary.starts_with("squash!") {
                    RebaseAction::Squash
                } else {
                    RebaseAction::Pick
                };
                PlanRow {
                    id: c.id.clone(),
                    summary: c.summary.clone(),
                    action,
                }
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

/// Applies a key to a text buffer with a caret at `cursor` (byte index).
/// Supports left/right/home/end, backspace/delete, alt+backspace (delete word),
/// and inserting typed characters mid-string.
fn edit_key(buf: &mut String, cursor: &mut usize, ev: &KeyDownEvent, multiline: bool) {
    let m = &ev.keystroke.modifiers;
    *cursor = (*cursor).min(buf.len());
    match ev.keystroke.key.as_str() {
        "left" => *cursor = prev_boundary(buf, *cursor),
        "right" => *cursor = next_boundary(buf, *cursor),
        "home" | "up" => *cursor = 0,
        "end" | "down" => *cursor = buf.len(),
        "backspace" => {
            if m.alt || m.control {
                let start = word_start(buf, *cursor);
                buf.replace_range(start..*cursor, "");
                *cursor = start;
            } else if *cursor > 0 {
                let p = prev_boundary(buf, *cursor);
                buf.replace_range(p..*cursor, "");
                *cursor = p;
            }
        }
        "delete" => {
            if *cursor < buf.len() {
                let n = next_boundary(buf, *cursor);
                buf.replace_range(*cursor..n, "");
            }
        }
        "enter" => {
            if multiline {
                buf.insert(*cursor, '\n');
                *cursor += 1;
            }
        }
        "space" => {
            buf.insert(*cursor, ' ');
            *cursor += 1;
        }
        _ => {
            if let Some(c) = &ev.keystroke.key_char {
                if !c.is_empty() && !m.platform && !m.control {
                    buf.insert_str(*cursor, c);
                    *cursor += c.len();
                }
            }
        }
    }
}

/// Previous char boundary before `i`.
fn prev_boundary(s: &str, i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    let mut j = i - 1;
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

/// Next char boundary after `i`.
fn next_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// Start of the word before `i` (skips trailing spaces, then non-spaces).
fn word_start(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut j = i.min(s.len());
    while j > 0 && b[j - 1] == b' ' {
        j -= 1;
    }
    while j > 0 && b[j - 1] != b' ' {
        j -= 1;
    }
    j
}

/// Renders text with a caret glyph at `cursor`.
fn with_caret(text: &str, cursor: usize) -> String {
    let mut c = cursor.min(text.len());
    if !text.is_char_boundary(c) {
        c = prev_boundary(text, c);
    }
    format!("{}▏{}", &text[..c], &text[c..])
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

        // Refresh the commit-graph once per repo (background; speeds log/blame).
        if !self.graph_maintained {
            self.graph_maintained = true;
            let path = self.repo_path.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = git_core::Repo::open(&path).map(|r| r.write_commit_graph());
                })
                .detach();
        }

        // Focus a freshly-opened modal prompt's input.
        if self.focus_prompt {
            if let Some(p) = &self.prompt {
                window.focus(&p.focus);
            }
            self.focus_prompt = false;
        }
        // Focus a freshly-opened PR comment composer.
        if self.focus_pr_compose {
            window.focus(&self.pr_compose_focus);
            self.focus_pr_compose = false;
        }

        let toolbar = self.render_toolbar(cx);
        let banner = self.render_op_banner(cx);
        let body = if self.rebase.is_some() {
            self.render_rebase_editor(cx)
        } else {
            match self.view {
                ViewMode::Log => self.render_log(cx),
                ViewMode::Changes => self.render_changes(cx),
                ViewMode::Branches => self.render_branches(cx),
                ViewMode::Conflicts => self.render_conflicts(cx),
                ViewMode::Stashes => self.render_stashes(cx),
                ViewMode::Reflog => self.render_reflog(cx),
                ViewMode::Remotes => self.render_remotes(cx),
                ViewMode::Submodules => self.render_submodules(cx),
                ViewMode::PullRequests => self.render_prs(cx),
                ViewMode::Console => self.render_console(cx),
                ViewMode::Settings => self.render_settings(cx),
                ViewMode::FileHistory => self.render_file_history(cx),
                ViewMode::Search => self.render_search(cx),
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
            .children(banner)
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
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .mt_2()
                    .child(btn("welcome-open", "Open repository…", {
                        let e = e.clone();
                        move |app| {
                            e.update(app, |t, cx| t.open_dialog(cx));
                        }
                    }))
                    .child(btn("welcome-clone", "Clone from URL…", {
                        let e = e.clone();
                        move |app| {
                            e.update(app, |t, cx| t.open_prompt("Clone: url [target-dir]", "", PromptKind::Clone, cx));
                        }
                    })),
            )
            .children(err)
            .child(div().mt_4().text_color(color::dim()).text_sm().child("Recent"))
            .child(recents)
            .into_any_element()
    }

    /// Banner shown while a rebase/merge/cherry-pick is in progress.
    fn render_op_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let op = self.op_state.clone()?;
        let e = cx.entity();
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(rgb(0x4a3a1f))
            .border_b_1()
            .border_color(rgb(0xe6c46a))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(0xe6c46a))
                    .child(format!("⚠ {op} in progress — resolve conflicts, then continue")),
            )
            .child(btn("op-confl", "View conflicts", { let e = e.clone(); move |app| e.update(app, |t, cx| t.set_view(ViewMode::Conflicts, cx)) }));
        match op.as_str() {
            "rebase" => {
                row = row
                    .child(btn("op-cont", "Continue", { let e = e.clone(); move |app| e.update(app, |t, cx| t.op_action("rcont", cx)) }))
                    .child(btn("op-skip", "Skip", { let e = e.clone(); move |app| e.update(app, |t, cx| t.op_action("rskip", cx)) }))
                    .child(btn("op-abort", "Abort", { let e = e.clone(); move |app| e.update(app, |t, cx| t.op_action("rabort", cx)) }));
            }
            "merge" => {
                row = row.child(btn("op-mabort", "Abort merge", { let e = e.clone(); move |app| e.update(app, |t, cx| t.op_action("mabort", cx)) }));
            }
            _ => {
                row = row.child(btn("op-cabort", "Abort", { let e = e.clone(); move |app| e.update(app, |t, cx| t.op_action("cabort", cx)) }));
            }
        }
        Some(row.into_any_element())
    }

    /// Floating overlays (commit context menu, modal prompt) drawn on top.
    fn render_overlays(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        if self.more_open {
            out.push(self.render_more_menu(cx));
        }
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
                        with_caret(&p.value, p.cursor)
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
            .child({
                let secondary = !matches!(view, ViewMode::Log | ViewMode::Changes | ViewMode::Branches);
                let e2 = e.clone();
                div()
                    .id("tab-more")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .bg(if secondary { color::tab_active() } else { color::panel() })
                    .text_color(if secondary { color::accent() } else { color::dim() })
                    .hover(|s| s.bg(color::hover()))
                    .on_click(move |_, _, app| e2.update(app, |t, cx| t.toggle_more(cx)))
                    .child("More ▾")
            })
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
                        with_caret(&self.log_filter, self.filter_cursor)
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

        // Toggle bar (only when a file's diff is shown).
        let diff_bar = (!self.wt_rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w_full()
                .h(px(26.0))
                .px_2()
                .bg(color::panel())
                .border_b_1()
                .border_color(color::line())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(color::dim())
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(self.wt_file.clone().unwrap_or_default()),
                )
                .child(btn("wt-sbs", if self.diff_side_by_side { "Unified" } else { "Side-by-side" }, {
                    let e = e.clone();
                    move |app| e.update(app, |t, cx| t.toggle_side_by_side(cx))
                }))
                .child(btn("wt-ws", if self.diff_ignore_ws { "WS: ignored" } else { "Ignore WS" }, {
                    let e = e.clone();
                    move |app| e.update(app, |t, cx| t.toggle_ignore_ws(cx))
                }))
                .child(btn("wt-syn", if self.diff_syntax { "Syntax ✓" } else { "Syntax" }, {
                    let e = e.clone();
                    move |app| e.update(app, |t, cx| t.toggle_syntax(cx))
                }))
        });

        // working-tree diff of the selected file (virtualized)
        let diff_area = if self.wt_rows.is_empty() {
            div().flex_1().min_h_0().p_3().text_color(color::dim()).child("Select a file to view its diff").into_any_element()
        } else if self.diff_side_by_side {
            let rows = std::rc::Rc::new(build_side_rows(&self.wt_diff));
            let n = rows.len();
            let syntax_on = self.diff_syntax;
            div()
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_xs()
                .child(
                    uniform_list("wt-side", n, {
                        let rows = rows.clone();
                        move |range: std::ops::Range<usize>, _w, _c| {
                            range.filter_map(|i| rows.get(i).map(|r| side_row_el(r, syntax_on))).collect::<Vec<_>>()
                        }
                    })
                    .size_full(),
                )
                .into_any_element()
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
                        cx.processor({
                            let entity = e.clone();
                            move |this, range: std::ops::Range<usize>, _w, _c| {
                                let syntax_on = this.diff_syntax;
                                range
                                    .filter_map(|i| {
                                        this.wt_rows.get(i).map(|row| wt_row_el(row, &entity, syntax_on))
                                    })
                                    .collect::<Vec<_>>()
                            }
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
                        with_caret(&self.commit_msg, self.commit_cursor)
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(btn("do-commit", "Commit", { let e = e.clone(); move |app| { e.update(app, |t, cx| { t.do_commit(false, cx); }); } }))
                    .child(btn("do-commit-push", "Commit & Push", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.commit_and_push(cx)); } }))
                    .child(btn("do-amend", "Amend", { let e = e.clone(); move |app| { e.update(app, |t, cx| { t.do_commit(true, cx); }); } }))
                    .child(btn("do-undo", "Undo last", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.undo_commit(cx)); } }))
                    .child(btn("do-signoff", if self.sign_off { "Sign-off ✓" } else { "Sign-off" }, { let e = e.clone(); move |app| { e.update(app, |t, cx| { t.sign_off = !t.sign_off; cx.notify(); }); } })),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .children(diff_bar)
            .child(diff_area)
            .child(commit_box)
            .into_any_element()
    }

    /// Branches view: local branches (with checkout/merge/rename/upstream/delete),
    /// tags (delete/push) and worktrees, plus a create-branch box.
    fn render_branches(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut list = div()
            .id("branch-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();

        list = list.child(section_row("Local branches"));
        for b in &self.branches {
            let name = b.name.clone();
            let up = b.upstream.clone();
            let (e1, n1) = (e.clone(), name.clone());
            let (e2, n2) = (e.clone(), name.clone());
            let (e3, n3) = (e.clone(), name.clone());
            let (e4, n4) = (e.clone(), name.clone());
            let (e5, n5) = (e.clone(), name.clone());
            let (e6, n6) = (e.clone(), name.clone());
            let up_for_prompt = up.clone().unwrap_or_default();
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
                    .child(div().flex_none().w(px(12.0)).text_color(color::accent()).child(if b.is_head { "●" } else { "" }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(if b.is_head { color::accent() } else { color::fg() })
                            .child(name.clone()),
                    )
                    .child(div().flex_none().w(px(140.0)).whitespace_nowrap().text_ellipsis().text_xs().text_color(color::dim()).child(up.unwrap_or_default()))
                    .child(btn(&format!("co-{name}"), "checkout", move |app| { let n = n1.clone(); e1.update(app, |t, cx| t.do_checkout(n, cx)); }))
                    .child(btn(&format!("mg-{name}"), "merge", move |app| { let n = n2.clone(); e2.update(app, |t, cx| t.do_merge(n, cx)); }))
                    .child(btn(&format!("ro-{name}"), "rebase onto", move |app| { let n = n6.clone(); e6.update(app, |t, cx| t.do_rebase_onto(n, cx)); }))
                    .child(btn(&format!("rn-{name}"), "rename", move |app| { let n = n4.clone(); e4.update(app, |t, cx| t.open_prompt("Rename branch", &n, PromptKind::RenameBranch(n.clone()), cx)); }))
                    .child(btn(&format!("up-{name}"), "upstream", move |app| { let (n, u) = (n5.clone(), up_for_prompt.clone()); e5.update(app, |t, cx| t.open_prompt("Set upstream (empty to unset)", &u, PromptKind::SetUpstream(n.clone()), cx)); }))
                    .child(btn(&format!("rm-{name}"), "delete", move |app| { let n = n3.clone(); e3.update(app, |t, cx| t.do_delete_branch(n, cx)); })),
            );
        }

        // Tags
        list = list.child(section_row("Tags"));
        if self.tags.is_empty() {
            list = list.child(div().px_3().py_1().text_xs().text_color(color::dim()).child("No tags"));
        }
        for t in &self.tags {
            let name = t.name.clone();
            let (e1, n1) = (e.clone(), name.clone());
            let (e2, n2) = (e.clone(), name.clone());
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .h(px(28.0))
                    .hover(|s| s.bg(color::hover()))
                    .child(div().flex_none().text_color(color::tab_active()).text_color(rgb(0xe6c46a)).child("⌗"))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(name.clone()))
                    .child(div().flex_none().w(px(64.0)).font_family("Menlo").text_xs().text_color(color::dim()).child(short(&t.target)))
                    .child(btn(&format!("tpush-{name}"), "push", move |app| { let n = n1.clone(); e1.update(app, |t, cx| t.push_tag(n, cx)); }))
                    .child(btn(&format!("tdel-{name}"), "delete", move |app| { let n = n2.clone(); e2.update(app, |t, cx| t.delete_tag(n, cx)); })),
            );
        }

        // Worktrees (read live; cheap)
        let worktrees = git_core::Repo::open(&self.repo_path)
            .and_then(|r| r.worktrees())
            .unwrap_or_default();
        if !worktrees.is_empty() {
            list = list.child(section_row("Worktrees"));
            for w in worktrees {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .w_full()
                        .px_3()
                        .h(px(26.0))
                        .text_color(color::fg())
                        .child(w),
                );
            }
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
                        with_caret(&self.new_branch, self.branch_cursor)
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

impl RebasedApp {
    /// Conflicts view: file list + 3-way (base/ours/theirs) with resolve actions.
    fn render_conflicts(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        if self.conflicts.is_empty() {
            return titled(
                "Conflicts",
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(color::ok())
                    .child("No conflicts ✓")
                    .into_any_element(),
            );
        }

        let mut files = div()
            .id("confl-files")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(240.0))
            .border_r_1()
            .border_color(color::line())
            .overflow_y_scroll();
        for f in &self.conflicts {
            let sel = self.conflict_file.as_deref() == Some(f.as_str());
            let (e1, f1) = (e.clone(), f.clone());
            files = files.child(
                div()
                    .id(SharedString::from(format!("cf-{f}")))
                    .w_full()
                    .px_3()
                    .h(px(26.0))
                    .cursor_pointer()
                    .when(sel, |d| d.bg(color::sel()))
                    .hover(|s| s.bg(color::hover()))
                    .text_color(color::err())
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .on_click(move |_, _, app| {
                        let f = f1.clone();
                        e1.update(app, |t, cx| t.select_conflict(f, cx));
                    })
                    .child(f.clone()),
            );
        }

        let side = |title: &str, content: Option<&String>, col: Rgba| -> gpui::AnyElement {
            let body = content.cloned().unwrap_or_else(|| "(absent)".to_string());
            let lines: Vec<_> = body
                .lines()
                .map(|l| div().whitespace_nowrap().child(l.to_string()))
                .collect();
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(color::line())
                .child(div().px_2().py_1().bg(color::panel()).text_xs().text_color(col).child(title.to_string()))
                .child(
                    div()
                        .id(SharedString::from(format!("cs-{title}")))
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .font_family("Menlo")
                        .text_xs()
                        .p_2()
                        .flex()
                        .flex_col()
                        .children(lines),
                )
                .into_any_element()
        };
        let s = self.conflict_sides.as_ref();
        let panel = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(side("Base", s.and_then(|x| x.base.as_ref()), color::dim()))
            .child(side("Ours (HEAD)", s.and_then(|x| x.ours.as_ref()), color::accent()))
            .child(side("Theirs", s.and_then(|x| x.theirs.as_ref()), rgb(0xc8b8ec)));

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(color::fg())
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.conflict_file.clone().unwrap_or_default()),
            )
            .child(btn("c-ours", "Use ours", { let e = e.clone(); move |app| e.update(app, |t, cx| t.resolve_conflict(true, cx)) }))
            .child(btn("c-theirs", "Use theirs", { let e = e.clone(); move |app| e.update(app, |t, cx| t.resolve_conflict(false, cx)) }));

        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(files)
            .child(div().flex().flex_col().flex_1().min_h_0().child(toolbar).child(panel));
        titled(&format!("Conflicts ({})", self.conflicts.len()), body.into_any_element())
    }

    /// Stashes view.
    fn render_stashes(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut list = div().id("stash-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        if self.stashes.is_empty() {
            list = list.child(div().p_3().text_color(color::dim()).child("No stashes"));
        }
        for s in &self.stashes {
            let i = s.index;
            let (e1, e2, e3) = (e.clone(), e.clone(), e.clone());
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
                    .child(div().flex_none().w(px(70.0)).font_family("Menlo").text_xs().text_color(color::accent()).child(format!("stash@{{{i}}}")))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(s.message.clone()))
                    .child(btn(&format!("sp-{i}"), "pop", move |app| { e1.update(app, |t, cx| t.stash_pop_ix(i, cx)); }))
                    .child(btn(&format!("sa-{i}"), "apply", move |app| { e2.update(app, |t, cx| t.stash_apply_ix(i, cx)); }))
                    .child(btn(&format!("sd-{i}"), "drop", move |app| { e3.update(app, |t, cx| t.stash_drop_ix(i, cx)); })),
            );
        }
        let footer = div()
            .flex()
            .flex_row()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(btn("stash-new", "Stash working changes", { let e = e.clone(); move |app| e.update(app, |t, cx| t.do_stash(cx)) }));
        titled("Stashes", div().flex().flex_col().flex_1().min_h_0().child(list).child(footer).into_any_element())
    }

    /// Reflog view (HEAD).
    fn render_reflog(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut list = div().id("reflog-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        if self.reflog.is_empty() {
            list = list.child(div().p_3().text_color(color::dim()).child("Empty reflog"));
        }
        for (k, r) in self.reflog.iter().enumerate() {
            let id = r.id.clone();
            let e1 = e.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("rl-{k}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .h(px(26.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(color::hover()))
                    .on_click(move |_, _, app| {
                        let id = id.clone();
                        e1.update(app, |t, cx| t.go_to_hash(&id, cx));
                    })
                    .child(div().flex_none().w(px(56.0)).font_family("Menlo").text_color(color::accent()).child(short(&r.id)))
                    .child(div().flex_none().w(px(96.0)).text_xs().text_color(color::dim()).child(format!("HEAD@{{{k}}}")))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(r.message.clone())),
            );
        }
        titled("Reflog (HEAD)", list.into_any_element())
    }

    /// Remotes view.
    fn render_remotes(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let mut list = div().id("remotes-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        if self.remotes.is_empty() {
            list = list.child(div().p_3().text_color(color::dim()).child("No remotes"));
        }
        for r in &self.remotes {
            let name = r.name.clone();
            let (e1, n1) = (e.clone(), name.clone());
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
                    .child(div().flex_none().w(px(120.0)).text_color(color::fg()).whitespace_nowrap().text_ellipsis().child(name.clone()))
                    .child(div().flex_1().min_w_0().text_color(color::dim()).text_sm().whitespace_nowrap().text_ellipsis().child(r.url.clone()))
                    .child(btn(&format!("rmv-{name}"), "remove", move |app| { let n = n1.clone(); e1.update(app, |t, cx| t.remote_remove(n, cx)); })),
            );
        }
        let footer = div()
            .flex()
            .flex_row()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(btn("add-remote", "Add remote…", { let e = e.clone(); move |app| e.update(app, |t, cx| t.open_prompt("Add remote: name url", "origin ", PromptKind::AddRemote, cx)) }))
            .child(btn("fetch-all", "Fetch all (prune)", { let e = e.clone(); move |app| e.update(app, |t, cx| t.run_remote("fetchall", cx)) }))
            .child(btn("pull-r", "Pull --rebase", { let e = e.clone(); move |app| e.update(app, |t, cx| t.run_remote("pullrebase", cx)) }))
            .child(btn("pull-m", "Pull --merge", { let e = e.clone(); move |app| e.update(app, |t, cx| t.run_remote("pullmerge", cx)) }))
            .child(btn("push-f", "Push --force-with-lease", { let e = e.clone(); move |app| e.update(app, |t, cx| t.run_remote("pushforce", cx)) }))
            .child(btn("push-u", "Push -u", { let e = e.clone(); move |app| e.update(app, |t, cx| t.run_remote("pushupstream", cx)) }))
            .child(btn("push-t", "Push --tags", { let e = e.clone(); move |app| e.update(app, |t, cx| t.run_remote("pushtags", cx)) }));
        titled("Remotes", div().flex().flex_col().flex_1().min_h_0().child(list).child(footer).into_any_element())
    }

    /// Submodules view.
    fn render_submodules(&self, _cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut list = div().id("subm-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        if self.submodules.is_empty() {
            list = list.child(div().p_3().text_color(color::dim()).child("No submodules"));
        }
        for s in &self.submodules {
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .h(px(28.0))
                    .child(div().flex_none().w(px(180.0)).text_color(color::fg()).whitespace_nowrap().text_ellipsis().child(s.name.clone()))
                    .child(div().flex_1().min_w_0().text_color(color::dim()).text_sm().whitespace_nowrap().text_ellipsis().child(s.path.clone()))
                    .child(div().flex_none().w(px(64.0)).font_family("Menlo").text_xs().text_color(color::accent()).child(s.head.as_deref().map(short).unwrap_or_default())),
            );
        }
        titled("Submodules", list.into_any_element())
    }

    /// GitHub pull requests (via the `gh` CLI).
    fn render_prs(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // An open review session takes over the whole view.
        if self.pr_view.is_some() {
            return self.render_pr_detail(cx);
        }
        let e = cx.entity();
        let mut list = div().id("prs-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        if let Some(m) = &self.prs_msg {
            list = list.child(div().p_3().text_color(color::dim()).child(m.clone()));
        }
        for p in &self.prs {
            let num = p.number.clone();
            let (e0, n0) = (e.clone(), num.clone());
            let (e1, n1) = (e.clone(), num.clone());
            let (e2, n2) = (e.clone(), num.clone());
            let state_col = if p.state == "OPEN" { color::ok() } else { color::dim() };
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .h(px(32.0))
                    .hover(|s| s.bg(color::hover()))
                    .child(div().flex_none().w(px(48.0)).font_family("Menlo").text_color(color::accent()).child(format!("#{num}")))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(p.title.clone()))
                    .child(div().flex_none().w(px(150.0)).whitespace_nowrap().text_ellipsis().text_xs().text_color(color::dim()).child(p.branch.clone()))
                    .child(div().flex_none().w(px(90.0)).whitespace_nowrap().text_ellipsis().text_xs().text_color(color::dim()).child(p.author.clone()))
                    .child(div().flex_none().w(px(56.0)).text_xs().text_color(state_col).child(p.state.clone()))
                    .child(btn(&format!("pr-rev-{num}"), "review", move |app| { let n = n0.clone(); e0.update(app, |t, cx| t.open_pr(n, cx)); }))
                    .child(btn(&format!("pr-co-{num}"), "checkout", move |app| { let n = n1.clone(); e1.update(app, |t, cx| t.pr_action(n, false, cx)); }))
                    .child(btn(&format!("pr-web-{num}"), "web", move |app| { let n = n2.clone(); e2.update(app, |t, cx| t.pr_action(n, true, cx)); })),
            );
        }
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(btn("pr-refresh", "Refresh", { let e = e.clone(); move |app| e.update(app, |t, cx| t.refresh_prs(cx)) }))
            .child(div().flex_1())
            .child(div().text_xs().text_color(color::dim()).child("via gh CLI"));
        titled("Pull requests", div().flex().flex_col().flex_1().min_h_0().child(list).child(footer).into_any_element())
    }

    /// PR review view: header + actions + (conversation + diff + inline threads)
    /// + the inline composer panel when active.
    fn render_pr_detail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let v = self.pr_view.as_ref().unwrap();
        let d = &v.detail;

        let state_col = if d.state == "OPEN" { color::ok() } else { color::dim() };
        let header = div()
            .flex()
            .flex_col()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(btn("pr-back", "← PRs", { let e = e.clone(); move |app| e.update(app, |t, cx| t.close_pr(cx)) }))
                    .child(div().flex_none().font_family("Menlo").text_color(color::accent()).child(format!("#{}", d.number)))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(d.title.clone()))
                    .child(div().flex_none().text_xs().text_color(state_col).child(d.state.clone())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .text_xs()
                    .text_color(color::dim())
                    .child(format!("@{}", d.author))
                    .child(format!("{} ← {}", d.base, d.head))
                    .child(div().text_color(color::add_fg()).child(format!("+{}", d.additions)))
                    .child(div().text_color(color::del_fg()).child(format!("−{}", d.deletions))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .child(btn("pr-d-refresh", "Refresh", { let e = e.clone(); move |app| e.update(app, |t, cx| t.reload_pr(cx)) }))
                    .child(btn("pr-d-approve", "✓ Approve", { let e = e.clone(); move |app| e.update(app, |t, cx| t.pr_compose_to(ComposeTarget::Review { kind: "approve".into() }, cx)) }))
                    .child(btn("pr-d-request", "✗ Request changes", { let e = e.clone(); move |app| e.update(app, |t, cx| t.pr_compose_to(ComposeTarget::Review { kind: "request-changes".into() }, cx)) }))
                    .child(btn("pr-d-comment", "💬 Comment", { let e = e.clone(); move |app| e.update(app, |t, cx| t.pr_compose_to(ComposeTarget::General, cx)) }))
                    .child(btn("pr-d-co", "Checkout", { let e = e.clone(); let n = d.number.clone(); move |app| { let n = n.clone(); e.update(app, |t, cx| t.pr_action(n, false, cx)); } }))
                    .child(btn("pr-d-web", "Web", { let e = e.clone(); let n = d.number.clone(); move |app| { let n = n.clone(); e.update(app, |t, cx| t.pr_action(n, true, cx)); } })),
            );

        let body: gpui::AnyElement = if v.loading {
            div().flex_1().p_3().text_color(color::dim()).child("Loading pull request…").into_any_element()
        } else if let Some(err) = &v.error {
            div().flex_1().p_3().text_color(color::err()).child(format!("gh: {err}")).into_any_element()
        } else {
            let entity = e.clone();
            let syntax_on = self.diff_syntax;
            let desc = (!d.body.trim().is_empty()).then(|| {
                let mut col = div().flex().flex_col().w_full().px_3().py_2().bg(color::bg()).border_b_1().border_color(color::line()).text_sm().text_color(color::fg());
                for line in d.body.lines().take(40) {
                    col = col.child(div().child(line.to_string()));
                }
                col
            });
            // Comment cards have variable height, so this list is a plain
            // scroll column (not uniform_list). PR diffs are bounded; we cap
            // rendered rows for safety and note any truncation.
            const CAP: usize = 4000;
            let mut list = div()
                .id("pr-rows")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .font_family("Menlo")
                .text_xs();
            for r in v.rows.iter().take(CAP) {
                list = list.child(pr_row_el(r, &entity, syntax_on));
            }
            if v.rows.len() > CAP {
                list = list.child(
                    div().p_2().text_color(color::dim()).child(format!("… {} more rows (truncated)", v.rows.len() - CAP)),
                );
            }
            div().flex().flex_col().flex_1().min_h_0().children(desc).child(list).into_any_element()
        };

        let composer = self.pr_compose_target.as_ref().map(|target| {
            let label = target.label();
            let submit_label = match target {
                ComposeTarget::Review { kind } if kind == "approve" => "Approve",
                ComposeTarget::Review { kind } if kind == "request-changes" => "Request changes",
                ComposeTarget::Review { .. } => "Submit review",
                ComposeTarget::Reply { .. } => "Reply",
                _ => "Comment",
            };
            div()
                .flex()
                .flex_col()
                .w_full()
                .gap_1()
                .p_2()
                .border_t_1()
                .border_color(color::line())
                .bg(color::panel())
                .child(div().text_xs().text_color(color::accent()).child(label))
                .child(
                    div()
                        .id("pr-compose")
                        .w_full()
                        .h(px(60.0))
                        .p_2()
                        .bg(color::bg())
                        .rounded_md()
                        .border_1()
                        .border_color(color::line())
                        .track_focus(&self.pr_compose_focus)
                        .key_context("prcompose")
                        .on_key_down(cx.listener(Self::on_pr_compose_key))
                        .on_click(cx.listener(|t, _, w, _| w.focus(&t.pr_compose_focus)))
                        .font_family("Menlo")
                        .text_xs()
                        .text_color(if self.pr_compose.is_empty() { color::dim() } else { color::fg() })
                        .child(if self.pr_compose.is_empty() {
                            "Write a comment…  (⌘↵ to submit)".to_string()
                        } else {
                            with_caret(&self.pr_compose, self.pr_compose_cursor)
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(btn("pr-submit", submit_label, { let e = e.clone(); move |app| e.update(app, |t, cx| t.pr_submit(cx)) }))
                        .child(btn("pr-cancel", "Cancel", { let e = e.clone(); move |app| e.update(app, |t, cx| t.pr_cancel_compose(cx)) })),
                )
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(body)
            .children(composer)
            .into_any_element()
    }

    /// File-history view (commits touching one file) over the shared diff panel.
    fn render_file_history(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        self.aux_list_view(cx, false)
    }

    /// Pickaxe search view (a term box + matching commits) over the diff panel.
    fn render_search(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        self.aux_list_view(cx, true)
    }

    /// Shared layout for the ad-hoc commit lists (file history & search):
    /// header · [search bar] · virtualized commit list · diff head · diff body.
    fn aux_list_view(&self, cx: &mut Context<Self>, search: bool) -> gpui::AnyElement {
        let e = cx.entity();
        let label = self.aux.as_ref().map(|a| a.label.clone()).unwrap_or_default();
        let n = self.aux.as_ref().map(|a| a.commits.len()).unwrap_or(0);
        let msg = self.aux.as_ref().and_then(|a| a.msg.clone());

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .text_sm()
            .child(btn("aux-back", "← Log", {
                let e = e.clone();
                move |app| e.update(app, |t, cx| t.set_view(ViewMode::Log, cx))
            }))
            .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::accent()).child(label))
            .child(div().flex_none().text_xs().text_color(color::dim()).child(format!("{n} commits")));

        let search_bar = search.then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w_full()
                .h(px(34.0))
                .px_2()
                .bg(color::panel())
                .border_b_1()
                .border_color(color::line())
                .child(div().flex_none().text_color(color::dim()).text_sm().child("⌕"))
                .child(
                    div()
                        .id("search-input")
                        .flex_1()
                        .min_w_0()
                        .h(px(24.0))
                        .px_2()
                        .bg(color::bg())
                        .rounded_md()
                        .border_1()
                        .border_color(color::line())
                        .track_focus(&self.search_focus)
                        .key_context("search")
                        .on_key_down(cx.listener(Self::on_search_key))
                        .on_click(cx.listener(|t, _, w, _| w.focus(&t.search_focus)))
                        .font_family("Menlo")
                        .text_sm()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(if self.search_term.is_empty() { color::dim() } else { color::fg() })
                        .child(if self.search_term.is_empty() {
                            "Find commits that add/remove this text — Enter to run".to_string()
                        } else {
                            with_caret(&self.search_term, self.search_cursor)
                        }),
                )
                .child(btn("search-regex", if self.search_regex { "regex ✓" } else { "regex" }, {
                    let e = e.clone();
                    move |app| e.update(app, |t, cx| { t.search_regex = !t.search_regex; cx.notify(); })
                }))
                .child(btn("search-run", "Run", {
                    let e = e.clone();
                    move |app| e.update(app, |t, cx| t.run_search(cx))
                }))
        });

        let list_area = if n == 0 {
            div()
                .flex_none()
                .h(px(220.0))
                .p_3()
                .text_color(color::dim())
                .border_b_1()
                .border_color(color::line())
                .child(msg.unwrap_or_else(|| "—".into()))
        } else {
            let entity = e.clone();
            div()
                .flex_none()
                .h(px(220.0))
                .border_b_1()
                .border_color(color::line())
                .child(
                    uniform_list(
                        "aux-commits",
                        n,
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, _c| {
                            let Some(aux) = &this.aux else { return Vec::new() };
                            range
                                .filter_map(|ix| {
                                    aux.commits.get(ix).map(|c| {
                                        aux_commit_row(c, ix, aux.selected == Some(ix), entity.clone())
                                    })
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .size_full(),
                )
        };

        // Diff head: selected commit summary + the same toggles as the main panel.
        let sel_summary = self
            .aux
            .as_ref()
            .and_then(|a| a.selected.and_then(|i| a.commits.get(i)))
            .map(|c| format!("{}  {}", c.id.get(..8).unwrap_or(&c.id), c.summary))
            .unwrap_or_else(|| "Select a commit to view its diff".into());
        let diff_head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(color::panel())
            .border_t_1()
            .border_color(color::line())
            .text_sm()
            .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().font_family("Menlo").text_color(color::dim()).child(sel_summary))
            .child(btn("aux-sbs", if self.diff_side_by_side { "Unified" } else { "Side-by-side" }, {
                let e = e.clone();
                move |app| e.update(app, |t, cx| t.toggle_side_by_side(cx))
            }))
            .child(btn("aux-syn", if self.diff_syntax { "Syntax ✓" } else { "Syntax" }, {
                let e = e.clone();
                move |app| e.update(app, |t, cx| t.toggle_syntax(cx))
            }));

        let body = self.diff_body(cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(header)
            .children(search_bar)
            .child(list_area)
            .child(diff_head)
            .child(body)
            .into_any_element()
    }

    /// Built-in git console.
    fn render_console(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let lines: Vec<_> = self
            .console_output
            .lines()
            .map(|l| {
                let col = if l.starts_with("$ ") {
                    color::accent()
                } else if l.starts_with("error:") {
                    color::err()
                } else {
                    color::fg()
                };
                div().whitespace_nowrap().text_color(col).child(l.to_string())
            })
            .collect();
        let out = div()
            .id("console-out")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .font_family("Menlo")
            .text_xs()
            .p_2()
            .flex()
            .flex_col()
            .children(lines);
        let input = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(color::line())
            .bg(color::panel())
            .child(div().flex_none().text_color(color::dim()).child("git"))
            .child(
                div()
                    .id("console-input")
                    .flex_1()
                    .h(px(28.0))
                    .px_2()
                    .bg(color::bg())
                    .rounded_md()
                    .border_1()
                    .border_color(color::line())
                    .track_focus(&self.console_focus)
                    .key_context("console")
                    .on_key_down(cx.listener(Self::on_console_key))
                    .on_click(cx.listener(|t, _, w, _| w.focus(&t.console_focus)))
                    .font_family("Menlo")
                    .text_sm()
                    .text_color(if self.console_input.is_empty() { color::dim() } else { color::fg() })
                    .child(if self.console_input.is_empty() {
                        "status · log --oneline -5 · diff --stat …".to_string()
                    } else {
                        with_caret(&self.console_input, self.console_cursor)
                    }),
            )
            .child(btn("console-run", "Run", { let e = cx.entity(); move |app| e.update(app, |t, cx| t.run_console(cx)) }));
        titled("Git console", div().flex().flex_col().flex_1().min_h_0().child(out).child(input).into_any_element())
    }

    /// Settings / info view.
    fn render_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let row = |k: &str, v: String| {
            div()
                .flex()
                .flex_row()
                .gap_3()
                .px_3()
                .py_1()
                .child(div().flex_none().w(px(150.0)).text_color(color::dim()).child(k.to_string()))
                .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(v))
        };
        let ab = self
            .ahead_behind
            .map(|a| format!("ahead {}, behind {}", a.ahead, a.behind))
            .unwrap_or_else(|| "—".into());
        let branch = self.branches.iter().find(|b| b.is_head).map(|b| b.name.clone()).unwrap_or_default();
        let body = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .child(row("Repository", self.repo_path.clone()))
            .child(row("Branch", branch))
            .child(row("Upstream", ab))
            .child(row("Recent repos", self.recents.len().to_string()))
            .child(div().h(px(10.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .px_3()
                    .child(btn("set-ws", if self.diff_ignore_ws { "Ignore WS: on" } else { "Ignore WS: off" }, { let e = e.clone(); move |app| e.update(app, |t, cx| t.toggle_ignore_ws(cx)) }))
                    .child(btn("set-sbs", if self.diff_side_by_side { "Side-by-side: on" } else { "Side-by-side: off" }, { let e = e.clone(); move |app| e.update(app, |t, cx| t.toggle_side_by_side(cx)) }))
                    .child(btn("set-signoff", if self.sign_off { "Sign-off: on" } else { "Sign-off: off" }, { let e = e.clone(); move |app| e.update(app, |t, cx| { t.sign_off = !t.sign_off; cx.notify(); }) })),
            );
        titled("Settings", body.into_any_element())
    }

    /// "More views" dropdown (Conflicts/Stashes/Reflog/Remotes/Submodules/Console/Settings).
    fn render_more_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = cx.entity();
        let nav = |key: &'static str, label: &'static str, view: ViewMode| {
            let e = e.clone();
            menu_row(key, label, move |app| {
                e.update(app, |t, cx| t.set_view(view, cx));
            })
        };
        let panel = div()
            .flex()
            .flex_col()
            .w(px(180.0))
            .bg(color::panel())
            .border_1()
            .border_color(color::line())
            .rounded_md()
            .py_1()
            .child(nav("mv-conf", "Conflicts", ViewMode::Conflicts))
            .child(nav("mv-stash", "Stashes", ViewMode::Stashes))
            .child(nav("mv-reflog", "Reflog", ViewMode::Reflog))
            .child(nav("mv-remotes", "Remotes", ViewMode::Remotes))
            .child(nav("mv-subm", "Submodules", ViewMode::Submodules))
            .child(nav("mv-search", "Search in changes", ViewMode::Search))
            .child(nav("mv-prs", "Pull requests", ViewMode::PullRequests))
            .child(nav("mv-console", "Git console", ViewMode::Console))
            .child(nav("mv-settings", "Settings", ViewMode::Settings));
        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(MouseButton::Left, { let e = e.clone(); move |_, _, app| e.update(app, |t, cx| t.toggle_more(cx)) });
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(gpui::anchored().position(gpui::point(px(330.0), px(40.0))).child(panel))
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

/// Wraps a view body with a titled header bar.
fn titled(title: &str, body: gpui::AnyElement) -> gpui::AnyElement {
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
                .child(title.to_string()),
        )
        .child(body)
        .into_any_element()
}

/// A small dim section label inside a list.
fn section_row(label: &str) -> impl IntoElement {
    div()
        .w_full()
        .px_3()
        .py_1()
        .text_xs()
        .text_color(color::dim())
        .bg(color::row_line())
        .child(label.to_string())
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

/// Working-tree diff row, with per-hunk Stage/Unstage/Revert actions.
fn wt_row_el(row: &DiffRow, entity: &Entity<RebasedApp>, syntax_on: bool) -> gpui::AnyElement {
    match row {
        DiffRow::File(_, label) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .bg(color::panel())
            .text_color(color::fg())
            .child(label.clone())
            .into_any_element(),
        DiffRow::Hunk { file, index, header, staged } => {
            let staged = *staged;
            let idx = *index;
            let chip = |key: String, label: &'static str, fg: Rgba, on: Box<dyn Fn(&mut App)>| {
                div()
                    .id(SharedString::from(key))
                    .px_1()
                    .rounded_sm()
                    .bg(color::btn())
                    .cursor_pointer()
                    .text_xs()
                    .text_color(fg)
                    .hover(|s| s.bg(color::hover()))
                    .on_click(move |_, _, app| on(app))
                    .child(label)
            };
            let (e1, f1) = (entity.clone(), file.clone());
            let mut r = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_3()
                .text_color(color::dim())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(header.clone()),
                )
                .child(chip(
                    format!("sh-{file}-{idx}-{staged}"),
                    if staged { "− unstage" } else { "+ stage" },
                    color::fg(),
                    Box::new(move |app| {
                        let f = f1.clone();
                        e1.update(app, |t, cx| t.stage_hunk(f, idx, staged, cx));
                    }),
                ));
            if !staged {
                let (e2, f2) = (entity.clone(), file.clone());
                r = r.child(chip(
                    format!("rh-{file}-{idx}"),
                    "↶ revert",
                    color::del_fg(),
                    Box::new(move |app| {
                        let f = f2.clone();
                        e2.update(app, |t, cx| t.revert_hunk(f, idx, cx));
                    }),
                ));
            }
            r.into_any_element()
        }
        DiffRow::Line(origin, content, lang) => {
            let (fg, bg, sign) = match origin {
                LineOrigin::Add => (color::add_fg(), color::add_bg(), "+"),
                LineOrigin::Del => (color::del_fg(), color::del_bg(), "−"),
                LineOrigin::Context => (color::fg(), color::bg(), " "),
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_3()
                .bg(bg)
                .child(div().flex_none().w(px(12.0)).text_color(fg).child(sign))
                .child(line_spans(content, *lang, syntax_on, fg))
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
                    .child(btn("d-sbs", if self.diff_side_by_side { "Unified" } else { "Side-by-side" }, {
                        let e = cx.entity();
                        move |app| e.update(app, |t, cx| t.toggle_side_by_side(cx))
                    }))
                    .child(btn("d-ws", if self.diff_ignore_ws { "WS: ignored" } else { "Ignore WS" }, {
                        let e = cx.entity();
                        move |app| e.update(app, |t, cx| t.toggle_ignore_ws(cx))
                    }))
                    .child(btn("d-syn", if self.diff_syntax { "Syntax ✓" } else { "Syntax" }, {
                        let e = cx.entity();
                        move |app| e.update(app, |t, cx| t.toggle_syntax(cx))
                    }))
                    .child(btn("d-prev", "↑", {
                        let e = cx.entity();
                        move |app| e.update(app, |t, cx| t.diff_nav(false, cx))
                    }))
                    .child(btn("d-next", "↓", {
                        let e = cx.entity();
                        move |app| e.update(app, |t, cx| t.diff_nav(true, cx))
                    }))
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

        let body = self.diff_body(cx);
        container.child(head).child(body).into_any_element()
    }

    /// The virtualized diff body (error / empty / side-by-side / unified).
    /// Shared by the commit-diff panel and the file-history/search panels.
    fn diff_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(err) = &self.diff_error {
            div().flex_1().p_3().text_color(color::err()).child(err.clone()).into_any_element()
        } else if self.diff_rows.is_empty() {
            div().flex_1().p_3().text_color(color::dim()).child("No changes in this commit").into_any_element()
        } else if self.diff_side_by_side {
            let rows = std::rc::Rc::new(build_side_rows(&self.diff));
            let n = rows.len();
            let syntax_on = self.diff_syntax;
            div()
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_xs()
                .child(
                    uniform_list("side-rows", n, {
                        let rows = rows.clone();
                        move |range: std::ops::Range<usize>, _w, _cx| {
                            range.filter_map(|i| rows.get(i).map(|r| side_row_el(r, syntax_on))).collect::<Vec<_>>()
                        }
                    })
                    .track_scroll(self.diff_scroll.clone())
                    .size_full(),
                )
                .into_any_element()
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
                            let syntax_on = this.diff_syntax;
                            range
                                .filter_map(|i| this.diff_rows.get(i).map(|r| diff_row_el(r, &entity, syntax_on)))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(self.diff_scroll.clone())
                    .size_full(),
                )
                .into_any_element()
        }
    }
}

/// A virtualized diff row: file header (with blame/history actions), hunk, or ± line.
fn diff_row_el(row: &DiffRow, entity: &Entity<RebasedApp>, syntax_on: bool) -> gpui::AnyElement {
    match row {
        DiffRow::File(path, label) => div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .bg(color::panel())
            .text_color(color::fg())
            .child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().child(label.clone()))
            .child(file_action_chip("bl", path, "blame", entity.clone(), |t, p, cx| t.show_blame(p, cx)))
            .child(file_action_chip("hi", path, "history", entity.clone(), |t, p, cx| t.show_file_history(p, cx)))
            .into_any_element(),
        DiffRow::Hunk { header, .. } => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .text_color(color::dim())
            .child(header.clone())
            .into_any_element(),
        DiffRow::Line(origin, content, lang) => {
            let (fg, bg, sign) = match origin {
                LineOrigin::Add => (color::add_fg(), color::add_bg(), "+"),
                LineOrigin::Del => (color::del_fg(), color::del_bg(), "−"),
                LineOrigin::Context => (color::fg(), color::bg(), " "),
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_3()
                .bg(bg)
                .child(div().flex_none().w(px(12.0)).text_color(fg).child(sign))
                .child(line_spans(content, *lang, syntax_on, fg))
                .into_any_element()
        }
    }
}

/// A small clickable chip on a diff file header (e.g. "blame", "history").
fn file_action_chip(
    key: &str,
    path: &str,
    label: &'static str,
    entity: Entity<RebasedApp>,
    action: fn(&mut RebasedApp, String, &mut Context<RebasedApp>),
) -> impl IntoElement {
    let p = path.to_string();
    div()
        .id(SharedString::from(format!("{key}:{path}")))
        .flex_none()
        .px_1()
        .rounded_sm()
        .bg(color::btn())
        .cursor_pointer()
        .text_xs()
        .text_color(color::accent())
        .hover(|s| s.bg(color::hover()))
        .on_click(move |_, _, app| {
            let p = p.clone();
            entity.update(app, |t, cx| action(t, p, cx));
        })
        .child(label)
}

/// Renders one PR review row: a conversation card, a file/hunk header, a diff
/// line (with an "add comment" affordance), or an inline comment card.
fn pr_row_el(row: &PrRow, entity: &Entity<RebasedApp>, syntax_on: bool) -> gpui::AnyElement {
    match row {
        PrRow::Conversation(c) => pr_comment_card(c, entity, false),
        PrRow::Comment(c) => pr_comment_card(c, entity, true),
        PrRow::File(label) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .bg(color::panel())
            .text_color(color::fg())
            .child(label.clone())
            .into_any_element(),
        PrRow::Hunk(h) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .text_color(color::dim())
            .child(h.clone())
            .into_any_element(),
        PrRow::Line { origin, content, lang, path, line, side } => {
            let (fg, bg, sign) = match origin {
                LineOrigin::Add => (color::add_fg(), color::add_bg(), "+"),
                LineOrigin::Del => (color::del_fg(), color::del_bg(), "−"),
                LineOrigin::Context => (color::fg(), color::bg(), " "),
            };
            let (p, s, ln) = (path.clone(), side.clone(), *line);
            let e = entity.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(DIFF_ROW_H))
                .w_full()
                .px_2()
                .bg(bg)
                .child(
                    div()
                        .id(SharedString::from(format!("prc:{path}:{side}:{line}")))
                        .flex_none()
                        .w(px(16.0))
                        .text_color(color::dim())
                        .cursor_pointer()
                        .hover(|x| x.text_color(color::accent()))
                        .on_click(move |_, _, app| {
                            let (p, s) = (p.clone(), s.clone());
                            e.update(app, |t, cx| {
                                t.pr_compose_to(ComposeTarget::Line { path: p, line: ln, side: s }, cx)
                            });
                        })
                        .child("✚"),
                )
                .child(div().flex_none().w(px(12.0)).text_color(fg).child(sign))
                .child(line_spans(content, *lang, syntax_on, fg))
                .into_any_element()
        }
    }
}

/// A comment card (conversation or inline). Inline cards are indented/tinted.
fn pr_comment_card(c: &PrComment, entity: &Entity<RebasedApp>, inline: bool) -> gpui::AnyElement {
    let anchor = if c.path.is_empty() {
        String::new()
    } else {
        format!("  ·  {}:{}", c.path, c.line)
    };
    let when = c.created_at.split('T').next().unwrap_or("").to_string();
    let (id, author) = (c.id.clone(), c.author.clone());
    let e = entity.clone();

    let mut body_col = div().flex().flex_col().w_full().text_color(color::fg());
    if c.body.trim().is_empty() {
        body_col = body_col.child(div().text_color(color::dim()).child("(no text)"));
    } else {
        for line in c.body.lines() {
            body_col = body_col.child(div().w_full().child(line.to_string()));
        }
    }

    div()
        .flex()
        .flex_col()
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .when(inline, |d| d.pl(px(28.0)).bg(color::panel()))
        .border_b_1()
        .border_color(color::row_line())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().flex_none().text_xs().text_color(color::accent()).child(format!("@{author}")))
                .child(div().flex_1().min_w_0().text_xs().text_color(color::dim()).whitespace_nowrap().text_ellipsis().child(format!("{when}{anchor}")))
                .child(btn(&format!("pr-reply-{id}"), "reply", move |app| {
                    let (id, author) = (id.clone(), author.clone());
                    e.update(app, |t, cx| {
                        t.pr_compose_to(ComposeTarget::Reply { comment_id: id, label: format!("@{author}") }, cx)
                    });
                })),
        )
        .child(body_col)
        .into_any_element()
}

/// A side-by-side diff row: file header, hunk header, or a left/right pair.
fn side_row_el(row: &SideRow, syntax_on: bool) -> gpui::AnyElement {
    match row {
        SideRow::File(label) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .bg(color::panel())
            .text_color(color::fg())
            .child(label.clone())
            .into_any_element(),
        SideRow::Hunk(h) => div()
            .flex()
            .items_center()
            .h(px(DIFF_ROW_H))
            .w_full()
            .px_3()
            .text_color(color::dim())
            .child(h.clone())
            .into_any_element(),
        SideRow::Pair(l, r) => div()
            .flex()
            .flex_row()
            .h(px(DIFF_ROW_H))
            .w_full()
            .child(side_cell(l, syntax_on))
            .child(div().flex_none().w(px(1.0)).h(px(DIFF_ROW_H)).bg(color::line()))
            .child(side_cell(r, syntax_on))
            .into_any_element(),
    }
}

/// One half of a side-by-side row (empty filler if `None`).
fn side_cell(cell: &Option<SideCell>, syntax_on: bool) -> gpui::AnyElement {
    let Some(c) = cell else {
        return div().flex_1().min_w_0().h(px(DIFF_ROW_H)).bg(color::bg()).into_any_element();
    };
    let (fg, bg) = match c.kind {
        LineOrigin::Add => (color::add_fg(), color::add_bg()),
        LineOrigin::Del => (color::del_fg(), color::del_bg()),
        LineOrigin::Context => (color::fg(), color::bg()),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w_0()
        .h(px(DIFF_ROW_H))
        .px_2()
        .gap_2()
        .bg(bg)
        .text_color(fg)
        .child(
            div()
                .flex_none()
                .w(px(38.0))
                .text_color(color::dim())
                .child(c.lineno.map(|n| n.to_string()).unwrap_or_default()),
        )
        .child(line_spans(&c.text, c.lang, syntax_on, fg))
        .into_any_element()
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

/// A commit row for the ad-hoc lists (file history / search): no graph gutter,
/// clickable → loads that commit's diff into the shared panel.
fn aux_commit_row(
    c: &CommitInfo,
    ix: usize,
    selected: bool,
    entity: Entity<RebasedApp>,
) -> gpui::AnyElement {
    let short = c.id.get(..8).unwrap_or(&c.id).to_string();
    let mut row = div()
        .id(ix)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(ROW_H))
        .px_3()
        .border_b_1()
        .border_color(color::row_line())
        .cursor_pointer()
        .hover(|s| s.bg(color::hover()))
        .on_click(move |_, _, app| {
            entity.update(app, |t, cx| t.aux_select(ix, cx));
        });
    if selected {
        row = row.bg(color::sel());
    }
    row.child(div().flex_1().min_w_0().whitespace_nowrap().text_ellipsis().text_color(color::fg()).child(truncate(&c.summary, 120)))
        .child(div().flex_none().w(px(150.0)).whitespace_nowrap().text_ellipsis().text_color(color::dim()).text_sm().child(c.author.clone()))
        .child(div().flex_none().w(px(88.0)).whitespace_nowrap().font_family("Menlo").text_color(color::dim()).text_sm().child(fmt_date(c.time)))
        .child(div().flex_none().w(px(68.0)).whitespace_nowrap().font_family("Menlo").text_color(color::dim()).text_sm().child(short))
        .into_any_element()
}

/// Graph gutter: a `canvas` painting vertical lines + merge/fork curves, with
/// the commit dot overlaid on top.
fn graph_gutter(g: &RowGraph, width: usize) -> impl IntoElement {
    let w = width as f32 * LANE_W;
    let node_lane = g.lane.min(width.saturating_sub(1));
    let edges = g.edges.clone();

    let lines = canvas(
        |_bounds, _w, _cx| {},
        move |bounds, _, window, _cx| {
            let x = |col: usize| bounds.origin.x + px(col as f32 * LANE_W + LANE_W / 2.0);
            let top = bounds.origin.y;
            let cy = bounds.origin.y + px(ROW_H / 2.0);
            let bot = bounds.origin.y + px(ROW_H);
            for e in &edges {
                let mut pb = PathBuilder::stroke(px(1.6));
                match e.kind {
                    EdgeKind::Vertical => {
                        pb.move_to(point(x(e.col), top));
                        pb.line_to(point(x(e.col), bot));
                    }
                    EdgeKind::IntoNode => {
                        pb.move_to(point(x(e.col), top));
                        if e.col == node_lane {
                            pb.line_to(point(x(node_lane), cy));
                        } else {
                            pb.curve_to(point(x(node_lane), cy), point(x(e.col), cy));
                        }
                    }
                    EdgeKind::OutOfNode => {
                        pb.move_to(point(x(node_lane), cy));
                        if e.col == node_lane {
                            pb.line_to(point(x(node_lane), bot));
                        } else {
                            pb.curve_to(point(x(e.col), bot), point(x(e.col), cy));
                        }
                    }
                }
                if let Ok(path) = pb.build() {
                    window.paint_path(path, Hsla::from(branch_color(e.color)));
                }
            }
        },
    )
    .w(px(w))
    .h(px(ROW_H));

    div()
        .relative()
        .flex_none()
        .h(px(ROW_H))
        .w(px(w))
        .child(lines)
        .child(
            div()
                .absolute()
                .left(px(node_lane as f32 * LANE_W + LANE_W / 2.0 - DOT / 2.0))
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

/// When launched from Finder as a `.app`, the process inherits a minimal PATH
/// (typically just `/usr/bin:/bin:/usr/sbin:/sbin`), so Homebrew tools like `gh`
/// — and sometimes `git` — are not found. Prepend the common tool directories so
/// the CLI integrations work the same whether launched from a terminal or Finder.
fn ensure_tool_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = ["/opt/homebrew/bin", "/usr/local/bin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for p in current.split(':').filter(|p| !p.is_empty()) {
        if !parts.iter().any(|x| x == p) {
            parts.push(p.to_string());
        }
    }
    std::env::set_var("PATH", parts.join(":"));
}

fn main() {
    ensure_tool_path();
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
                        wt_diff: Vec::new(),
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
                        diff_ignore_ws: false,
                        diff_side_by_side: false,
                        diff_syntax: true,
                        diff_scroll: UniformListScrollHandle::new(),
                        diff_cursor: 0,
                        stashes: Vec::new(),
                        reflog: Vec::new(),
                        remotes: Vec::new(),
                        submodules: Vec::new(),
                        tags: Vec::new(),
                        conflicts: Vec::new(),
                        conflict_file: None,
                        conflict_sides: None,
                        console_input: String::new(),
                        console_focus: cx.focus_handle(),
                        console_output: String::new(),
                        more_open: false,
                        sign_off: false,
                        op_state: None,
                        commit_cursor: 0,
                        branch_cursor: 0,
                        filter_cursor: 0,
                        console_cursor: 0,
                        prs: Vec::new(),
                        prs_msg: None,
                        pr_view: None,
                        pr_compose_target: None,
                        pr_compose: String::new(),
                        pr_compose_focus: cx.focus_handle(),
                        pr_compose_cursor: 0,
                        focus_pr_compose: false,
                        aux: None,
                        search_term: String::new(),
                        search_focus: cx.focus_handle(),
                        search_cursor: 0,
                        search_regex: false,
                        graph_maintained: false,
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

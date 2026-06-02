//! diff — native window (GPUI): log graph + diff viewer.
//! M2: virtualized log + DAG.  M3: click a commit → diff below.
//!
//! Usage:  diff [repo-path] [limit]   (default: . and 50000)

use git_core::blame::BlameLine;
use git_core::diff::{FileDiff, LineOrigin};
use git_core::graph::{compute_graph, RowGraph};
use git_core::rebase::{RebaseAction, RebaseResult, RebaseStep};
use git_core::{BranchInfo, CommitInfo, StatusEntry};
use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, Entity,
    FocusHandle, KeyDownEvent, Rgba, SharedString, Window, WindowBounds, WindowOptions,
};

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
}

impl RebasedApp {
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

fn short(id: &str) -> String {
    id.get(..7).unwrap_or(id).to_string()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
    }
}

impl RebasedApp {
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
            .child(div().px_2().text_color(color::accent()).child("diff"))
            .child(tab("Log", ViewMode::Log))
            .child(tab("Changes", ViewMode::Changes))
            .child(tab("Branches", ViewMode::Branches))
            .child(div().flex_1().min_w_0().text_color(color::dim()).text_sm().px_2().whitespace_nowrap().text_ellipsis().child(if cur.is_empty() { String::new() } else { format!("⎇ {cur}") }))
            .child(btn("tb-fetch", "Fetch", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.run_remote("fetch", cx)); } }))
            .child(btn("tb-pull", "Pull", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.run_remote("pull", cx)); } }))
            .child(btn("tb-push", "Push", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.run_remote("push", cx)); } }))
            .child(btn("tb-stash", "Stash", { let e = e.clone(); move |app| { e.update(app, |t, cx| t.do_stash(cx)); } }))
            .into_any_element()
    }

    /// Log view: virtualized log graph + diff/blame panel.
    fn render_log(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        let log = match &self.error {
            Some(e) => div().flex_1().p_3().text_color(color::err()).child(e.clone()).into_any_element(),
            None => div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "commit-log",
                        self.commits.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, _c| {
                            range
                                .map(|ix| {
                                    commit_row(
                                        &this.commits[ix],
                                        &this.graph[ix],
                                        this.graph_width,
                                        ix,
                                        this.selected == Some(ix),
                                        entity.clone(),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .size_full(),
                )
                .into_any_element(),
        };
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
    entity: Entity<RebasedApp>,
) -> impl IntoElement {
    let short_id = c.id.get(..8).unwrap_or(&c.id).to_string();
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
            entity.update(app, |this, cx| this.select(ix, cx));
        });
    if selected {
        row = row.bg(color::sel());
    }
    row.child(graph_gutter(g, graph_width))
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
    let repo_path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    let (commits, graph, graph_width, error) = match git_core::gix_log(&repo_path, limit) {
        Ok(commits) => {
            let graph = compute_graph(&commits);
            let width = graph
                .iter()
                .map(|r| r.width())
                .max()
                .unwrap_or(1)
                .clamp(1, MAX_LANES);
            (commits, graph, width, None)
        }
        Err(e) => (Vec::new(), Vec::new(), 1, Some(format!("Could not open repo: {e}"))),
    };

    // Pre-warm the most recent commit's diff: leaves the gix repo cache
    // warm on the UI thread (subsequent clicks are instant) and shows
    // its diff by default, like Rebased does.
    let (selected, diff) = match commits.first() {
        Some(c) => match git_core::diff::commit_diff(&repo_path, &c.id) {
            Ok(d) => (Some(0usize), d),
            Err(_) => (None, Vec::new()),
        },
        None => (None, Vec::new()),
    };
    let diff_rows = build_diff_rows(&diff);

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(760.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |cx| RebasedApp {
                    repo_path,
                    commits,
                    graph,
                    graph_width,
                    error,
                    selected,
                    diff,
                    diff_rows,
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
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

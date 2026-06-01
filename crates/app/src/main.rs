//! rebased-rs — ventana nativa (GPUI): log graph + visor de diff.
//! M2: log virtualizado + DAG.  M3: clic en commit → diff abajo.
//!
//! Uso:  rebased-rs [ruta-al-repo] [limit]   (por defecto: . y 50000)

use git_core::diff::{FileDiff, LineOrigin};
use git_core::graph::{compute_graph, RowGraph};
use git_core::CommitInfo;
use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, Entity, Rgba,
    Window, WindowBounds, WindowOptions,
};

const ROW_H: f32 = 24.0;
const LANE_W: f32 = 14.0;
const DOT: f32 = 8.0;
const MAX_LANES: usize = 14;
const DIFF_LINE_BUDGET: usize = 4000;

/// Paleta estilo IntelliJ "New UI" (dark).
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
}

/// Colores de rama del graph (se ciclan por índice de lane).
fn branch_color(c: u32) -> Rgba {
    const P: [u32; 8] = [
        0x548af7, 0x59a869, 0xd9923a, 0xc9555f, 0x9876d6, 0x4aa3a3, 0xc2924a, 0xd06fb3,
    ];
    rgb(P[(c as usize) % P.len()])
}

struct RebasedApp {
    repo_path: String,
    commits: Vec<CommitInfo>,
    graph: Vec<RowGraph>,
    graph_width: usize,
    error: Option<String>,
    selected: Option<usize>,
    diff: Vec<FileDiff>,
    diff_error: Option<String>,
}

impl RebasedApp {
    /// Selecciona un commit y carga su diff (reabre el repo: ~1 ms).
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.selected == Some(ix) {
            return;
        }
        self.selected = Some(ix);
        let id = self.commits[ix].id.clone();
        match git_core::diff::commit_diff(&self.repo_path, &id) {
            Ok(files) => {
                self.diff = files;
                self.diff_error = None;
            }
            Err(e) => {
                self.diff.clear();
                self.diff_error = Some(e);
            }
        }
        cx.notify();
    }
}

impl Render for RebasedApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .h(px(38.0))
            .px_3()
            .bg(color::panel())
            .border_b_1()
            .border_color(color::line())
            .child(div().text_color(color::accent()).child("rebased-rs"))
            .child(div().text_color(color::dim()).text_sm().child(self.repo_path.clone()))
            .child(
                div()
                    .text_color(color::dim())
                    .text_sm()
                    .child(format!("· {} commits", self.commits.len())),
            );

        let entity = cx.entity();
        let log = match &self.error {
            Some(e) => div().flex_1().p_3().text_color(color::err()).child(e.clone()),
            None => div().flex_1().min_h_0().child(
                uniform_list(
                    "commit-log",
                    self.commits.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, _cx| {
                        range
                            .map(|ix| {
                                let selected = this.selected == Some(ix);
                                commit_row(
                                    &this.commits[ix],
                                    &this.graph[ix],
                                    this.graph_width,
                                    ix,
                                    selected,
                                    entity.clone(),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .size_full(),
            ),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(color::bg())
            .text_color(color::fg())
            .child(header)
            .child(log)
            .child(self.render_diff_panel())
    }
}

impl RebasedApp {
    /// Panel inferior con el diff del commit seleccionado.
    fn render_diff_panel(&self) -> impl IntoElement {
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
            }
            None => div()
                .w_full()
                .px_3()
                .py_1()
                .bg(color::panel())
                .text_sm()
                .text_color(color::dim())
                .child("Selecciona un commit para ver su diff"),
        };

        let content = if let Some(err) = &self.diff_error {
            div().p_3().text_color(color::err()).child(err.clone()).into_any_element()
        } else {
            let mut body = div()
                .id("diff-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .font_family("Menlo")
                .text_xs();
            let mut budget = DIFF_LINE_BUDGET;
            for f in &self.diff {
                let (add, del) = f.line_stats();
                body = body.child(
                    div()
                        .w_full()
                        .px_3()
                        .py_1()
                        .bg(color::panel())
                        .text_color(color::fg())
                        .child(format!("{}   +{add} −{del}{}", f.path, if f.binary { "  [binario]" } else { "" })),
                );
                for h in &f.hunks {
                    body = body.child(div().w_full().px_3().text_color(color::dim()).child(h.header.clone()));
                    for l in &h.lines {
                        if budget == 0 {
                            break;
                        }
                        budget -= 1;
                        let (fg, bg, sign) = match l.origin {
                            LineOrigin::Add => (color::add_fg(), color::add_bg(), "+"),
                            LineOrigin::Del => (color::del_fg(), color::del_bg(), "−"),
                            LineOrigin::Context => (color::fg(), color::bg(), " "),
                        };
                        body = body.child(
                            div()
                                .w_full()
                                .px_3()
                                .bg(bg)
                                .text_color(fg)
                                .child(format!("{sign} {}", l.content)),
                        );
                    }
                }
            }
            if budget == 0 {
                body = body.child(div().px_3().py_1().text_color(color::dim()).child("… diff truncado"));
            }
            body.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .h(px(340.0))
            .border_t_1()
            .border_color(color::line())
            .bg(color::bg())
            .child(head)
            .child(content)
    }
}

/// Una fila del log (clicable): gutter del graph · summary · autor · hash.
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
                .w(px(72.0))
                .whitespace_nowrap()
                .font_family("Menlo")
                .text_color(color::dim())
                .text_sm()
                .child(short_id),
        )
}

/// Gutter del graph: una línea vertical por lane activo + el punto del commit.
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
        Err(e) => (Vec::new(), Vec::new(), 1, Some(format!("No se pudo abrir el repo: {e}"))),
    };

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(760.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |_| RebasedApp {
                    repo_path,
                    commits,
                    graph,
                    graph_width,
                    error,
                    selected: None,
                    diff: Vec::new(),
                    diff_error: None,
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

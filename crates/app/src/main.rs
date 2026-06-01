//! rebased-rs — ventana nativa (GPUI) con el log graph leído del motor gix.
//! M2: log virtualizado (uniform_list) + render del DAG (swimlanes con color de rama).
//!
//! Uso:  rebased-rs [ruta-al-repo] [limit]   (por defecto: . y 50000)

use git_core::graph::{compute_graph, RowGraph};
use git_core::CommitInfo;
use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, Rgba, Window,
    WindowBounds, WindowOptions,
};

const ROW_H: f32 = 24.0;
const LANE_W: f32 = 14.0;
const DOT: f32 = 8.0;
const MAX_LANES: usize = 14;

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

        let body = match &self.error {
            Some(e) => div().flex_1().p_3().text_color(color::err()).child(e.clone()),
            // Lista virtualizada: solo se renderizan las filas visibles.
            None => div().flex_1().min_h_0().child(
                uniform_list(
                    "commit-log",
                    self.commits.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _window, _cx| {
                        range
                            .map(|ix| commit_row(&this.commits[ix], &this.graph[ix], this.graph_width))
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
            .child(body)
    }
}

/// Gutter del graph: una línea vertical por lane activo + el punto del commit.
fn graph_gutter(g: &RowGraph, width: usize) -> impl IntoElement {
    let mut gutter = div()
        .relative()
        .flex_none()
        .h(px(ROW_H))
        .w(px(width as f32 * LANE_W));

    // Líneas verticales de los lanes activos en esta fila.
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

    // Punto del commit, centrado verticalmente en su lane.
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

/// Una fila del log: gutter del graph · summary · autor · hash.
fn commit_row(c: &CommitInfo, g: &RowGraph, graph_width: usize) -> impl IntoElement {
    let short_id = c.id.get(..8).unwrap_or(&c.id).to_string();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(ROW_H))
        .px_2()
        .border_b_1()
        .border_color(color::row_line())
        .child(graph_gutter(g, graph_width))
        .child(div().flex_1().text_color(color::fg()).child(truncate(&c.summary, 90)))
        .child(div().flex_none().text_color(color::dim()).text_sm().child(c.author.clone()))
        .child(div().flex_none().w(px(64.0)).text_color(color::dim()).text_sm().child(short_id))
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
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
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
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

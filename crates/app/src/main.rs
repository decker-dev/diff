//! rebased-rs — ventana nativa (GPUI): log graph + visor de diff.
//! M2: log virtualizado + DAG.  M3: clic en commit → diff abajo.
//!
//! Uso:  rebased-rs [ruta-al-repo] [limit]   (por defecto: . y 50000)

use git_core::blame::BlameLine;
use git_core::diff::{FileDiff, LineOrigin};
use git_core::graph::{compute_graph, RowGraph};
use git_core::CommitInfo;
use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, Entity, Rgba,
    SharedString, Window, WindowBounds, WindowOptions,
};

const ROW_H: f32 = 24.0;
const LANE_W: f32 = 14.0;
const DOT: f32 = 8.0;
const MAX_LANES: usize = 14;
/// Altura de fila en los paneles de diff/blame (monospace, virtualizados).
const DIFF_ROW_H: f32 = 18.0;

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

/// Anotación de un archivo (modo blame del panel inferior).
struct BlameView {
    file: String,
    lines: Vec<BlameLine>,
    error: Option<String>,
    /// `true` mientras el blame se calcula en background (no congela la UI).
    loading: bool,
}

/// Fila plana del diff, para virtualizar con `uniform_list` (todas ~misma altura).
enum DiffRow {
    /// Cabecera de archivo (clicable → blame). `String` = ruta; el resto = etiqueta.
    File(String, String),
    Hunk(String),
    Line(LineOrigin, String),
}

/// Aplana el diff (archivos→hunks→líneas) en filas para la lista virtualizada.
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
    /// Diff aplanado para la lista virtualizada (deriva de `diff`).
    diff_rows: Vec<DiffRow>,
    diff_error: Option<String>,
    /// Si está `Some`, el panel inferior muestra blame en vez del diff.
    blame: Option<BlameView>,
}

impl RebasedApp {
    /// Selecciona un commit y carga su diff (reabre el repo: ~1 ms).
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.selected == Some(ix) {
            return;
        }
        self.selected = Some(ix);
        self.blame = None; // al cambiar de commit, volvemos a vista de diff
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

    /// Muestra el blame de `file` en el commit seleccionado. Calcula en background
    /// (blame puede tardar ~1s en historia profunda) para NO congelar la UI.
    fn show_blame(&mut self, file: String, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let id = self.commits[ix].id.clone();
        let repo_path = self.repo_path.clone();

        // Estado "cargando" inmediato (la UI sigue respondiendo).
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
                // Aplicar solo si seguimos esperando el blame de ESTE archivo.
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

    /// Vuelve del modo blame a la vista de diff.
    fn clear_blame(&mut self, cx: &mut Context<Self>) {
        self.blame = None;
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
            .child(self.render_diff_panel(cx))
    }
}

impl RebasedApp {
    /// Panel inferior: diff del commit, o blame de un archivo si está activo.
    /// Ambos listados están VIRTUALIZADOS (uniform_list) → render y scroll fluidos
    /// aunque el archivo tenga decenas de miles de líneas.
    fn render_diff_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let container = div()
            .flex()
            .flex_col()
            .w_full()
            .h(px(340.0))
            .border_t_1()
            .border_color(color::line())
            .bg(color::bg());

        // ---- Modo blame ----
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
                div().flex_1().p_3().text_color(color::dim()).child("Cargando blame…").into_any_element()
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

        // ---- Modo diff ----
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

        let body = if let Some(err) = &self.diff_error {
            div().flex_1().p_3().text_color(color::err()).child(err.clone()).into_any_element()
        } else if self.diff_rows.is_empty() {
            div().flex_1().p_3().text_color(color::dim()).child("Sin cambios en este commit").into_any_element()
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

/// Una fila del diff virtualizado: cabecera de archivo (clicable→blame), hunk, o línea ±.
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

/// Una fila de blame virtualizada: línea · commit · autor · texto.
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

    // Precalentar el diff del commit más reciente: deja el cache del repo gix
    // caliente en el hilo de UI (los clics siguientes son instantáneos) y muestra
    // ya su diff por defecto, como hace Rebased.
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
                cx.new(move |_| RebasedApp {
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
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

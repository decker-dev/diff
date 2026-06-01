//! rebased-rs — ventana nativa (GPUI) que renderiza el log real desde el motor gix.
//! M1: esqueleto de la app + prueba end-to-end de UI nativa ↔ motor.
//!
//! Uso:  rebased-rs [ruta-al-repo]   (por defecto: directorio actual)

use git_core::CommitInfo;
use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, Window, WindowBounds,
    WindowOptions,
};

/// Paleta estilo IntelliJ "New UI" (dark).
mod color {
    use gpui::{rgb, Rgba};
    pub fn bg() -> Rgba { rgb(0x1e1f22) }
    pub fn panel() -> Rgba { rgb(0x2b2d30) }
    pub fn line() -> Rgba { rgb(0x393b40) }
    pub fn fg() -> Rgba { rgb(0xbcbec4) }
    pub fn dim() -> Rgba { rgb(0x7a7e85) }
    pub fn accent() -> Rgba { rgb(0x548af7) }
    pub fn err() -> Rgba { rgb(0xff6b68) }
}

struct RebasedApp {
    repo_path: String,
    commits: Vec<CommitInfo>,
    error: Option<String>,
}

impl Render for RebasedApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(
                div()
                    .text_color(color::dim())
                    .text_sm()
                    .child(self.repo_path.clone()),
            )
            .child(
                div()
                    .text_color(color::dim())
                    .text_sm()
                    .child(format!("· {} commits", self.commits.len())),
            );

        let body = match &self.error {
            Some(e) => div().p_3().text_color(color::err()).child(e.clone()),
            None => div()
                .flex()
                .flex_col()
                .w_full()
                .children(self.commits.iter().take(80).map(commit_row)),
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

/// Una fila del log: hash · summary · autor (+ marca de merge).
fn commit_row(c: &CommitInfo) -> impl IntoElement {
    let short_id = c.id.get(..8).unwrap_or(&c.id).to_string();
    let is_merge = c.parents.len() > 1;

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .w_full()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(rgb(0x26282c))
        .child(div().w(px(64.0)).text_color(color::accent()).child(short_id))
        .child(
            div()
                .flex_1()
                .text_color(color::fg())
                .child(truncate(&c.summary, 90)),
        )
        .children(is_merge.then(|| {
            div()
                .text_xs()
                .text_color(color::dim())
                .child("merge")
        }))
        .child(div().text_color(color::dim()).text_sm().child(c.author.clone()))
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
    let (commits, error) = match git_core::gix_log(&repo_path, 200) {
        Ok(c) => (c, None),
        Err(e) => (Vec::new(), Some(format!("No se pudo abrir el repo: {e}"))),
    };

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |_| RebasedApp {
                    repo_path,
                    commits,
                    error,
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

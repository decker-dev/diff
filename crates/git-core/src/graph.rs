//! Cálculo del layout del DAG de commits ("swimlanes") para el log graph.
//!
//! Dada la lista de commits en orden topológico/fecha (nuevos primero), asigna
//! a cada uno una *lane* (columna) y un color de rama estable, y deja un
//! snapshot de los lanes activos en cada fila para dibujar las líneas verticales.
//! Es lógica pura y testeable, sin dependencia de la UI.

use crate::CommitInfo;

/// Layout de una fila del graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGraph {
    /// Columna (lane) donde se pinta el punto del commit.
    pub lane: usize,
    /// Color del punto (índice de paleta, sin acotar; el render hace `% paleta`).
    pub color: u32,
    /// Lanes activos al entrar en esta fila: `Some(color)` si está ocupado.
    /// Permite dibujar las líneas verticales del gutter y los merges.
    pub lanes: Vec<Option<u32>>,
}

impl RowGraph {
    /// Nº de lanes ocupados en esta fila (para dimensionar el gutter).
    pub fn width(&self) -> usize {
        self.lanes
            .iter()
            .rposition(|l| l.is_some())
            .map_or(0, |i| i + 1)
    }
}

/// Calcula el layout del graph para `commits` (orden: nuevos primero).
pub fn compute_graph(commits: &[CommitInfo]) -> Vec<RowGraph> {
    // Cada lane guarda el oid del commit que "espera" encontrar más abajo.
    let mut lanes: Vec<Option<&str>> = Vec::new();
    // Color asignado a cada lane (paralelo a `lanes`).
    let mut colors: Vec<u32> = Vec::new();
    let mut next_color: u32 = 0;
    let mut out = Vec::with_capacity(commits.len());

    for c in commits {
        // 1. Lane del nodo: el que ya esperaba a este commit (un hijo lo apuntó),
        //    o uno nuevo si es un tip sin hijos en la ventana cargada.
        let node_lane = match lanes.iter().position(|l| *l == Some(c.id.as_str())) {
            Some(idx) => idx,
            None => alloc_lane(&mut lanes, &mut colors, &mut next_color),
        };
        let node_color = colors[node_lane];

        // 2. Snapshot de los lanes activos en esta fila (líneas que la cruzan).
        let snapshot: Vec<Option<u32>> = lanes
            .iter()
            .enumerate()
            .map(|(i, l)| l.map(|_| colors[i]))
            .collect();

        // 3. Merges convergentes: otros lanes que esperaban a este commit se funden.
        for i in 0..lanes.len() {
            if i != node_lane && lanes[i] == Some(c.id.as_str()) {
                lanes[i] = None;
            }
        }

        // 4. El primer padre continúa por el lane del nodo; sin padres, el lane acaba.
        match c.parents.first() {
            Some(p) => lanes[node_lane] = Some(p.as_str()),
            None => lanes[node_lane] = None,
        }

        // 5. Padres extra (merge): abren lanes nuevos si no los espera ya nadie.
        for p in c.parents.iter().skip(1) {
            if !lanes.iter().any(|l| *l == Some(p.as_str())) {
                let idx = alloc_lane(&mut lanes, &mut colors, &mut next_color);
                lanes[idx] = Some(p.as_str());
            }
        }

        out.push(RowGraph {
            lane: node_lane,
            color: node_color,
            lanes: snapshot,
        });
    }
    out
}

/// Reserva el primer lane libre (o crea uno al final) y le da un color nuevo.
fn alloc_lane<'a>(
    lanes: &mut Vec<Option<&'a str>>,
    colors: &mut Vec<u32>,
    next_color: &mut u32,
) -> usize {
    let color = *next_color;
    *next_color = next_color.wrapping_add(1);
    match lanes.iter().position(|l| l.is_none()) {
        Some(idx) => {
            colors[idx] = color;
            idx
        }
        None => {
            lanes.push(None);
            colors.push(color);
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, parents: &[&str]) -> CommitInfo {
        CommitInfo {
            id: id.to_string(),
            summary: String::new(),
            author: String::new(),
            time: 0,
            parents: parents.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn historia_lineal_un_solo_lane() {
        let commits = [commit("A", &["B"]), commit("B", &["C"]), commit("C", &[])];
        let g = compute_graph(&commits);
        assert_eq!(g.len(), 3);
        for row in &g {
            assert_eq!(row.lane, 0, "toda la historia lineal va en el lane 0");
        }
        // Mismo color de rama en toda la línea.
        assert_eq!(g[0].color, g[1].color);
        assert_eq!(g[1].color, g[2].color);
    }

    #[test]
    fn merge_abre_y_cierra_segundo_lane() {
        // M es merge de A y B; ambos vienen de `base`.
        let commits = [
            commit("M", &["A", "B"]),
            commit("A", &["base"]),
            commit("B", &["base"]),
            commit("base", &[]),
        ];
        let g = compute_graph(&commits);
        assert_eq!(g[0].lane, 0, "el merge va en el lane 0");
        assert_eq!(g[1].lane, 0, "A continúa el lane 0 (primer padre)");
        assert_eq!(g[2].lane, 1, "B vive en el segundo lane abierto por el merge");
        assert_eq!(g[3].lane, 0, "base reconverge al lane 0");
        // En la fila de B hay dos lanes activos (el gutter mide 2).
        assert_eq!(g[2].width(), 2);
        // A y B tienen colores de rama distintos.
        assert_ne!(g[1].color, g[2].color);
    }
}

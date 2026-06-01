# Benchmarks

Repo de prueba: `../rebased` (IntelliJ + git integration), **505.988 commits**.
Máquina: macOS 26.5, Apple Silicon (arm64). Build `--release` (lto thin, opt 3).

## M0 — motor de `log` (elección de motor de git)

Operación: leer los N commits más recientes desde HEAD con autor, summary, fecha y padres.
gix ordena por fecha de commit (`ByCommitTime(NewestFirst)`); git2 en orden por defecto.

### log(1000)

| Motor | Tiempo | vs libgit2 | vs git CLI |
|---|---|---|---|
| libgit2 (`git2`) | 417 ms | 1x | ~20x más lento |
| gitoxide (`gix`) frío | 47 ms | 9x | ~2x más lento |
| **gitoxide (`gix`) caliente** | **17,4 ms** | **24x** | **iguala/supera git (20 ms)** |
| git CLI (referencia) | 20 ms | — | 1x |

### log(50.000)

| Motor | Tiempo | vs libgit2 | vs git CLI |
|---|---|---|---|
| libgit2 (`git2`) | 1,48 s | 1x | 5x más lento |
| **gitoxide (`gix`)** | **240 ms** (~208k c/s) | **6,2x** | **1,25x más RÁPIDO que git (300 ms)** |
| git CLI (referencia) | 300 ms | — | 1x |

**Conclusión:** Rust no acelera nada por sí solo — libgit2 era 18–150x más lento que
git. El motor correcto (**gitoxide**) iguala o supera al `git` CLI, y lo hace
**in-process** (sin lanzar subprocesos como hace la app JVM). El gate de 10x en
latencia de operación frente a Rebased (JVM) es holgadamente alcanzable.

### Hallazgos clave
- `Sort::TIME` en libgit2 cuesta ~8x sobre el orden por defecto.
- gix necesita `object_cache_size` configurado (el walk por fecha consulta cada commit 2x).
- No hay commit-graph en el repo; aun así gix iguala a git. Con commit-graph debería mejorar más.

## M1 — esqueleto GPUI (ventana nativa)

Ventana nativa (Metal) que carga el log con gix y lo renderiza. Build debug, ventana abierta.

| Métrica | rebased-rs (debug) | Rebased / IntelliJ (típico) | Factor |
|---|---|---|---|
| **RSS (RAM real)** | **85 MB** | ~700 MB – 1,5 GB | **>10x más ligero** |

- Compila y corre en stable 1.89; gpui 0.2.2 desde crates.io (no hace falta el repo de Zed).
- Requisito de sistema descubierto: macOS 26 / Xcode 26 necesita el *Metal Toolchain* aparte
  (`xcodebuild -downloadComponent MetalToolchain`, ~688 MB, una vez).
- Release debería reducir aún más la RAM. Pendiente medir contra una instancia real de Rebased.

## M3 — diff de un commit (lo que pasa al hacer clic)

Diff de un commit vs su padre en el repo de 505k commits (árboles de 200k+ archivos).

| Implementación | Tiempo / clic | Nota |
|---|---|---|
| libgit2 `diff_tree_to_tree` | ~460 ms | acceso a objetos lento (como en el log) |
| gix tree-diff (reabre repo) | ~83 ms | 5,5x mejor |
| **gix + repo cacheado caliente** | **2–12 ms** (95 ms el 1er clic) | in-process; **más rápido que git CLI** (20 ms) |
| git CLI `git diff-tree -p` (ref) | ~20 ms | paga arranque de proceso cada vez |

**Cómo:** gix hace el tree-diff (qué archivos cambian) y lee los blobs; el diff de
líneas se hace con `git2::Patch::from_buffers` sobre los bytes (en memoria, sin
object store). El repo gix se reutiliza por hilo con object-cache caliente → los
clics tras el primero son instantáneos. Mantener el estado caliente in-process es
exactamente la ventaja frente a lanzar `git` o a la app JVM.

## Pendiente
- **status**: con libgit2 tardó ~4,16 s (git CLI ~3,83 s) → es coste de *stat* del working
  tree (200k+ archivos), no del motor. El 10x aquí sale de **fsmonitor** (status incremental),
  no de cambiar de librería. Se aborda en M4.

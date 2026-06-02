//! Lightweight, dependency-free syntax highlighting for the diff viewer.
//!
//! This is NOT a full grammar: it's a single-line lexical tokenizer (keywords,
//! strings, comments, numbers, types, calls) tuned to make diffs readable —
//! the same "IDE feel" the heavyweight clients get from a bundled language
//! grammar, but with zero dependencies and a tiny footprint (the whole point
//! of `diff`). Because a diff shows lines out of context, multi-line constructs
//! (block comments spanning several lines, multi-line strings) are only
//! approximated per line; that's a deliberate trade-off for speed and size.

/// Token category. The UI maps each to a color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    /// Default foreground (identifiers, punctuation, whitespace).
    Text,
    Keyword,
    Type,
    Str,
    Comment,
    Number,
    /// An identifier immediately followed by `(` — a function/method call or def.
    Func,
}

/// Languages we tokenize. `Plain` disables highlighting (raw text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    JsTs,
    Python,
    Go,
    /// C / C++ / Java / Kotlin / C# / Swift / Scala (a shared keyword union).
    CLike,
    Json,
    Yaml,
    Toml,
    Css,
    Shell,
    Markdown,
    Plain,
}

/// Picks a language from a file path (by extension, then a few known names).
pub fn lang_for_path(path: &str) -> Lang {
    let name = path.rsplit('/').next().unwrap_or(path);
    // Extension-less names worth special-casing.
    match name {
        "Cargo.lock" => return Lang::Toml,
        "Dockerfile" | "Makefile" | ".bashrc" | ".zshrc" | ".profile" => return Lang::Shell,
        _ => {}
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Lang::Rust,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Lang::JsTs,
        "py" | "pyi" | "pyw" => Lang::Python,
        "go" => Lang::Go,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "java" | "kt" | "kts" | "cs"
        | "swift" | "scala" | "m" | "mm" | "rs_in" => Lang::CLike,
        "json" | "jsonc" | "json5" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "css" | "scss" | "less" | "sass" => Lang::Css,
        "sh" | "bash" | "zsh" | "fish" | "ksh" => Lang::Shell,
        "md" | "markdown" | "mdx" => Lang::Markdown,
        _ => Lang::Plain,
    }
}

/// Tokenizes one line into coalesced `(kind, text)` spans. The concatenation of
/// all span texts is exactly the input line (so monospace alignment is exact).
pub fn highlight(line: &str, lang: Lang) -> Vec<(Tok, String)> {
    if matches!(lang, Lang::Plain) {
        return vec![(Tok::Text, line.to_string())];
    }
    if matches!(lang, Lang::Markdown) {
        return highlight_md(line);
    }
    let spec = spec_for(lang);
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out: Vec<(Tok, String)> = Vec::new();
    let mut text = String::new(); // pending Text run (whitespace/punctuation/plain idents)
    let mut i = 0;

    while i < n {
        // Line comment → the rest of the line.
        if spec.line_comment.iter().any(|m| starts_with(&chars, i, m)) {
            push(&mut out, Tok::Text, std::mem::take(&mut text));
            push(&mut out, Tok::Comment, chars[i..].iter().collect());
            break;
        }
        // Block comment (best-effort within this single line).
        if let Some((open, close)) = spec.block {
            if starts_with(&chars, i, open) {
                push(&mut out, Tok::Text, std::mem::take(&mut text));
                let mut j = i + open.chars().count();
                let mut end = n;
                while j < n {
                    if starts_with(&chars, j, close) {
                        end = j + close.chars().count();
                        break;
                    }
                    j += 1;
                }
                push(&mut out, Tok::Comment, chars[i..end].iter().collect());
                i = end;
                continue;
            }
        }
        let c = chars[i];
        // String / char literal.
        if spec.quotes.contains(&c) {
            push(&mut out, Tok::Text, std::mem::take(&mut text));
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let end = j.min(n);
            push(&mut out, Tok::Str, chars[i..end].iter().collect());
            i = end;
            continue;
        }
        // Number literal.
        if c.is_ascii_digit() || (c == '.' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            push(&mut out, Tok::Text, std::mem::take(&mut text));
            let mut j = i + 1;
            while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_') {
                j += 1;
            }
            push(&mut out, Tok::Number, chars[i..j].iter().collect());
            i = j;
            continue;
        }
        // Identifier / keyword / type / call.
        if is_ident_start(c) {
            push(&mut out, Tok::Text, std::mem::take(&mut text));
            let mut j = i + 1;
            while j < n && is_ident_cont(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let kind = if spec.keywords.contains(&word.as_str()) {
                Tok::Keyword
            } else if spec.types.contains(&word.as_str()) {
                Tok::Type
            } else if spec.caps_type && word.chars().next().is_some_and(char::is_uppercase) {
                Tok::Type
            } else if spec.calls && next_nonspace(&chars, j) == Some('(') {
                Tok::Func
            } else {
                Tok::Text
            };
            push(&mut out, kind, word);
            i = j;
            continue;
        }
        // Anything else (whitespace, operators, punctuation): plain text.
        text.push(c);
        i += 1;
    }
    push(&mut out, Tok::Text, std::mem::take(&mut text));
    if out.is_empty() {
        out.push((Tok::Text, String::new()));
    }
    out
}

/// Minimal Markdown: headings, blockquotes, inline `code` spans.
fn highlight_md(line: &str) -> Vec<(Tok, String)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return vec![(Tok::Keyword, line.to_string())];
    }
    if trimmed.starts_with('>') {
        return vec![(Tok::Comment, line.to_string())];
    }
    let mut out: Vec<(Tok, String)> = Vec::new();
    let mut buf = String::new();
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            push(&mut out, if in_code { Tok::Str } else { Tok::Text }, std::mem::take(&mut buf));
            push(&mut out, Tok::Str, "`".to_string());
            in_code = !in_code;
        } else {
            buf.push(ch);
        }
    }
    push(&mut out, if in_code { Tok::Str } else { Tok::Text }, buf);
    if out.is_empty() {
        out.push((Tok::Text, String::new()));
    }
    out
}

/// Appends a span, coalescing with the previous one if it's the same kind.
fn push(out: &mut Vec<(Tok, String)>, kind: Tok, s: String) {
    if s.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.0 == kind {
            last.1.push_str(&s);
            return;
        }
    }
    out.push((kind, s));
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}
fn is_ident_cont(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// First non-space character at or after `i`.
fn next_nonspace(chars: &[char], i: usize) -> Option<char> {
    chars[i..].iter().copied().find(|c| !c.is_whitespace())
}

/// Does `chars[i..]` start with the ASCII pattern `pat`?
fn starts_with(chars: &[char], i: usize, pat: &str) -> bool {
    pat.chars().enumerate().all(|(k, pc)| chars.get(i + k) == Some(&pc))
}

/// Per-language lexer configuration.
struct Spec {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    /// Treat Capitalized identifiers as types (good for code, noisy for data).
    caps_type: bool,
    /// Color `ident(` as a function call.
    calls: bool,
}

fn spec_for(lang: Lang) -> Spec {
    match lang {
        Lang::Rust => Spec {
            line_comment: &["//"],
            block: Some(("/*", "*/")),
            quotes: &['"'], // not `'` — would clash with lifetimes
            keywords: &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "union", "unsafe", "use", "where", "while",
            ],
            types: &[
                "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
                "i32", "i64", "i128", "isize", "f32", "f64",
            ],
            caps_type: true,
            calls: true,
        },
        Lang::JsTs => Spec {
            line_comment: &["//"],
            block: Some(("/*", "*/")),
            quotes: &['"', '\'', '`'],
            keywords: &[
                "abstract", "any", "as", "async", "await", "break", "case", "catch", "class",
                "const", "continue", "debugger", "default", "delete", "do", "else", "enum",
                "export", "extends", "false", "finally", "for", "from", "function", "get", "if",
                "implements", "import", "in", "instanceof", "interface", "let", "new", "null",
                "of", "private", "protected", "public", "readonly", "return", "set", "static",
                "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined",
                "var", "void", "while", "yield",
            ],
            types: &["string", "number", "boolean", "any", "unknown", "never", "object", "symbol"],
            caps_type: true,
            calls: true,
        },
        Lang::Python => Spec {
            line_comment: &["#"],
            block: None,
            quotes: &['"', '\''],
            keywords: &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield", "match", "case", "self",
            ],
            types: &["int", "str", "float", "bool", "list", "dict", "set", "tuple", "bytes"],
            caps_type: true,
            calls: true,
        },
        Lang::Go => Spec {
            line_comment: &["//"],
            block: Some(("/*", "*/")),
            quotes: &['"', '`', '\''],
            keywords: &[
                "break", "case", "chan", "const", "continue", "default", "defer", "else",
                "fallthrough", "for", "func", "go", "goto", "if", "import", "interface", "map",
                "package", "range", "return", "select", "struct", "switch", "type", "var", "nil",
                "true", "false", "iota",
            ],
            types: &[
                "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32",
                "uint64", "uintptr", "float32", "float64", "complex64", "complex128", "string",
                "bool", "byte", "rune", "error",
            ],
            caps_type: true,
            calls: true,
        },
        Lang::CLike => Spec {
            line_comment: &["//"],
            block: Some(("/*", "*/")),
            quotes: &['"', '\''],
            keywords: &[
                "abstract", "auto", "boolean", "break", "case", "catch", "class", "const",
                "continue", "default", "delete", "do", "else", "enum", "extends", "extern",
                "false", "final", "finally", "for", "fun", "goto", "if", "implements", "import",
                "inline", "interface", "let", "namespace", "new", "null", "nullptr", "object",
                "operator", "override", "package", "private", "protected", "public", "return",
                "sizeof", "static", "struct", "super", "switch", "template", "this", "throw",
                "throws", "true", "try", "typedef", "typename", "union", "unsigned", "using", "val",
                "var", "virtual", "volatile", "while",
            ],
            types: &[
                "int", "long", "short", "char", "float", "double", "bool", "void", "signed",
                "size_t", "string",
            ],
            caps_type: true,
            calls: true,
        },
        Lang::Json => Spec {
            line_comment: &[],
            block: None,
            quotes: &['"'],
            keywords: &["true", "false", "null"],
            types: &[],
            caps_type: false,
            calls: false,
        },
        Lang::Yaml => Spec {
            line_comment: &["#"],
            block: None,
            quotes: &['"', '\''],
            keywords: &["true", "false", "null", "yes", "no", "on", "off"],
            types: &[],
            caps_type: false,
            calls: false,
        },
        Lang::Toml => Spec {
            line_comment: &["#"],
            block: None,
            quotes: &['"', '\''],
            keywords: &["true", "false"],
            types: &[],
            caps_type: false,
            calls: false,
        },
        Lang::Css => Spec {
            line_comment: &[],
            block: Some(("/*", "*/")),
            quotes: &['"', '\''],
            keywords: &[],
            types: &[],
            caps_type: false,
            calls: false,
        },
        Lang::Shell => Spec {
            line_comment: &["#"],
            block: None,
            quotes: &['"', '\''],
            keywords: &[
                "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case",
                "esac", "in", "function", "return", "local", "export", "readonly", "set", "unset",
                "source", "alias", "echo", "cd",
            ],
            types: &[],
            caps_type: false,
            calls: false,
        },
        Lang::Markdown | Lang::Plain => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cardinal invariant: spans concatenate back to the original line,
    /// so monospace columns never drift.
    fn assert_lossless(line: &str, lang: Lang) {
        let joined: String = highlight(line, lang).into_iter().map(|(_, s)| s).collect();
        assert_eq!(joined, line, "lossless for {lang:?}");
    }

    #[test]
    fn lossless_across_langs() {
        let samples = [
            (r#"    let x: u32 = foo("a\"b", 42); // tail"#, Lang::Rust),
            (r#"export const n: number = bar(`t`, 0x1F);"#, Lang::JsTs),
            ("def greet(name):  # hi\n", Lang::Python),
            ("func main() { return /* c */ 3 }", Lang::Go),
            (r#"{ "key": "val", "n": 1.5, "ok": true }"#, Lang::Json),
            ("# heading with `code` span", Lang::Markdown),
            ("plain text no lang", Lang::Plain),
            ("", Lang::Rust),
        ];
        for (line, lang) in samples {
            assert_lossless(line, lang);
        }
    }

    #[test]
    fn classifies_rust_tokens() {
        let toks = highlight("let n = 1;", Lang::Rust);
        assert!(toks.iter().any(|(k, s)| *k == Tok::Keyword && s == "let"));
        assert!(toks.iter().any(|(k, s)| *k == Tok::Number && s == "1"));
    }

    #[test]
    fn lifetime_is_not_a_string() {
        // `'a` would run to EOL if `'` were a quote char in Rust.
        let toks = highlight("fn f<'a>(x: &'a str) {}", Lang::Rust);
        assert!(!toks.iter().any(|(k, _)| *k == Tok::Str), "no string tokens: {toks:?}");
        assert!(toks.iter().any(|(k, s)| *k == Tok::Type && s == "str"));
    }

    #[test]
    fn detects_calls_and_comments() {
        let toks = highlight("foo(); // done", Lang::Rust);
        assert!(toks.iter().any(|(k, s)| *k == Tok::Func && s == "foo"));
        assert!(toks.iter().any(|(k, s)| *k == Tok::Comment && s.contains("done")));
    }

    #[test]
    fn lang_from_path() {
        assert_eq!(lang_for_path("src/main.rs"), Lang::Rust);
        assert_eq!(lang_for_path("a/b/App.tsx"), Lang::JsTs);
        assert_eq!(lang_for_path("Cargo.lock"), Lang::Toml);
        assert_eq!(lang_for_path("README"), Lang::Plain);
    }
}

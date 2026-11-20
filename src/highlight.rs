//! A small syntax highlighter for fenced code blocks (no dependencies): comments, strings, numbers,
//! keywords, function calls and Capitalised names, for the common languages.

use eframe::egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontId,
};

const KEYWORD: Color32 = Color32::from_rgb(255, 123, 114);
const STRING: Color32 = Color32::from_rgb(165, 214, 255);
const COMMENT: Color32 = Color32::from_rgb(139, 148, 158);
const NUMBER: Color32 = Color32::from_rgb(121, 192, 255);
const FUNCTION: Color32 = Color32::from_rgb(210, 168, 255);
const TYPE: Color32 = Color32::from_rgb(255, 166, 87);
const TAG: Color32 = Color32::from_rgb(126, 231, 135);

struct Spec {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    keywords: &'static [&'static str],
    ignore_case: bool,
    markup: bool,
}

const RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
];
const JS: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do",
    "else", "enum", "export", "extends", "false", "finally", "for", "function", "if", "implements", "import", "in",
    "instanceof", "interface", "let", "new", "null", "of", "private", "protected", "public", "readonly", "return",
    "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined", "var", "void", "while",
    "with", "yield",
];
const PY: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or",
    "pass", "raise", "return", "self", "True", "try", "while", "with", "yield",
];
const CLIKE: &[&str] = &[
    "abstract", "auto", "bool", "boolean", "break", "byte", "case", "catch", "chan", "char", "class", "const",
    "continue", "default", "defer", "delete", "do", "double", "else", "enum", "explicit", "extends", "extern",
    "false", "final", "float", "for", "fun", "func", "go", "goto", "if", "implements", "import", "inline", "int",
    "interface", "internal", "long", "map", "namespace", "new", "nil", "null", "nullptr", "object", "operator",
    "override", "package", "private", "protected", "public", "range", "record", "return", "sealed", "short",
    "signed", "sizeof", "static", "string", "struct", "switch", "template", "this", "throw", "throws", "true", "try",
    "typedef", "union", "unsigned", "using", "val", "var", "virtual", "void", "volatile", "while",
];
const SHELL: &[&str] = &[
    "case", "cd", "do", "done", "echo", "elif", "else", "esac", "exit", "export", "false", "fi", "for", "function",
    "if", "in", "local", "return", "source", "then", "true", "while",
];
const SQL: &[&str] = &[
    "add", "all", "alter", "and", "as", "by", "case", "create", "default", "delete", "distinct", "drop", "else",
    "end", "foreign", "from", "group", "having", "index", "inner", "insert", "into", "join", "key", "left", "limit",
    "not", "null", "offset", "on", "or", "order", "outer", "primary", "references", "right", "select", "set",
    "table", "then", "union", "unique", "update", "values", "when", "where",
];
const LITERALS: &[&str] = &["true", "false", "null", "yes", "no", "on", "off"];

fn spec_for(lang: &str) -> Option<Spec> {
    let slash = &["//"][..];
    let hash = &["#"][..];
    let c_block = Some(("/*", "*/"));
    Some(match lang {
        "rust" | "rs" => Spec { line: slash, block: c_block, quotes: &['"'], keywords: RUST, ignore_case: false, markup: false },
        "js" | "jsx" | "javascript" | "ts" | "tsx" | "typescript" | "mjs" | "cjs" => {
            Spec { line: slash, block: c_block, quotes: &['"', '\'', '`'], keywords: JS, ignore_case: false, markup: false }
        }
        "py" | "python" | "python3" => {
            Spec { line: hash, block: None, quotes: &['"', '\''], keywords: PY, ignore_case: false, markup: false }
        }
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "cs" | "csharp" | "java" | "go" | "golang" | "kotlin" | "kt"
        | "swift" | "php" | "dart" | "scala" => {
            Spec { line: slash, block: c_block, quotes: &['"', '\''], keywords: CLIKE, ignore_case: false, markup: false }
        }
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "ps1" | "powershell" | "bat" => {
            Spec { line: hash, block: None, quotes: &['"', '\'', '`'], keywords: SHELL, ignore_case: false, markup: false }
        }
        "sql" => Spec { line: &["--"], block: c_block, quotes: &['"', '\''], keywords: SQL, ignore_case: true, markup: false },
        "json" | "jsonc" | "json5" => {
            Spec { line: slash, block: c_block, quotes: &['"'], keywords: LITERALS, ignore_case: false, markup: false }
        }
        "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "properties" => {
            Spec { line: hash, block: None, quotes: &['"', '\''], keywords: LITERALS, ignore_case: false, markup: false }
        }
        "css" | "scss" | "less" => Spec { line: &[], block: c_block, quotes: &['"', '\''], keywords: &[], ignore_case: false, markup: false },
        "html" | "xml" | "svg" | "xhtml" | "htm" => {
            Spec { line: &[], block: None, quotes: &[], keywords: &[], ignore_case: false, markup: true }
        }
        _ => return None,
    })
}

fn push(job: &mut LayoutJob, font: &FontId, text: &str, color: Color32) {
    if !text.is_empty() {
        job.append(text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
    }
}

/// The code as a coloured, non-wrapping text layout. Unknown languages (and huge blocks) stay plain.
pub fn layout(lang: &str, code: &str, size: f32, base: Color32) -> LayoutJob {
    let font = FontId::monospace(size);
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;

    let spec = match spec_for(&lang.to_ascii_lowercase()) {
        Some(spec) if code.len() <= 60_000 => spec,
        _ => {
            push(&mut job, &font, code, base);
            return job;
        }
    };

    let mut i = 0;
    let mut plain_from = 0;
    while i < code.len() {
        let rest = &code[i..];
        let c = rest.chars().next().unwrap_or(' ');
        // A token is (length in bytes, colour); `None` means "ordinary text, keep going".
        let token: Option<(usize, Color32)> = if spec.markup && rest.starts_with("<!--") {
            Some((rest.find("-->").map(|e| e + 3).unwrap_or(rest.len()), COMMENT))
        } else if spec.markup && c == '<' {
            // A whole tag; quoted attribute values are drawn as strings below.
            Some((tag_len(rest), TAG))
        } else if spec.block.map_or(false, |(open, _)| rest.starts_with(open)) {
            let (open, close) = spec.block.unwrap();
            Some((rest[open.len()..].find(close).map(|e| open.len() + e + close.len()).unwrap_or(rest.len()), COMMENT))
        } else if spec.line.iter().any(|p| rest.starts_with(p)) {
            Some((rest.find('\n').unwrap_or(rest.len()), COMMENT))
        } else if spec.quotes.contains(&c) {
            Some((string_len(rest, c), STRING))
        } else if c.is_ascii_digit() {
            let len = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '.' || ch == '_'))
                .unwrap_or(rest.len());
            Some((len, NUMBER))
        } else if c.is_alphabetic() || c == '_' {
            let len = rest.find(|ch: char| !(ch.is_alphanumeric() || ch == '_')).unwrap_or(rest.len());
            let word = &rest[..len];
            let is_keyword = if spec.ignore_case {
                spec.keywords.iter().any(|k| k.eq_ignore_ascii_case(word))
            } else {
                spec.keywords.iter().any(|k| *k == word)
            };
            let color = if is_keyword {
                Some(KEYWORD)
            } else if rest[len..].trim_start_matches([' ', '\t']).starts_with('(') {
                Some(FUNCTION)
            } else if c.is_uppercase() && word.len() > 1 {
                Some(TYPE)
            } else {
                None
            };
            match color {
                Some(color) => Some((len, color)),
                None => {
                    i += len; // plain word
                    continue;
                }
            }
        } else {
            None
        };

        match token {
            Some((len, color)) => {
                push(&mut job, &font, &code[plain_from..i], base);
                let text = &code[i..i + len];
                if color == TAG {
                    push_tag(&mut job, &font, text);
                } else {
                    push(&mut job, &font, text, color);
                }
                i += len.max(1);
                plain_from = i;
            }
            None => i += c.len_utf8(),
        }
    }
    push(&mut job, &font, &code[plain_from.min(code.len())..], base);
    job
}

/// Bytes of the quoted string starting at `rest` (including both quotes).
fn string_len(rest: &str, quote: char) -> usize {
    let mut escaped = false;
    for (j, ch) in rest.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return j + ch.len_utf8();
        } else if ch == '\n' && quote != '`' {
            return j; // an unterminated string ends at the line end
        }
    }
    rest.len()
}

/// Bytes of the tag starting at `rest` ("<a href="x">").
fn tag_len(rest: &str) -> usize {
    let mut quote: Option<char> = None;
    for (j, ch) in rest.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '>' => return j + 1,
            None => {}
        }
    }
    rest.len()
}

fn push_tag(job: &mut LayoutJob, font: &FontId, tag: &str) {
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (j, ch) in tag.char_indices() {
        match quote {
            Some(q) if ch == q => {
                push(job, font, &tag[start..j + 1], STRING);
                start = j + 1;
                quote = None;
            }
            Some(_) => {}
            None if ch == '"' || ch == '\'' => {
                push(job, font, &tag[start..j], TAG);
                start = j;
                quote = Some(ch);
            }
            None => {}
        }
    }
    push(job, font, &tag[start..], if quote.is_some() { STRING } else { TAG });
}

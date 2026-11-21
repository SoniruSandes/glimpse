//! Syntax highlighter (no dependencies) for code files and Markdown code blocks.
//!
//! `tokens` turns text into classified byte ranges; the colours live in `color`. Every language gets
//! visibly different colours for: declaration keywords, control flow, types, built-in types, function
//! names / calls, macros, attributes / decorators, constants, numbers, strings (and their escapes),
//! comments (and doc comments). Config / markup languages have their own rules: JSON / YAML / TOML keys,
//! CSS selectors / properties / values, HTML tags / attributes (with embedded <script> and <style>),
//! shell variables and options, diffs and Markdown source.

use std::path::Path;

use eframe::egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontId,
};

// ------------------------------------------------------------------------------------------
// Classes and colours (One Dark-like, tuned for the dark panel)
// ------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Plain,
    Comment,
    DocComment,
    /// Declarations / storage: fn, let, class, struct, import ...
    Keyword,
    /// Control flow: if, else, for, return, match, try ...
    Control,
    /// User types (Capitalised names).
    Type,
    /// Built-in types: i32, int, string ...
    BuiltinType,
    Function,
    Macro,
    /// Attributes, decorators, annotations.
    Attribute,
    Number,
    /// true / false / null, ALL_CAPS, this / self.
    Constant,
    String,
    Escape,
    Operator,
    /// Object keys, struct fields, CSS properties.
    Property,
    Tag,
    /// HTML attribute names, command-line options.
    Attr,
    Variable,
    Label,
    Added,
    Removed,
    Heading,
    Link,
}

pub fn color(class: Class, base: Color32) -> Color32 {
    match class {
        Class::Plain => base,
        Class::Comment => Color32::from_rgb(110, 118, 129),
        Class::DocComment => Color32::from_rgb(122, 158, 110),
        Class::Keyword => Color32::from_rgb(255, 123, 114),
        Class::Control => Color32::from_rgb(198, 120, 221),
        Class::Type => Color32::from_rgb(229, 192, 123),
        Class::BuiltinType => Color32::from_rgb(78, 201, 176),
        Class::Function => Color32::from_rgb(97, 175, 239),
        Class::Macro => Color32::from_rgb(235, 120, 190),
        Class::Attribute => Color32::from_rgb(235, 120, 190),
        Class::Number => Color32::from_rgb(209, 154, 102),
        Class::Constant => Color32::from_rgb(209, 154, 102),
        Class::String => Color32::from_rgb(152, 195, 121),
        Class::Escape => Color32::from_rgb(86, 182, 194),
        Class::Operator => Color32::from_rgb(150, 162, 178),
        Class::Property => Color32::from_rgb(126, 214, 223),
        Class::Tag => Color32::from_rgb(224, 108, 117),
        Class::Attr => Color32::from_rgb(209, 154, 102),
        Class::Variable => Color32::from_rgb(240, 200, 130),
        Class::Label => Color32::from_rgb(86, 182, 194),
        Class::Added => Color32::from_rgb(126, 231, 135),
        Class::Removed => Color32::from_rgb(255, 123, 114),
        Class::Heading => Color32::from_rgb(97, 175, 239),
        Class::Link => Color32::from_rgb(86, 156, 245),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub start: usize,
    pub end: usize,
    pub class: Class,
}

fn emit(out: &mut Vec<Tok>, start: usize, end: usize, class: Class) {
    if end <= start || class == Class::Plain {
        return;
    }
    if let Some(last) = out.last_mut() {
        // Neighbouring pieces of the same class become one (operators, comment lines, diff lines).
        if last.class == class && last.end == start && matches!(class, Class::Operator | Class::Comment | Class::Added | Class::Removed) {
            last.end = end;
            return;
        }
    }
    out.push(Tok { start, end, class });
}

// ------------------------------------------------------------------------------------------
// Language table
// ------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Code,
    Json,
    Yaml,
    Toml,
    Css,
    Markup,
    Diff,
    Markdown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dollar {
    None,
    /// `$name` is a variable (shell, PHP, PowerShell).
    Var,
    /// `$` is a letter (JavaScript).
    Ident,
}

#[derive(Clone, Copy)]
struct Spec {
    id: &'static str,
    family: Family,
    line: &'static [&'static str],
    doc_line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    nested: bool,
    quotes: &'static [char],
    /// Quote characters that make identifiers (SQL "name", `name`).
    ident_quotes: &'static [char],
    triple: bool,
    rust_strings: bool,
    keywords: &'static [&'static str],
    control: &'static [&'static str],
    constants: &'static [&'static str],
    types: &'static [&'static str],
    ignore_case: bool,
    bang_macro: bool,
    hash_attr: bool,
    at_attr: bool,
    preproc: bool,
    dollar: Dollar,
    percent_vars: bool,
    shell: bool,
    capital_types: bool,
    upper_const: bool,
    props: bool,
    ops: bool,
    hyphen_idents: bool,
}

const BASE: Spec = Spec {
    id: "",
    family: Family::Code,
    line: &[],
    doc_line: &[],
    block: None,
    nested: false,
    quotes: &['"', '\''],
    ident_quotes: &[],
    triple: false,
    rust_strings: false,
    keywords: &[],
    control: &[],
    constants: &[],
    types: &[],
    ignore_case: false,
    bang_macro: false,
    hash_attr: false,
    at_attr: false,
    preproc: false,
    dollar: Dollar::None,
    percent_vars: false,
    shell: false,
    capital_types: true,
    upper_const: true,
    props: true,
    ops: true,
    hyphen_idents: false,
};

const SLASH: &[&str] = &["//"];
const HASH: &[&str] = &["#"];
const C_BLOCK: Option<(&str, &str)> = Some(("/*", "*/"));

fn spec(id: &str) -> Option<Spec> {
    let s = match id {
        "rust" => Spec {
            id: "rust",
            line: SLASH,
            doc_line: &["///", "//!"],
            block: C_BLOCK,
            nested: true,
            rust_strings: true,
            bang_macro: true,
            hash_attr: true,
            keywords: &[
                "as", "async", "const", "crate", "dyn", "enum", "extern", "fn", "impl", "let", "mod", "move", "mut", "pub",
                "ref", "static", "struct", "super", "trait", "type", "unsafe", "use", "where", "macro_rules", "union",
            ],
            control: &["await", "break", "continue", "else", "for", "if", "in", "loop", "match", "return", "while", "yield"],
            constants: &["true", "false", "self", "Self"],
            types: &[
                "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32", "f64", "bool",
                "char", "str",
            ],
            ..BASE
        },
        "js" => Spec {
            id: "js",
            line: SLASH,
            block: C_BLOCK,
            quotes: &['"', '\'', '`'],
            dollar: Dollar::Ident,
            at_attr: true,
            keywords: &[
                "abstract", "async", "class", "const", "constructor", "declare", "enum", "export", "extends", "from",
                "function", "get", "implements", "import", "interface", "let", "module", "namespace", "new", "private",
                "protected", "public", "readonly", "set", "static", "type", "var", "as", "delete", "typeof", "instanceof",
                "in", "of", "keyof", "satisfies", "is", "override", "accessor",
            ],
            control: &[
                "await", "break", "case", "catch", "continue", "debugger", "default", "do", "else", "finally", "for", "if",
                "return", "switch", "throw", "try", "while", "with", "yield",
            ],
            constants: &["true", "false", "null", "undefined", "NaN", "Infinity", "this", "super", "arguments"],
            types: &["string", "number", "boolean", "any", "unknown", "never", "object", "symbol", "bigint", "void"],
            ..BASE
        },
        "python" => Spec {
            id: "python",
            line: HASH,
            triple: true,
            at_attr: true,
            keywords: &[
                "as", "assert", "async", "class", "def", "del", "from", "global", "import", "lambda", "nonlocal", "pass",
                "with", "type",
            ],
            control: &[
                "and", "await", "break", "case", "continue", "elif", "else", "except", "finally", "for", "if", "in", "is",
                "match", "not", "or", "raise", "return", "try", "while", "yield",
            ],
            constants: &["True", "False", "None", "self", "cls"],
            types: &[
                "int", "float", "str", "bool", "list", "dict", "set", "tuple", "bytes", "bytearray", "object", "complex",
                "frozenset", "range",
            ],
            ..BASE
        },
        "c" | "cpp" => Spec {
            id: "c",
            line: SLASH,
            doc_line: &["///", "//!"],
            block: C_BLOCK,
            preproc: true,
            keywords: &[
                "alignas", "alignof", "auto", "class", "const", "constexpr", "consteval", "constinit", "delete", "enum",
                "explicit", "extern", "final", "friend", "inline", "mutable", "namespace", "new", "noexcept", "operator",
                "override", "private", "protected", "public", "register", "sizeof", "static", "struct", "template",
                "typedef", "typeid", "typename", "union", "using", "virtual", "volatile", "concept", "requires",
            ],
            control: &[
                "break", "case", "catch", "continue", "default", "do", "else", "for", "goto", "if", "return", "switch",
                "throw", "try", "while", "co_await", "co_return", "co_yield",
            ],
            constants: &["true", "false", "NULL", "nullptr", "this"],
            types: &[
                "bool", "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void", "size_t", "ssize_t",
                "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "wchar_t",
                "char8_t", "char16_t", "char32_t", "ptrdiff_t", "intptr_t", "uintptr_t",
            ],
            ..BASE
        },
        "csharp" => Spec {
            id: "csharp",
            line: SLASH,
            doc_line: &["///"],
            block: C_BLOCK,
            preproc: true,
            keywords: &[
                "abstract", "as", "base", "class", "const", "delegate", "enum", "event", "explicit", "extern", "get", "in",
                "init", "interface", "internal", "is", "namespace", "new", "operator", "out", "override", "params", "partial",
                "private", "protected", "public", "readonly", "record", "ref", "required", "sealed", "set", "static",
                "struct", "this", "typeof", "using", "var", "virtual", "volatile", "nameof", "sizeof", "async", "unsafe",
            ],
            control: &[
                "await", "break", "case", "catch", "continue", "default", "do", "else", "finally", "for", "foreach", "goto",
                "if", "lock", "return", "switch", "throw", "try", "when", "while", "yield",
            ],
            constants: &["true", "false", "null"],
            types: &[
                "bool", "byte", "sbyte", "char", "decimal", "double", "float", "int", "uint", "long", "ulong", "short",
                "ushort", "object", "string", "void", "dynamic", "nint", "nuint",
            ],
            ..BASE
        },
        "java" => Spec {
            id: "java",
            line: SLASH,
            block: C_BLOCK,
            at_attr: true,
            keywords: &[
                "abstract", "class", "enum", "extends", "final", "implements", "import", "instanceof", "interface", "native",
                "new", "package", "permits", "private", "protected", "public", "record", "sealed", "static", "strictfp",
                "super", "synchronized", "this", "throws", "transient", "var", "volatile",
            ],
            control: &[
                "assert", "break", "case", "catch", "continue", "default", "do", "else", "finally", "for", "if", "return",
                "switch", "throw", "try", "while", "yield",
            ],
            constants: &["true", "false", "null"],
            types: &["boolean", "byte", "char", "double", "float", "int", "long", "short", "void"],
            ..BASE
        },
        "kotlin" => Spec {
            id: "kotlin",
            line: SLASH,
            block: C_BLOCK,
            nested: true,
            at_attr: true,
            keywords: &[
                "abstract", "annotation", "as", "by", "class", "companion", "const", "constructor", "crossinline", "data",
                "enum", "external", "final", "fun", "import", "in", "infix", "init", "inline", "inner", "interface",
                "internal", "is", "lateinit", "noinline", "object", "open", "operator", "out", "override", "package",
                "private", "protected", "public", "reified", "sealed", "suspend", "tailrec", "typealias", "val", "var",
                "vararg", "where",
            ],
            control: &["break", "catch", "continue", "do", "else", "finally", "for", "if", "return", "throw", "try", "when", "while"],
            constants: &["true", "false", "null", "this", "super", "it"],
            ..BASE
        },
        "go" => Spec {
            id: "go",
            line: SLASH,
            block: C_BLOCK,
            quotes: &['"', '\'', '`'],
            keywords: &["chan", "const", "func", "import", "interface", "map", "package", "struct", "type", "var"],
            control: &[
                "break", "case", "continue", "default", "defer", "else", "fallthrough", "for", "go", "goto", "if", "range",
                "return", "select", "switch",
            ],
            constants: &["true", "false", "nil", "iota"],
            types: &[
                "bool", "byte", "complex64", "complex128", "error", "float32", "float64", "int", "int8", "int16", "int32",
                "int64", "rune", "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "any",
            ],
            ..BASE
        },
        "swift" => Spec {
            id: "swift",
            line: SLASH,
            doc_line: &["///"],
            block: C_BLOCK,
            nested: true,
            at_attr: true,
            keywords: &[
                "actor", "associatedtype", "class", "deinit", "enum", "extension", "fileprivate", "func", "import", "init",
                "inout", "internal", "let", "open", "operator", "private", "protocol", "public", "static", "struct",
                "subscript", "typealias", "var", "async", "lazy", "weak", "unowned", "override", "final", "mutating",
                "where", "is", "as", "in",
            ],
            control: &[
                "await", "break", "case", "catch", "continue", "default", "defer", "do", "else", "fallthrough", "for",
                "guard", "if", "repeat", "return", "switch", "throw", "throws", "try", "while",
            ],
            constants: &["true", "false", "nil", "self", "super"],
            types: &["Int", "Double", "Float", "Bool", "String", "Character", "Any", "Void"],
            ..BASE
        },
        "php" => Spec {
            id: "php",
            line: &["//", "#"],
            block: C_BLOCK,
            dollar: Dollar::Var,
            ignore_case: true,
            keywords: &[
                "abstract", "class", "const", "extends", "final", "function", "global", "implements", "include",
                "include_once", "interface", "namespace", "new", "private", "protected", "public", "require",
                "require_once", "static", "trait", "use", "var", "echo", "print", "readonly", "enum", "fn", "match",
            ],
            control: &[
                "break", "case", "catch", "continue", "default", "do", "else", "elseif", "finally", "for", "foreach", "if",
                "return", "switch", "throw", "try", "while", "yield", "and", "or", "xor", "instanceof",
            ],
            constants: &["true", "false", "null", "self", "parent"],
            types: &["int", "float", "string", "bool", "array", "object", "mixed", "void", "callable", "iterable"],
            ..BASE
        },
        "ruby" => Spec {
            id: "ruby",
            line: HASH,
            keywords: &[
                "alias", "class", "def", "end", "module", "include", "extend", "require", "require_relative", "attr_reader",
                "attr_writer", "attr_accessor", "private", "protected", "public", "super", "undef", "lambda", "proc", "begin",
                "do", "puts",
            ],
            control: &[
                "and", "break", "case", "else", "elsif", "ensure", "for", "if", "in", "next", "not", "or", "raise", "redo",
                "rescue", "retry", "return", "then", "unless", "until", "when", "while", "yield",
            ],
            constants: &["true", "false", "nil", "self"],
            ..BASE
        },
        "lua" => Spec {
            id: "lua",
            line: &["--"],
            block: Some(("--[[", "]]")),
            keywords: &["function", "local", "end", "and", "or", "not", "in"],
            control: &["break", "do", "else", "elseif", "for", "goto", "if", "repeat", "return", "then", "until", "while"],
            constants: &["true", "false", "nil", "self"],
            ..BASE
        },
        "shell" => Spec {
            id: "shell",
            line: HASH,
            quotes: &['"', '\'', '`'],
            dollar: Dollar::Var,
            shell: true,
            ops: false,
            props: false,
            capital_types: false,
            upper_const: false,
            keywords: &["alias", "declare", "export", "function", "local", "readonly", "source", "unset", "set", "typeset"],
            control: &[
                "case", "do", "done", "elif", "else", "esac", "exit", "fi", "for", "if", "in", "return", "select", "then",
                "until", "while", "break", "continue",
            ],
            constants: &["true", "false"],
            ..BASE
        },
        "powershell" => Spec {
            id: "powershell",
            line: HASH,
            block: Some(("<#", "#>")),
            dollar: Dollar::Var,
            ignore_case: true,
            hyphen_idents: true,
            ops: false,
            capital_types: false,
            upper_const: false,
            keywords: &["function", "filter", "param", "begin", "process", "end", "class", "enum", "using", "workflow", "in"],
            control: &[
                "break", "catch", "continue", "do", "else", "elseif", "finally", "for", "foreach", "if", "return", "switch",
                "throw", "trap", "try", "until", "while",
            ],
            constants: &["true", "false", "null"],
            ..BASE
        },
        "batch" => Spec {
            id: "batch",
            line: &["::"],
            quotes: &['"'],
            percent_vars: true,
            ignore_case: true,
            ops: false,
            props: false,
            capital_types: false,
            upper_const: false,
            keywords: &[
                "set", "echo", "setlocal", "endlocal", "call", "start", "cd", "md", "mkdir", "rd", "rmdir", "del", "copy",
                "move", "xcopy", "robocopy", "type", "title", "pause", "cls", "color", "pushd", "popd",
                "enabledelayedexpansion",
            ],
            control: &[
                "if", "else", "for", "in", "do", "goto", "exit", "not", "exist", "defined", "errorlevel", "equ", "neq",
                "lss", "gtr", "leq", "geq",
            ],
            constants: &["on", "off", "nul"],
            ..BASE
        },
        "sql" => Spec {
            id: "sql",
            line: &["--"],
            block: C_BLOCK,
            quotes: &['\''],
            ident_quotes: &['"', '`'],
            ignore_case: true,
            capital_types: false,
            upper_const: false,
            props: false,
            keywords: &[
                "add", "all", "alter", "as", "by", "check", "column", "constraint", "create", "database", "default", "delete",
                "desc", "asc", "distinct", "drop", "foreign", "from", "full", "group", "having", "index", "inner", "insert",
                "into", "join", "key", "left", "limit", "offset", "on", "order", "outer", "primary", "references", "returning",
                "right", "select", "set", "table", "union", "unique", "update", "using", "values", "view", "where", "with",
                "cross", "truncate", "begin", "commit", "rollback", "transaction", "trigger", "procedure", "function",
                "replace", "if", "over", "partition",
            ],
            control: &["and", "between", "case", "else", "end", "exists", "in", "is", "like", "not", "or", "then", "when", "ilike"],
            constants: &["null", "true", "false"],
            types: &[
                "int", "integer", "bigint", "smallint", "tinyint", "varchar", "char", "text", "boolean", "bool", "date",
                "time", "timestamp", "datetime", "decimal", "numeric", "float", "double", "real", "serial", "uuid", "json",
                "jsonb", "blob", "bytea",
            ],
            ..BASE
        },
        "docker" => Spec {
            id: "docker",
            line: HASH,
            dollar: Dollar::Var,
            ops: false,
            props: false,
            capital_types: false,
            upper_const: false,
            ignore_case: true,
            keywords: &[
                "from", "run", "cmd", "label", "maintainer", "expose", "env", "add", "copy", "entrypoint", "volume", "user",
                "workdir", "arg", "onbuild", "stopsignal", "healthcheck", "shell",
            ],
            control: &["as"],
            ..BASE
        },
        "make" => Spec {
            id: "make",
            line: HASH,
            dollar: Dollar::Var,
            ops: false,
            props: false,
            capital_types: false,
            upper_const: false,
            keywords: &["define", "endef", "export", "include", "override", "undefine", "vpath", "unexport"],
            control: &["ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "foreach", "if", "call"],
            ..BASE
        },
        // Dart, Scala and anything else C-like.
        "clike" => Spec {
            id: "clike",
            line: SLASH,
            block: C_BLOCK,
            at_attr: true,
            keywords: &[
                "abstract", "class", "const", "def", "enum", "extends", "extension", "external", "final", "fun", "implements",
                "import", "interface", "library", "mixin", "new", "object", "override", "package", "private", "protected",
                "public", "sealed", "static", "super", "this", "trait", "type", "val", "var", "with", "late", "required",
                "async", "export", "part", "typedef",
            ],
            control: &[
                "await", "break", "case", "catch", "continue", "default", "do", "else", "finally", "for", "if", "match",
                "return", "switch", "throw", "try", "while", "yield", "when",
            ],
            constants: &["true", "false", "null", "nil"],
            types: &["bool", "int", "double", "num", "String", "void", "dynamic", "Unit", "Int", "Long", "Double", "Boolean"],
            ..BASE
        },
        "json" => Spec {
            id: "json",
            family: Family::Json,
            line: SLASH,
            block: C_BLOCK,
            quotes: &['"'],
            constants: &["true", "false", "null"],
            ..BASE
        },
        "yaml" => Spec { id: "yaml", family: Family::Yaml, ..BASE },
        "toml" => Spec { id: "toml", family: Family::Toml, line: HASH, ..BASE },
        "ini" => Spec { id: "ini", family: Family::Toml, line: &[";", "#"], ..BASE },
        "css" => Spec { id: "css", family: Family::Css, block: C_BLOCK, ..BASE },
        "scss" => Spec { id: "scss", family: Family::Css, line: SLASH, block: C_BLOCK, ..BASE },
        "markup" => Spec { id: "markup", family: Family::Markup, ..BASE },
        "diff" => Spec { id: "diff", family: Family::Diff, ..BASE },
        "markdown" => Spec { id: "markdown", family: Family::Markdown, ..BASE },
        _ => return None,
    };
    Some(s)
}

/// A language name (as written after ``` in Markdown, or a file extension) -> our language id.
pub fn canonical(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rust",
        "js" | "jsx" | "javascript" | "ts" | "tsx" | "typescript" | "mjs" | "cjs" | "mts" | "cts" | "node" => "js",
        "py" | "python" | "python3" | "pyw" | "pyi" => "python",
        "c" | "h" => "c",
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "ino" | "cu" | "cuh" => "cpp",
        "cs" | "csharp" | "c#" => "csharp",
        "java" => "java",
        "kt" | "kts" | "kotlin" => "kotlin",
        "go" | "golang" => "go",
        "swift" => "swift",
        "php" | "phtml" => "php",
        "rb" | "ruby" | "rake" | "gemspec" => "ruby",
        "lua" => "lua",
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "ksh" | "command" | "env" | "profile" | "bashrc" | "zshrc" => "shell",
        "ps1" | "psm1" | "psd1" | "powershell" | "pwsh" => "powershell",
        "bat" | "cmd" | "batch" => "batch",
        "sql" | "mysql" | "pgsql" | "postgres" | "postgresql" | "sqlite" | "plsql" => "sql",
        "dockerfile" | "docker" | "containerfile" => "docker",
        "makefile" | "make" | "mk" | "mak" | "gnumakefile" => "make",
        "dart" | "scala" | "sc" | "groovy" | "gradle" | "zig" | "sol" | "proto" | "glsl" | "hlsl" | "wgsl" => "clike",
        "json" | "jsonc" | "json5" | "geojson" | "webmanifest" | "jsonl" | "ndjson" | "ipynb" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" | "conf" | "cfg" | "properties" | "editorconfig" | "gitconfig" | "desktop" | "service" | "reg" | "inf" => "ini",
        "css" => "css",
        "scss" | "sass" | "less" => "scss",
        "html" | "htm" | "xhtml" | "xml" | "svg" | "vue" | "svelte" | "xaml" | "plist" | "csproj" | "vcxproj" | "props"
        | "targets" | "xsd" | "xsl" | "xslt" | "rss" | "atom" | "kml" | "gpx" | "ui" | "qrc" | "resx" | "manifest" | "wxs"
        | "aepx" => "markup",
        "diff" | "patch" => "diff",
        "md" | "markdown" | "mdown" | "mkd" | "mdx" => "markdown",
        _ => return None,
    })
}

/// The language of a file, from its extension or well-known name (Dockerfile, Makefile, ...).
pub fn language_for_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" | "containerfile" => return Some("docker"),
        "makefile" | "gnumakefile" => return Some("make"),
        ".gitignore" | ".gitattributes" | ".dockerignore" | ".npmignore" => return Some("shell"),
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".env" => return Some("shell"),
        _ => {}
    }
    if name.starts_with("dockerfile.") {
        return Some("docker");
    }
    canonical(path.extension()?.to_str()?)
}

// ------------------------------------------------------------------------------------------
// Tokenizing
// ------------------------------------------------------------------------------------------

/// Classified ranges of `text` (sorted, not overlapping). Unclassified text is plain.
pub fn tokens(lang: &str, text: &str) -> Option<Vec<Tok>> {
    let spec = spec(canonical(lang).unwrap_or(lang))?;
    let mut out = Vec::new();
    run(&spec, text, 0, &mut out, 0);
    Some(out)
}

fn run(spec: &Spec, text: &str, base: usize, out: &mut Vec<Tok>, depth: u32) {
    match spec.family {
        Family::Code => code(spec, text, base, out),
        Family::Json => json(spec, text, base, out),
        Family::Yaml => yaml(text, base, out),
        Family::Toml => toml(spec, text, base, out),
        Family::Css => css(spec, text, base, out),
        Family::Markup => markup(text, base, out, depth),
        Family::Diff => diff(text, base, out),
        Family::Markdown => markdown(text, base, out, depth),
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn ident_len(rest: &str) -> usize {
    rest.find(|c: char| !is_ident_char(c)).unwrap_or(rest.len())
}

/// Like `ident_len`, but `-` may appear inside (CSS names).
fn dashed_len(rest: &str) -> usize {
    rest.find(|c: char| !(is_ident_char(c) || c == '-')).unwrap_or(rest.len())
}

fn line_end(rest: &str) -> usize {
    rest.find('\n').unwrap_or(rest.len())
}

fn number_len(rest: &str) -> usize {
    let b = rest.as_bytes();
    let mut i = 0;
    if b.len() > 2 && b[0] == b'0' && matches!(b[1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O') {
        return 2 + rest[2..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len() - 2);
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    // A fraction only when a digit follows the dot (so `1..5` and `1.max()` stay intact).
    if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
        i += 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    // Suffix: u8, f32, L, px ...
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    i
}

/// Bytes of the quoted string at the start of `rest` (both quotes included).
fn string_len(rest: &str, quote: char, multiline: bool) -> usize {
    let mut escaped = false;
    for (j, ch) in rest.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return j + ch.len_utf8();
        } else if ch == '\n' && !multiline {
            return j; // an unterminated string ends at the line end
        }
    }
    rest.len()
}

/// Python strings, including """triple quoted""" ones.
fn py_string_len(rest: &str, quote: char) -> usize {
    let triple: String = std::iter::repeat(quote).take(3).collect();
    if !rest.starts_with(&triple) {
        return string_len(rest, quote, false);
    }
    let mut escaped = false;
    let mut j = 3;
    while j < rest.len() {
        let ch = rest[j..].chars().next().unwrap_or(' ');
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if rest[j..].starts_with(&triple) {
            return j + 3;
        }
        j += ch.len_utf8();
    }
    rest.len()
}

/// Splits a string literal into text, escapes (`\n`, `\u{..}`) and `${...}` template markers.
fn string_tokens(text: &str, start: usize, out: &mut Vec<Tok>, escapes: bool) {
    if !escapes {
        emit(out, start, start + text.len(), Class::String);
        return;
    }
    let mut seg = 0; // start of the plain-string run
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().unwrap_or(' ');
        if ch == '\\' {
            emit(out, start + seg, start + i, Class::String);
            let mut end = i + 1;
            if let Some(next) = text[end..].chars().next() {
                end += next.len_utf8();
                if matches!(next, 'x' | 'u' | 'U') {
                    if text[end..].starts_with('{') {
                        if let Some(close) = text[end..].find('}') {
                            end += close + 1;
                        }
                    } else {
                        end += text[end..].chars().take_while(|c| c.is_ascii_hexdigit()).take(8).count();
                    }
                }
            }
            emit(out, start + i, start + end, Class::Escape);
            seg = end;
            i = end;
        } else if ch == '$' && text[i + 1..].starts_with('{') {
            match text[i..].find('}') {
                Some(close) => {
                    emit(out, start + seg, start + i, Class::String);
                    emit(out, start + i, start + i + 2, Class::Operator);
                    emit(out, start + i + close, start + i + close + 1, Class::Operator);
                    seg = i + close + 1;
                    i = seg;
                }
                None => i += 1,
            }
        } else {
            i += ch.len_utf8();
        }
    }
    emit(out, start + seg, start + text.len(), Class::String);
}

/// If `rest` starts a comment: (length, class).
fn comment(spec: &Spec, rest: &str) -> Option<(usize, Class)> {
    if let Some((open, close)) = spec.block {
        if rest.starts_with(open) {
            let doc = open == "/*" && ((rest.starts_with("/**") && !rest.starts_with("/**/")) || rest.starts_with("/*!"));
            let len = if spec.nested {
                let (mut depth, mut i) = (0usize, 0usize);
                let mut end = rest.len();
                while i < rest.len() {
                    if rest[i..].starts_with(open) {
                        depth += 1;
                        i += open.len();
                    } else if rest[i..].starts_with(close) {
                        depth = depth.saturating_sub(1);
                        i += close.len();
                        if depth == 0 {
                            end = i;
                            break;
                        }
                    } else {
                        i += rest[i..].chars().next().map_or(1, char::len_utf8);
                    }
                }
                end
            } else {
                rest[open.len()..].find(close).map(|e| open.len() + e + close.len()).unwrap_or(rest.len())
            };
            return Some((len, if doc { Class::DocComment } else { Class::Comment }));
        }
    }
    for p in spec.line {
        if rest.starts_with(p) {
            // `///` is a doc comment, `////` is not.
            let doc = spec
                .doc_line
                .iter()
                .any(|d| rest.starts_with(d) && d.chars().last().map_or(true, |l| !rest[d.len()..].starts_with(l)));
            return Some((line_end(rest), if doc { Class::DocComment } else { Class::Comment }));
        }
    }
    None
}

fn code(spec: &Spec, text: &str, base: usize, out: &mut Vec<Tok>) {
    let mut i = 0;
    let mut bol = true; // only whitespace since the line start
    let mut prev_word = String::new();
    let mut last_sig = '\n'; // last non-space character ('a' after a word / literal)
    let mut cmd_pos = spec.shell; // shell: the next word is a command

    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let at = base + i;

        if c == '\n' {
            bol = true;
            cmd_pos = spec.shell;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let was_bol = bol;
        bol = false;

        // Shebang.
        if i == 0 && rest.starts_with("#!") {
            let len = line_end(rest);
            emit(out, at, at + len, Class::Macro);
            i += len;
            continue;
        }
        // Batch: REM comments.
        if spec.id == "batch" && was_bol && rest.len() >= 3 && rest.is_char_boundary(3) && rest[..3].eq_ignore_ascii_case("rem") {
            let len = line_end(rest);
            emit(out, at, at + len, Class::Comment);
            i += len;
            continue;
        }
        // Comments.
        if let Some((len, class)) = comment(spec, rest) {
            emit(out, at, at + len, class);
            i += len;
            continue;
        }
        // C preprocessor: #include <x.h>, #define ...
        if spec.preproc && c == '#' && was_bol {
            let after_hash = &rest[1..];
            let ws = after_hash.len() - after_hash.trim_start_matches([' ', '\t']).len();
            let word = after_hash[ws..].chars().take_while(|ch| ch.is_alphabetic()).map(char::len_utf8).sum::<usize>();
            let end = 1 + ws + word;
            emit(out, at, at + end, Class::Macro);
            let tail = &rest[end..];
            let skip = tail.len() - tail.trim_start_matches([' ', '\t']).len();
            if tail[skip..].starts_with('<') {
                if let Some(close) = tail[skip..].find(['>', '\n']).filter(|&e| tail[skip + e..].starts_with('>')) {
                    emit(out, at + end + skip, at + end + skip + close + 1, Class::String);
                    i += end + skip + close + 1;
                    continue;
                }
            }
            i += end;
            continue;
        }
        // Rust attributes #[...] / #![...].
        if spec.hash_attr && c == '#' && (rest[1..].starts_with('[') || rest[1..].starts_with("![")) {
            let open = rest.find('[').unwrap_or(0);
            let mut depth = 0i32;
            let mut end = rest.len();
            for (j, ch) in rest.char_indices().skip_while(|&(j, _)| j < open) {
                match ch {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            end = j + 1;
                            break;
                        }
                    }
                    '\n' => {
                        end = j;
                        break;
                    }
                    _ => {}
                }
            }
            emit(out, at, at + end, Class::Attribute);
            i += end;
            continue;
        }
        // Decorators / annotations: @name.sub
        if spec.at_attr && c == '@' && rest[1..].chars().next().map_or(false, is_ident_start) {
            let len = 1 + rest[1..].find(|ch: char| !(is_ident_char(ch) || ch == '.')).unwrap_or(rest.len() - 1);
            emit(out, at, at + len, Class::Attribute);
            i += len;
            continue;
        }
        // Rust raw strings r"..." r#"..."#, byte strings b"...".
        if spec.rust_strings && (c == 'r' || c == 'b') {
            let prefix = if rest.starts_with("br") { 2 } else { 1 };
            let after = &rest[prefix..];
            let hashes = after.chars().take_while(|&ch| ch == '#').count();
            if (c == 'r' || prefix == 2) && after[hashes..].starts_with('"') {
                let close = format!("\"{}", "#".repeat(hashes));
                let body = prefix + hashes + 1;
                let len = rest[body..].find(&close).map(|e| body + e + close.len()).unwrap_or(rest.len());
                emit(out, at, at + len, Class::String);
                i += len;
                last_sig = 'a';
                continue;
            }
            if c == 'b' && after.starts_with('"') {
                let len = prefix + string_len(after, '"', true);
                string_tokens(&rest[..len], at, out, true);
                i += len;
                last_sig = 'a';
                continue;
            }
        }
        // Python string prefixes (r"", f"", b"", rb"").
        if spec.triple && matches!(c, 'r' | 'f' | 'b' | 'u' | 'R' | 'F' | 'B' | 'U') {
            let prefix = rest.chars().take_while(|ch| matches!(ch, 'r' | 'f' | 'b' | 'u' | 'R' | 'F' | 'B' | 'U')).count();
            if prefix <= 2 && rest[prefix..].starts_with(['"', '\'']) {
                let q = rest[prefix..].chars().next().unwrap_or('"');
                let len = prefix + py_string_len(&rest[prefix..], q);
                let raw = rest[..prefix].to_ascii_lowercase().contains('r');
                string_tokens(&rest[..len], at, out, !raw);
                i += len;
                last_sig = 'a';
                continue;
            }
        }
        // Strings.
        if spec.quotes.contains(&c) {
            // Rust lifetimes ('a, 'static) are not strings.
            if spec.rust_strings && c == '\'' {
                let name = ident_len(&rest[1..]);
                if name > 0 && rest[1..].chars().next().map_or(false, is_ident_start) && !rest[1 + name..].starts_with('\'') {
                    emit(out, at, at + 1 + name, Class::Label);
                    i += 1 + name;
                    last_sig = 'a';
                    continue;
                }
            }
            let len = if spec.triple { py_string_len(rest, c) } else { string_len(rest, c, c == '`') };
            string_tokens(&rest[..len], at, out, spec.id != "sql" && spec.id != "batch");
            i += len;
            last_sig = 'a';
            continue;
        }
        if spec.ident_quotes.contains(&c) {
            let len = string_len(rest, c, false);
            emit(out, at, at + len, Class::Property);
            i += len;
            last_sig = 'a';
            continue;
        }
        // $variables.
        if spec.dollar == Dollar::Var && c == '$' {
            let tail = &rest[1..];
            if tail.starts_with('(') {
                // $( command substitution )
                emit(out, at, at + 2, Class::Operator);
                i += 2;
                cmd_pos = spec.shell;
                continue;
            }
            let first = tail.chars().next();
            let len = if tail.starts_with('{') {
                1 + tail.find('}').map(|e| e + 1).unwrap_or(tail.len())
            } else if first.map_or(false, is_ident_start) {
                1 + ident_len(tail)
            } else if first.map_or(false, |ch| ch.is_ascii_digit() || "@*#?$!-".contains(ch)) {
                2
            } else {
                1
            };
            emit(out, at, at + len, Class::Variable);
            i += len;
            last_sig = 'a';
            continue;
        }
        // %VAR% (batch).
        if spec.percent_vars && c == '%' {
            let tail = &rest[1..];
            let len = if tail.chars().next().map_or(false, |ch| ch.is_ascii_digit() || ch == '*') {
                2
            } else if let Some(close) = tail.find(['%', '\n']).filter(|&e| tail[e..].starts_with('%') && e > 0) {
                close + 2
            } else {
                1
            };
            emit(out, at, at + len, Class::Variable);
            i += len;
            last_sig = 'a';
            continue;
        }
        // Batch labels.
        if spec.id == "batch" && c == ':' && was_bol {
            let len = line_end(rest);
            emit(out, at, at + len, Class::Label);
            i += len;
            continue;
        }
        // Command-line options (-x, --long, /switch).
        if (spec.shell || spec.id == "powershell" || spec.id == "batch" || spec.id == "docker")
            && (c == '-' || (c == '/' && spec.id == "batch"))
            && text[..i].ends_with(char::is_whitespace)
            && rest[1..].chars().next().map_or(false, |ch| ch.is_ascii_alphabetic() || ch == '-')
        {
            let len = rest.find(|ch: char| ch.is_whitespace() || matches!(ch, '=' | ')' | ';' | '"')).unwrap_or(rest.len());
            if len > 1 && len < 40 {
                emit(out, at, at + len, Class::Attr);
                i += len;
                last_sig = 'a';
                continue;
            }
        }
        // Numbers.
        if c.is_ascii_digit() {
            let len = number_len(rest);
            emit(out, at, at + len, Class::Number);
            i += len;
            last_sig = 'a';
            continue;
        }
        // Words.
        if is_ident_start(c) || (spec.dollar == Dollar::Ident && c == '$') {
            let mut len = if c == '$' { 1 + ident_len(&rest[1..]) } else { ident_len(rest) };
            if spec.hyphen_idents {
                // PowerShell cmdlets: Get-ChildItem
                while rest[len..].starts_with('-') && rest[len + 1..].chars().next().map_or(false, |ch| ch.is_ascii_alphabetic()) {
                    len += 1 + ident_len(&rest[len + 1..]);
                }
            }
            let word = &rest[..len];
            let after = rest[len..].trim_start_matches([' ', '\t']);
            let eq = |k: &&str| if spec.ignore_case { k.eq_ignore_ascii_case(word) } else { *k == word };
            let all_caps = len > 1 && word.chars().all(|ch| ch.is_uppercase() || ch.is_ascii_digit() || ch == '_');
            let mut class = if spec.control.iter().any(eq) {
                Class::Control
            } else if spec.keywords.iter().any(eq) {
                Class::Keyword
            } else if spec.constants.iter().any(eq) {
                Class::Constant
            } else if spec.types.iter().any(eq) {
                Class::BuiltinType
            } else {
                Class::Plain
            };
            let mut total = len;
            if class == Class::Plain {
                if matches!(prev_word.as_str(), "fn" | "def" | "function" | "func" | "fun" | "sub" | "proc") {
                    class = Class::Function;
                } else if matches!(
                    prev_word.as_str(),
                    "class" | "struct" | "enum" | "trait" | "interface" | "type" | "impl" | "namespace" | "record" | "object" | "union" | "protocol" | "extension" | "mod" | "module"
                ) {
                    class = Class::Type;
                } else if spec.bang_macro && rest[len..].starts_with('!') && rest[len + 1..].starts_with(['(', '[', '{']) {
                    class = Class::Macro;
                    total = len + 1;
                } else if spec.shell && cmd_pos {
                    class = Class::Function;
                } else if after.starts_with('(') && !spec.shell {
                    class = Class::Function;
                } else if spec.hyphen_idents && word.contains('-') {
                    class = Class::Function;
                } else if spec.capital_types && c.is_uppercase() && len > 1 {
                    class = if spec.upper_const && all_caps { Class::Constant } else { Class::Type };
                } else if spec.upper_const && all_caps {
                    class = Class::Constant;
                } else if spec.props && last_sig == '.' {
                    class = Class::Property;
                }
            }
            emit(out, at, at + total, class);
            cmd_pos = false;
            prev_word = word.to_string();
            i += total;
            last_sig = 'a';
            continue;
        }
        // Operators and punctuation.
        let class = if spec.ops && "+-*/%=<>!&|^~?".contains(c) { Class::Operator } else { Class::Plain };
        if spec.shell && matches!(c, '|' | ';' | '(' | '`' | '&') {
            cmd_pos = true;
        }
        emit(out, at, at + c.len_utf8(), class);
        prev_word.clear();
        last_sig = c;
        i += c.len_utf8();
    }
}

// ------------------------------------------------------------------------------------------
// JSON
// ------------------------------------------------------------------------------------------

fn json(spec: &Spec, text: &str, base: usize, out: &mut Vec<Tok>) {
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let at = base + i;
        if let Some((len, class)) = comment(spec, rest) {
            emit(out, at, at + len, class);
            i += len;
        } else if c == '"' || c == '\'' {
            let len = string_len(rest, c, false);
            let is_key = rest[len.min(rest.len())..].trim_start_matches([' ', '\t']).starts_with(':');
            if is_key {
                emit(out, at, at + len, Class::Property);
            } else {
                string_tokens(&rest[..len], at, out, true);
            }
            i += len;
        } else if c.is_ascii_digit() || (c == '-' && rest[1..].chars().next().map_or(false, |ch| ch.is_ascii_digit())) {
            let start = (c == '-') as usize;
            let len = start + number_len(&rest[start..]);
            emit(out, at, at + len, Class::Number);
            i += len;
        } else if is_ident_start(c) {
            let len = ident_len(rest);
            let word = &rest[..len];
            let is_key = rest[len..].trim_start_matches([' ', '\t']).starts_with(':');
            let class = if is_key {
                Class::Property
            } else if spec.constants.contains(&word) {
                Class::Constant
            } else {
                Class::Plain
            };
            emit(out, at, at + len, class);
            i += len;
        } else {
            i += c.len_utf8();
        }
    }
}

// ------------------------------------------------------------------------------------------
// YAML / TOML / INI
// ------------------------------------------------------------------------------------------

const YAML_CONST: &[&str] = &["true", "false", "null", "yes", "no", "on", "off", "True", "False", "Null", "~"];

/// A value (right-hand side): strings, numbers, constants, anchors, comments.
fn value(text: &str, base: usize, out: &mut Vec<Tok>, comment_chars: &[char]) {
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let at = base + i;
        let prev_space = i == 0 || text[..i].ends_with([' ', '\t']);
        if comment_chars.contains(&c) && prev_space {
            emit(out, at, at + rest.len(), Class::Comment);
            return;
        }
        if c == '"' || c == '\'' {
            let len = string_len(rest, c, false);
            string_tokens(&rest[..len], at, out, c == '"');
            i += len;
        } else if (c == '&' || c == '*') && prev_space && rest[1..].chars().next().map_or(false, is_ident_start) {
            let len = 1 + rest[1..].find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ']' | '}')).unwrap_or(rest.len() - 1);
            emit(out, at, at + len, Class::Attribute);
            i += len;
        } else if c == '!' && prev_space {
            let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
            emit(out, at, at + len, Class::Attribute);
            i += len;
        } else if (c == '|' || c == '>') && prev_space && rest[1..].trim_start_matches(|ch: char| ch == '+' || ch == '-' || ch.is_ascii_digit()).trim().is_empty() {
            emit(out, at, at + rest.len(), Class::Operator);
            return;
        } else if c.is_ascii_digit() || (c == '-' && rest[1..].chars().next().map_or(false, |ch| ch.is_ascii_digit())) {
            let start = (c == '-') as usize;
            let len = start + number_len(&rest[start..]);
            let end_ok = rest[len..].chars().next().map_or(true, |ch| !(ch.is_alphanumeric() || ch == '_'));
            if end_ok {
                emit(out, at, at + len, Class::Number);
            }
            i += len;
        } else if is_ident_start(c) || c == '~' {
            let len = if c == '~' { 1 } else { ident_len(rest) };
            if YAML_CONST.contains(&&rest[..len]) {
                emit(out, at, at + len, Class::Constant);
            }
            i += len;
        } else {
            i += c.len_utf8();
        }
    }
}

fn yaml(text: &str, base: usize, out: &mut Vec<Tok>) {
    let mut offset = 0;
    let mut block_indent: Option<usize> = None; // inside a | or > block scalar
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let at = base + offset;
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();
        offset += line.len();

        if let Some(bi) = block_indent {
            if trimmed.is_empty() || indent > bi {
                emit(out, at + indent, at + body.len(), Class::String);
                continue;
            }
            block_indent = None;
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            emit(out, at + indent, at + body.len(), Class::Comment);
            continue;
        }
        if trimmed == "---" || trimmed == "..." || trimmed.starts_with("--- ") {
            emit(out, at + indent, at + indent + 3, Class::Operator);
            continue;
        }
        let mut pos = indent;
        // List markers (possibly several: "- - a").
        while body[pos..].starts_with("- ") || &body[pos..] == "-" {
            emit(out, at + pos, at + pos + 1, Class::Operator);
            pos += 1;
            pos += body[pos..].len() - body[pos..].trim_start().len();
        }
        let rest = &body[pos..];
        // key: value
        let key_end = if rest.starts_with(['"', '\'']) {
            let q = rest.chars().next().unwrap_or('"');
            let len = string_len(rest, q, false);
            Some(len).filter(|&l| rest[l.min(rest.len())..].starts_with(':'))
        } else if rest.starts_with(['{', '[']) {
            None
        } else {
            let mut found = None;
            for (j, ch) in rest.char_indices() {
                if ch == ':' && (rest[j + 1..].is_empty() || rest[j + 1..].starts_with([' ', '\t'])) {
                    found = Some(j);
                    break;
                }
                if ch == '#' && j > 0 && rest[..j].ends_with(' ') {
                    break;
                }
            }
            found
        };
        match key_end {
            Some(end) => {
                emit(out, at + pos, at + pos + end, Class::Property);
                emit(out, at + pos + end, at + pos + end + 1, Class::Operator);
                let val_start = pos + end + 1;
                value(&body[val_start..], at + val_start, out, &['#']);
                let first = body[val_start..].split_whitespace().next().unwrap_or("");
                if matches!(first, "|" | ">" | "|-" | ">-" | "|+" | ">+") {
                    block_indent = Some(indent);
                }
            }
            None => value(rest, at + pos, out, &['#']),
        }
    }
}

fn toml(spec: &Spec, text: &str, base: usize, out: &mut Vec<Tok>) {
    let comment_chars: Vec<char> = spec.line.iter().filter_map(|p| p.chars().next()).collect();
    let separators: &[char] = if spec.id == "ini" { &['=', ':'] } else { &['='] };
    let mut offset = 0;
    let mut in_multiline: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let at = base + offset;
        offset += line.len();
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();

        if let Some(q) = in_multiline {
            if let Some(end) = body.find(q) {
                emit(out, at, at + end + 3, Class::String);
                in_multiline = None;
                value(&body[end + 3..], at + end + 3, out, &comment_chars);
            } else {
                emit(out, at, at + body.len(), Class::String);
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if comment_chars.iter().any(|&c| trimmed.starts_with(c)) {
            emit(out, at + indent, at + body.len(), Class::Comment);
            continue;
        }
        if trimmed.starts_with('[') {
            let close = trimmed.rfind(']').map(|e| e + 1).unwrap_or(trimmed.len());
            emit(out, at + indent, at + indent + close, Class::Type);
            value(&trimmed[close..], at + indent + close, out, &comment_chars);
            continue;
        }
        match trimmed.find(separators) {
            Some(e) => {
                let key = trimmed[..e].trim_end();
                emit(out, at + indent, at + indent + key.len(), Class::Property);
                emit(out, at + indent + e, at + indent + e + 1, Class::Operator);
                let vstart = indent + e + 1;
                let v = &body[vstart..];
                let vt = v.trim_start();
                if let Some(q) = ["\"\"\"", "'''"].iter().find(|q| vt.starts_with(**q)) {
                    if !vt[3..].contains(q) {
                        emit(out, at + vstart + (v.len() - vt.len()), at + body.len(), Class::String);
                        in_multiline = Some(q);
                        continue;
                    }
                }
                value(v, at + vstart, out, &comment_chars);
            }
            None => value(trimmed, at + indent, out, &comment_chars),
        }
    }
}

// ------------------------------------------------------------------------------------------
// CSS / SCSS / Less
// ------------------------------------------------------------------------------------------

/// From a `:` / `.` / `[` / word inside a block: is this a nested selector (a `{` comes before `;` or `}`)?
fn selector_ahead(rest: &str) -> bool {
    for ch in rest.chars() {
        match ch {
            '{' => return true,
            ';' | '}' => return false,
            _ => {}
        }
    }
    false
}

fn css(spec: &Spec, text: &str, base: usize, out: &mut Vec<Tok>) {
    let mut i = 0;
    let mut depth = 0i32;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let at = base + i;
        if let Some((len, class)) = comment(spec, rest) {
            emit(out, at, at + len, class);
            i += len;
            continue;
        }
        let next = rest[c.len_utf8()..].chars().next();
        let selector = depth == 0 || selector_ahead(rest);
        match c {
            '{' => {
                depth += 1;
                i += 1;
            }
            '}' => {
                depth = (depth - 1).max(0);
                i += 1;
            }
            '"' | '\'' => {
                let len = string_len(rest, c, false);
                string_tokens(&rest[..len], at, out, true);
                i += len;
            }
            '@' => {
                let len = 1 + dashed_len(&rest[1..]);
                emit(out, at, at + len, Class::Control);
                i += len;
            }
            '$' if next.map_or(false, is_ident_start) => {
                let len = 1 + dashed_len(&rest[1..]);
                emit(out, at, at + len, Class::Variable);
                i += len;
            }
            '#' => {
                let hex = rest[1..].chars().take_while(|ch| ch.is_ascii_hexdigit()).count();
                let after = rest[1 + hex..].chars().next();
                let is_color = depth > 0 && matches!(hex, 3 | 4 | 6 | 8) && !after.map_or(false, is_ident_char) && !selector_ahead(rest);
                let len = if is_color { 1 + hex } else { 1 + dashed_len(&rest[1..]) };
                emit(out, at, at + len, if is_color { Class::Number } else { Class::Function });
                i += len;
            }
            '.' if next.map_or(false, is_ident_start) && selector => {
                let len = 1 + dashed_len(&rest[1..]);
                emit(out, at, at + len, Class::Type);
                i += len;
            }
            '!' if rest[1..].trim_start().starts_with("important") => {
                let len = rest.find([';', '}', '\n']).unwrap_or(rest.len());
                emit(out, at, at + len, Class::Keyword);
                i += len;
            }
            ':' if selector => {
                // Pseudo class / element.
                let colons = rest.chars().take_while(|&ch| ch == ':').count();
                let len = colons + dashed_len(&rest[colons..]);
                emit(out, at, at + len, Class::Attribute);
                i += len;
            }
            '[' if selector => {
                let len = rest.find([']', '\n']).map(|e| e + 1).unwrap_or(rest.len());
                emit(out, at, at + len, Class::Attr);
                i += len;
            }
            '-' if rest[1..].starts_with('-') && rest[2..].chars().next().map_or(false, is_ident_start) => {
                let len = 2 + dashed_len(&rest[2..]);
                emit(out, at, at + len, Class::Variable);
                i += len;
            }
            _ if c.is_ascii_digit() || (c == '.' && next.map_or(false, |ch| ch.is_ascii_digit())) => {
                let start = (c == '.') as usize;
                let mut len = start + number_len(&rest[start..]);
                len += rest[len..].chars().take_while(|&ch| ch == '%').count();
                emit(out, at, at + len, Class::Number);
                i += len;
            }
            _ if is_ident_start(c) || (c == '-' && next.map_or(false, is_ident_start)) => {
                let len = dashed_len(rest);
                let after = rest[len..].trim_start_matches([' ', '\t']);
                let class = if after.starts_with('(') {
                    Class::Function
                } else if depth > 0 && after.starts_with(':') && !selector_ahead(&rest[len..]) {
                    Class::Property
                } else if selector {
                    Class::Tag
                } else {
                    Class::Plain
                };
                emit(out, at, at + len, class);
                i += len;
            }
            _ => i += c.len_utf8(),
        }
    }
}

// ------------------------------------------------------------------------------------------
// HTML / XML
// ------------------------------------------------------------------------------------------

fn markup(text: &str, base: usize, out: &mut Vec<Tok>, depth: u32) {
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let at = base + i;
        if rest.starts_with("<!--") {
            let len = rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            emit(out, at, at + len, Class::Comment);
            i += len;
        } else if rest.starts_with("<![CDATA[") {
            let len = rest.find("]]>").map(|e| e + 3).unwrap_or(rest.len());
            let inner_end = len.saturating_sub(3).max(9);
            emit(out, at, at + 9, Class::Operator);
            emit(out, at + 9, at + inner_end, Class::String);
            emit(out, at + inner_end, at + len, Class::Operator);
            i += len;
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            let len = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            emit(out, at, at + len, Class::Macro);
            i += len;
        } else if rest.starts_with('<') && rest[1..].chars().next().map_or(false, |c| is_ident_start(c) || c == '/') {
            let len = tag(rest, at, out);
            let name: String = rest[1..].chars().take_while(|&c| is_ident_char(c) || c == '-' || c == ':').collect::<String>().to_ascii_lowercase();
            let self_closed = rest[..len].ends_with("/>");
            i += len;
            // Embedded languages.
            if depth < 2 && !self_closed && !rest.starts_with("</") && (name == "script" || name == "style") {
                let close = format!("</{name}");
                let end = text[i..].to_ascii_lowercase().find(&close).unwrap_or(text.len() - i);
                if let Some(inner_spec) = spec(if name == "script" { "js" } else { "css" }) {
                    run(&inner_spec, &text[i..i + end], base + i, out, depth + 1);
                }
                i += end;
            }
        } else if rest.starts_with('&') {
            let entity = rest[1..]
                .find(|c: char| !(c.is_alphanumeric() || c == '#'))
                .filter(|&e| rest[1 + e..].starts_with(';') && e > 0 && e < 10);
            match entity {
                Some(e) => {
                    emit(out, at, at + e + 2, Class::Escape);
                    i += e + 2;
                }
                None => i += 1,
            }
        } else if rest.starts_with("{{") || rest.starts_with("{%") {
            let close = if rest.starts_with("{{") { "}}" } else { "%}" };
            let len = rest.find(close).map(|e| e + 2).unwrap_or(2);
            emit(out, at, at + len, Class::Variable);
            i += len;
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
}

/// One tag (`<a href="x" disabled>`); returns its length. Name, attributes, values and brackets get their own classes.
fn tag(rest: &str, at: usize, out: &mut Vec<Tok>) -> usize {
    let mut i = 1;
    if rest[i..].starts_with('/') {
        i += 1;
    }
    emit(out, at, at + i, Class::Operator);
    let name = rest[i..].find(|c: char| !(is_ident_char(c) || matches!(c, '-' | ':' | '.'))).unwrap_or(rest.len() - i);
    let class = if rest[i..].chars().next().map_or(false, char::is_uppercase) { Class::Type } else { Class::Tag };
    emit(out, at + i, at + i + name, class);
    i += name;
    while i < rest.len() {
        let r = &rest[i..];
        let c = r.chars().next().unwrap_or(' ');
        if c == '>' {
            emit(out, at + i, at + i + 1, Class::Operator);
            return i + 1;
        }
        if r.starts_with("/>") {
            emit(out, at + i, at + i + 2, Class::Operator);
            return i + 2;
        }
        if c == '"' || c == '\'' {
            let len = r[1..].find(c).map(|e| e + 2).unwrap_or(r.len());
            emit(out, at + i, at + i + len, Class::String);
            i += len;
        } else if c == '=' {
            emit(out, at + i, at + i + 1, Class::Operator);
            i += 1;
        } else if c.is_whitespace() {
            i += c.len_utf8();
        } else {
            let len = r.find(|ch: char| ch.is_whitespace() || matches!(ch, '=' | '>' | '/' | '"' | '\'')).unwrap_or(r.len()).max(c.len_utf8());
            emit(out, at + i, at + i + len, Class::Attr);
            i += len;
        }
    }
    i
}

// ------------------------------------------------------------------------------------------
// Diff
// ------------------------------------------------------------------------------------------

fn diff(text: &str, base: usize, out: &mut Vec<Tok>) {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let at = base + offset;
        offset += line.len();
        let class = if body.starts_with("+++") || body.starts_with("---") || body.starts_with("diff ") || body.starts_with("index ") {
            Class::Heading
        } else if body.starts_with("@@") {
            Class::Function
        } else if body.starts_with('+') || body.starts_with('>') {
            Class::Added
        } else if body.starts_with('-') || body.starts_with('<') {
            Class::Removed
        } else {
            Class::Plain
        };
        emit(out, at, at + body.len(), class);
    }
}

// ------------------------------------------------------------------------------------------
// Markdown source
// ------------------------------------------------------------------------------------------

fn markdown(text: &str, base: usize, out: &mut Vec<Tok>, depth: u32) {
    let mut offset = 0;
    // An open code fence: (fence characters, where the code starts, language).
    let mut fence: Option<(String, usize, String)> = None;
    let mut front = text.starts_with("---\n") || text.starts_with("---\r\n");
    let mut line_no = 0;

    let flush_code = |out: &mut Vec<Tok>, from: usize, to: usize, lang: &str| {
        let code = &text[from.min(text.len())..to.min(text.len())];
        match canonical(lang).and_then(spec).filter(|_| depth < 2) {
            Some(s) => run(&s, code, base + from, out, depth + 1),
            None => emit(out, base + from, base + to, Class::String),
        }
    };

    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let at = base + offset;
        let line_start = offset;
        offset += line.len();
        line_no += 1;
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();

        // YAML front matter.
        if front {
            if line_no == 1 || body.trim_end() == "---" || body.trim_end() == "..." {
                emit(out, at, at + body.len(), Class::Operator);
                front = line_no == 1;
            } else {
                yaml(&format!("{body}\n"), at, out);
            }
            continue;
        }

        // Fenced code: the whole block is highlighted as its language.
        if let Some((marker, start, lang)) = fence.clone() {
            let ch = marker.chars().next().unwrap_or('`');
            if trimmed.starts_with(&marker) && trimmed.trim_end_matches(ch).trim().is_empty() {
                flush_code(out, start, line_start, &lang);
                emit(out, at + indent, at + body.len(), Class::Operator);
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap_or('`');
            let n = trimmed.chars().take_while(|&c| c == ch).count();
            let lang = trimmed[n..].split_whitespace().next().unwrap_or("").to_string();
            emit(out, at + indent, at + indent + n, Class::Operator);
            emit(out, at + indent + n, at + body.len(), Class::Function);
            fence = Some((ch.to_string().repeat(n), offset, lang));
            continue;
        }
        // Headings.
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with([' ', '\t']) {
            emit(out, at, at + body.len(), Class::Heading);
            continue;
        }
        // Horizontal rules.
        if trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) && trimmed.chars().filter(|&c| c != ' ').count() >= 3 {
            emit(out, at, at + body.len(), Class::Operator);
            continue;
        }
        // Block markers: > quotes, - * + lists, 1. lists, | tables.
        let mut pos = indent;
        while body[pos..].starts_with('>') {
            emit(out, at + pos, at + pos + 1, Class::Comment);
            pos += 1;
            pos += body[pos..].len() - body[pos..].trim_start().len();
        }
        let rest = &body[pos..];
        if rest.starts_with(['-', '*', '+']) && rest[1..].starts_with(' ') {
            emit(out, at + pos, at + pos + 1, Class::Operator);
            pos += 2;
            if body[pos..].starts_with("[ ]") || body[pos..].starts_with("[x]") || body[pos..].starts_with("[X]") {
                emit(out, at + pos, at + pos + 3, Class::Constant);
                pos += 3;
            }
        } else {
            let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && digits < 10 && rest[digits..].starts_with(". ") {
                emit(out, at + pos, at + pos + digits + 1, Class::Operator);
                pos += digits + 1;
            }
        }
        if body[pos..].trim_start().starts_with('|') {
            let separator_row = body[pos..].trim().chars().all(|c| "|:- ".contains(c));
            for (j, ch) in body[pos..].char_indices() {
                if ch == '|' || (separator_row && matches!(ch, ':' | '-')) {
                    emit(out, at + pos + j, at + pos + j + 1, Class::Operator);
                }
            }
        }
        inline_markdown(&body[pos..], at + pos, out);
    }
    // An unclosed fence runs to the end.
    if let Some((_, start, lang)) = fence {
        flush_code(out, start, text.len(), &lang);
    }
}

fn inline_markdown(text: &str, base: usize, out: &mut Vec<Tok>) {
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let at = base + i;
        match c {
            '\\' => i += 1 + rest[1..].chars().next().map_or(0, char::len_utf8),
            '`' => {
                let n = rest.chars().take_while(|&ch| ch == '`').count();
                let fence = "`".repeat(n);
                match rest[n..].find(&fence) {
                    Some(e) => {
                        emit(out, at, at + n + e + n, Class::String);
                        i += n + e + n;
                    }
                    None => i += n,
                }
            }
            '<' if rest[1..].chars().next().map_or(false, |ch| ch.is_alphabetic() || ch == '/') => match rest.find('>') {
                Some(e) => {
                    let len = e + 1;
                    if rest[1..].starts_with("http") || rest[1..].starts_with("mailto:") {
                        emit(out, at, at + len, Class::Link);
                    } else {
                        tag(&rest[..len], at, out);
                    }
                    i += len;
                }
                None => i += 1,
            },
            '*' | '_' => {
                let n = rest.chars().take_while(|&ch| ch == c).count().min(3);
                let marker: String = std::iter::repeat(c).take(n).collect();
                let prev_ok = c == '*' || !text[..i].chars().last().map_or(false, is_ident_char);
                match rest[n..].find(&marker).filter(|&e| e > 0 && prev_ok && !rest[n..].starts_with(' ')) {
                    Some(e) => {
                        emit(out, at, at + n + e + n, Class::Constant);
                        i += n + e + n;
                    }
                    None => i += n,
                }
            }
            '~' if rest.starts_with("~~") => match rest[2..].find("~~") {
                Some(e) => {
                    emit(out, at, at + e + 4, Class::Comment);
                    i += e + 4;
                }
                None => i += 2,
            },
            '!' | '[' => {
                // ![alt](url) and [text](url) / [text][ref] / [ref]: url
                let start = (c == '!') as usize;
                if start == 1 && !rest[1..].starts_with('[') {
                    i += 1;
                    continue;
                }
                let r = &rest[start..];
                match r.find("](").and_then(|m| r[m..].find(')').map(|e| (m, m + e))).filter(|&(m, _)| !r[..m].contains('\n')) {
                    Some((m, close)) => {
                        emit(out, at, at + start + m + 1, Class::Function);
                        emit(out, at + start + m + 1, at + start + close + 1, Class::Link);
                        i += start + close + 1;
                    }
                    None => match r.find(']').filter(|&e| r[e + 1..].starts_with(['[', ':'])) {
                        Some(e) => {
                            emit(out, at, at + start + e + 1, Class::Function);
                            i += start + e + 1;
                        }
                        None => i += 1 + start,
                    },
                }
            }
            'h' if rest.starts_with("http://") || rest.starts_with("https://") => {
                let len = rest.find(|ch: char| ch.is_whitespace() || matches!(ch, ')' | '>' | ']')).unwrap_or(rest.len());
                emit(out, at, at + len, Class::Link);
                i += len;
            }
            _ => i += c.len_utf8(),
        }
    }
}

// ------------------------------------------------------------------------------------------
// Using the tokens
// ------------------------------------------------------------------------------------------

fn push(job: &mut LayoutJob, font: &FontId, text: &str, color: Color32) {
    if !text.is_empty() {
        job.append(text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
    }
}

/// The code as a coloured, non-wrapping text layout (Markdown code blocks). Unknown languages stay plain.
pub fn layout(lang: &str, code: &str, size: f32, base: Color32) -> LayoutJob {
    let font = FontId::monospace(size);
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let toks = if code.len() <= 200_000 { tokens(lang, code) } else { None };
    let mut at = 0;
    for t in toks.iter().flatten() {
        if t.start < at || t.end > code.len() || !code.is_char_boundary(t.start) || !code.is_char_boundary(t.end) {
            continue;
        }
        push(&mut job, &font, &code[at..t.start], base);
        push(&mut job, &font, &code[t.start..t.end], color(t.class, base));
        at = t.end;
    }
    push(&mut job, &font, &code[at..], base);
    job
}

/// A whole file, coloured: per line, the classified byte ranges inside that line.
/// Built once on the loader thread (so multi-line comments and strings work), drawn per visible line.
pub struct Highlighted {
    lines: Vec<Vec<(u32, u32, Class)>>,
}

impl Highlighted {
    pub fn build(lang: &str, lines: &[String]) -> Option<Self> {
        let text = lines.join("\n");
        let toks = tokens(lang, &text)?;
        let mut out: Vec<Vec<(u32, u32, Class)>> = vec![Vec::new(); lines.len()];
        let mut starts = Vec::with_capacity(lines.len());
        let mut at = 0;
        for l in lines {
            starts.push(at);
            at += l.len() + 1;
        }
        for t in toks {
            // First line the token touches; a token may span several lines.
            let mut n = starts.partition_point(|&s| s <= t.start).saturating_sub(1);
            while n < lines.len() && starts[n] < t.end {
                let (ls, le) = (starts[n], starts[n] + lines[n].len());
                let (s, e) = (t.start.max(ls), t.end.min(le));
                if e > s {
                    out[n].push(((s - ls) as u32, (e - ls) as u32, t.class));
                }
                n += 1;
            }
        }
        Some(Self { lines: out })
    }

    /// The coloured layout for line `i` (whose text is `text`).
    pub fn line_job(&self, i: usize, text: &str, size: f32, base: Color32) -> LayoutJob {
        let font = FontId::monospace(size);
        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let mut at = 0;
        for &(s, e, class) in self.lines.get(i).into_iter().flatten() {
            let (s, e) = (s as usize, e as usize);
            if s < at || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
                continue;
            }
            push(&mut job, &font, &text[at..s], base);
            push(&mut job, &font, &text[s..e], color(class, base));
            at = e;
        }
        push(&mut job, &font, &text[at..], base);
        if text.is_empty() {
            push(&mut job, &font, " ", base); // an empty line still needs its height
        }
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(lang: &str, text: &str) -> Vec<(String, Class)> {
        tokens(lang, text).unwrap().iter().map(|t| (text[t.start..t.end].to_string(), t.class)).collect()
    }

    fn has(v: &[(String, Class)], s: &str, c: Class) -> bool {
        v.iter().any(|(t, k)| t == s && *k == c)
    }

    #[test]
    fn rust_tokens() {
        let v = classes(
            "rust",
            "#[derive(Debug)]\n/// doc\nfn main<'a>(x: &'a str) -> i32 {\n  let s = \"a\\n\"; println!(\"{}\", s); if x { return 1_000; }\n}\n",
        );
        assert!(has(&v, "#[derive(Debug)]", Class::Attribute));
        assert!(has(&v, "/// doc", Class::DocComment));
        assert!(has(&v, "fn", Class::Keyword));
        assert!(has(&v, "main", Class::Function));
        assert!(has(&v, "'a", Class::Label));
        assert!(has(&v, "i32", Class::BuiltinType));
        assert!(has(&v, "str", Class::BuiltinType));
        assert!(has(&v, "\\n", Class::Escape));
        assert!(has(&v, "println!", Class::Macro));
        assert!(has(&v, "if", Class::Control));
        assert!(has(&v, "return", Class::Control));
        assert!(has(&v, "1_000", Class::Number));
    }

    #[test]
    fn python_json_yaml() {
        let v = classes("python", "@app.route('/')\ndef f(self, x=None):\n    \"\"\"doc\"\"\"\n    return f\"{x}\"  # c\n");
        assert!(has(&v, "@app.route", Class::Attribute));
        assert!(has(&v, "def", Class::Keyword));
        assert!(has(&v, "f", Class::Function));
        assert!(has(&v, "None", Class::Constant));
        assert!(has(&v, "return", Class::Control));
        assert!(has(&v, "# c", Class::Comment));
        let j = classes("json", "{\"a\": [1, true, \"x\"]}");
        assert!(has(&j, "\"a\"", Class::Property));
        assert!(has(&j, "1", Class::Number));
        assert!(has(&j, "true", Class::Constant));
        assert!(has(&j, "\"x\"", Class::String));
        let y = classes("yaml", "name: demo # c\nlist:\n  - a: 1\n  - \"s\"\n");
        assert!(has(&y, "name", Class::Property));
        assert!(has(&y, "# c", Class::Comment));
        assert!(has(&y, "1", Class::Number));
    }

    #[test]
    fn css_html_shell_sql() {
        let c = classes("css", ".a:hover { color: #fff; margin: 4px 0; }\n@media (min-width: 1px) { a { b: c } }");
        assert!(has(&c, ".a", Class::Type));
        assert!(has(&c, ":hover", Class::Attribute));
        assert!(has(&c, "color", Class::Property));
        assert!(has(&c, "#fff", Class::Number));
        assert!(has(&c, "4px", Class::Number));
        assert!(has(&c, "@media", Class::Control));
        let h = classes("html", "<div class=\"a\">x &amp; y<script>let a = 1;</script></div>");
        assert!(has(&h, "div", Class::Tag));
        assert!(has(&h, "class", Class::Attr));
        assert!(has(&h, "\"a\"", Class::String));
        assert!(has(&h, "&amp;", Class::Escape));
        assert!(has(&h, "let", Class::Keyword));
        let s = classes("sh", "echo \"$HOME\" | grep -i foo # c\nif [ -f x ]; then ls; fi");
        assert!(has(&s, "echo", Class::Function));
        assert!(has(&s, "-i", Class::Attr));
        assert!(has(&s, "if", Class::Control));
        let q = classes("sql", "SELECT a, \"b\" FROM t WHERE x IS NULL -- c");
        assert!(has(&q, "SELECT", Class::Keyword));
        assert!(has(&q, "IS", Class::Control));
        assert!(has(&q, "NULL", Class::Constant));
    }

    #[test]
    fn line_split_keeps_multiline_comment() {
        let lines: Vec<String> = ["a /* x", "y */ b", "c"].iter().map(|s| s.to_string()).collect();
        let h = Highlighted::build("c", &lines).unwrap();
        assert_eq!(h.lines[0].len(), 1);
        assert_eq!(h.lines[1][0].2, Class::Comment);
        assert!(h.lines[2].is_empty());
    }

    #[test]
    fn markdown_source_and_toml() {
        let m = classes("md", "# Title\n\n```rust\nfn x() {}\n```\n- [a](http://x.y) `c`\n");
        assert!(has(&m, "# Title", Class::Heading));
        assert!(has(&m, "fn", Class::Keyword));
        assert!(has(&m, "`c`", Class::String));
        let t = classes("toml", "[package]\nname = \"q\" # c\nn = 3\n");
        assert!(has(&t, "[package]", Class::Type));
        assert!(has(&t, "name", Class::Property));
        assert!(has(&t, "3", Class::Number));
    }
}

#[cfg(test)]
mod stress {
    use super::*;

    #[test]
    fn never_panics_and_stays_sorted() {
        let langs = [
            "rust", "js", "python", "c", "cpp", "csharp", "java", "kotlin", "go", "swift", "php", "ruby", "lua", "shell",
            "powershell", "batch", "sql", "docker", "make", "clike", "json", "yaml", "toml", "ini", "css", "scss", "markup",
            "diff", "markdown",
        ];
        let mut corpus: Vec<String> = vec![
            "é😀\"unterminated\n'x\n/* open\n<!-- open\n<a href=\"x\n```rust\nfn\n#[attr\n r#\"raw\n${x\n\\u{1F600}\\x\n".into(),
            "{{ x }} <script>let é = '😀';</script> <style>a{b:c</style> <![CDATA[ x".into(),
            "a: |\n  block\nb: &x !!str 5\n- - 1\n[sec\nk = \"\"\"\nmulti\n\"\"\"\n".into(),
            "$(echo $HOME ${X} $1) %PATH% rem x\n:label\n-x --y /z".into(),
            "".into(),
            "é".into(),
        ];
        for entry in std::fs::read_dir("src").unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
            corpus.push(text);
        }
        for lang in langs {
            for text in &corpus {
                // Whole text, and every prefix cut at a char boundary (sampled for speed).
                let step = (text.len() / 6).max(37);
                let cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).step_by(step).collect();
                for end in std::iter::once(text.len()).chain(cuts) {
                    let slice = &text[..end];
                    let toks = tokens(lang, slice).unwrap();
                    let mut prev = 0;
                    for t in &toks {
                        assert!(t.start >= prev && t.end > t.start && t.end <= slice.len(), "{lang}: bad token {t:?}");
                        assert!(slice.is_char_boundary(t.start) && slice.is_char_boundary(t.end), "{lang}: boundary {t:?}");
                        prev = t.end;
                    }
                    let lines: Vec<String> = slice.lines().map(String::from).collect();
                    let h = Highlighted::build(lang, &lines).unwrap();
                    for (n, l) in lines.iter().enumerate() {
                        let _ = h.line_job(n, l, 13.0, Color32::WHITE);
                    }
                }
            }
        }
    }
}

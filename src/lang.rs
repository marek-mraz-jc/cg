//! Everything that differs per language: indexer command, kinds, paths, tests, complexity, Rust impls.
//! Adding a language = a variant here plus its SCIP indexer command.

use crate::scip::{Desc, Suffix, Symbol, SymbolInformation};
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Typescript,
    Python,
}

pub const CALLABLE: &[&str] = &["Function", "Method", "StaticMethod", "TraitMethod", "AbstractMethod", "Constructor", "Macro"];
pub const TYPES: &[&str] = &["Struct", "Enum", "Trait", "TypeAlias", "Type", "Union", "Class", "Interface"];
const SKIP: &[&str] = &["Variable", "Parameter", "SelfParameter", "TypeParameter", "UnspecifiedKind"];

pub fn is_callable(kind: &str) -> bool {
    CALLABLE.contains(&kind)
}

pub fn is_type(kind: &str) -> bool {
    TYPES.contains(&kind)
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Typescript => "typescript",
            Lang::Python => "python",
        }
    }

    pub fn from_scheme(scheme: &str) -> Option<Lang> {
        match scheme {
            "rust-analyzer" => Some(Lang::Rust),
            "scip-typescript" => Some(Lang::Typescript),
            "scip-python" => Some(Lang::Python),
            _ => None,
        }
    }

    /// Shell command run in the index root; `{out}` = output file, `{name}` = project name.
    /// The npx fallback keeps the TS/Python indexers optional installs.
    pub fn default_cmd(self) -> &'static str {
        match self {
            Lang::Rust => "rust-analyzer scip . --output {out}",
            Lang::Typescript => {
                "if command -v scip-typescript >/dev/null; then I=scip-typescript; else I='npx --yes @sourcegraph/scip-typescript'; fi; \
                 if [ -f tsconfig.json ]; then $I index --output {out}; else $I index --infer-tsconfig --output {out}; fi"
            }
            Lang::Python => {
                "if command -v scip-python >/dev/null; then I=scip-python; else I='npx --yes @sourcegraph/scip-python'; fi; \
                 $I index . --project-name {name} --output {out}"
            }
        }
    }

    /// Files whose edit makes this language's SCIP index stale: sources and the manifests that
    /// decide how they resolve.
    pub fn is_index_input(self, path: &str) -> bool {
        let base = path.rsplit('/').next().unwrap_or(path);
        let ext = base.rsplit_once('.').map_or("", |(_, e)| e);
        match self {
            Lang::Rust => ext == "rs" || matches!(base, "Cargo.toml" | "Cargo.lock"),
            Lang::Typescript => {
                matches!(ext, "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts")
                    || matches!(base, "package.json" | "tsconfig.json")
            }
            Lang::Python => ext == "py" || matches!(base, "pyproject.toml" | "setup.py" | "setup.cfg"),
        }
    }

    pub fn is_test_file(self, path: &str) -> bool {
        let p = format!("/{path}");
        let base = path.rsplit('/').next().unwrap_or(path);
        match self {
            Lang::Rust => p.contains("/tests/") || p.contains("/benches/"),
            Lang::Typescript => {
                base.contains(".test.") || base.contains(".spec.") || p.contains("/__tests__/") || p.contains("/e2e/")
            }
            Lang::Python => {
                base.starts_with("test_") || base.ends_with("_test.py") || base == "conftest.py" || p.contains("/tests/")
            }
        }
    }

    pub fn is_test_symbol(self, name: &str, attrs: &str) -> bool {
        match self {
            Lang::Rust => attrs.contains("#[test]") || TEST_ATTR.is_match(attrs),
            Lang::Python => name.starts_with("test_") || name.starts_with("Test"),
            Lang::Typescript => false,
        }
    }

    /// 1 + branch points in a body: an approximate cyclomatic complexity.
    pub fn complexity(self, body: &str) -> i64 {
        let re: &Regex = match self {
            Lang::Rust => &BRANCH_RS,
            Lang::Typescript => &BRANCH_TS,
            Lang::Python => &BRANCH_PY,
        };
        1 + re.find_iter(body).count() as i64
    }

    pub fn visibility(self, sig: &str, name: &str) -> &'static str {
        match self {
            Lang::Rust if sig.starts_with("pub(crate)") => "crate",
            Lang::Rust if sig.starts_with("pub(") => "restricted",
            Lang::Rust if sig.starts_with("pub ") => "pub",
            Lang::Rust => "private",
            Lang::Typescript if sig.contains("private ") || name.starts_with('#') => "private",
            Lang::Typescript if sig.contains("protected ") => "restricted",
            Lang::Python if name.starts_with("__") && name.ends_with("__") => "pub",
            _ if name.starts_with('_') => "private",
            _ => "pub",
        }
    }

    pub fn is_async(self, sig: &str) -> bool {
        match self {
            Lang::Rust => sig.contains("async fn"),
            Lang::Typescript => sig.contains("async ") || sig.contains("Promise<"),
            Lang::Python => sig.contains("async def"),
        }
    }
}

static TEST_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#\[[\w:]*test\b").unwrap());
static BRANCH_RS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:if|while|for|loop)\b|&&|\|\||=>|\?[;.)]").unwrap());
static BRANCH_TS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:if|while|for|case|catch)\b|&&|\|\||\?\?").unwrap());
static BRANCH_PY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:if|elif|while|for|except|and|or|case)\b").unwrap());

/// Hover text without its markdown code fence, whitespace collapsed.
fn code_block(doc: &str) -> String {
    let body = doc.strip_prefix("```").map_or(doc, |s| s.split_once('\n').map_or("", |(_, b)| b));
    let body = body.split("```").next().unwrap_or(body);
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Signature text: rust-analyzer fills signature_documentation; the TS/Python indexers put a
/// fenced code block first in `documentation`.
pub fn signature(si: Option<&SymbolInformation>) -> String {
    let Some(si) = si else { return String::new() };
    let text = si.signature_documentation.as_ref().map(|s| s.text.as_str()).unwrap_or("");
    if !text.is_empty() {
        return text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    si.documentation.first().filter(|d| d.starts_with("```")).map(|d| code_block(d)).unwrap_or_default()
}

/// First paragraph of the prose documentation (not the signature block).
pub fn doc_text(si: Option<&SymbolInformation>) -> String {
    let Some(si) = si else { return String::new() };
    si.documentation
        .iter()
        .rev()
        .find(|d| !d.starts_with("```") && !d.starts_with("(module)"))
        .map(|d| d.split("\n\n").next().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// Kind name: SCIP's own when the indexer sets one, else read from the hover text, else from the
/// descriptor shape. Returns None for symbols the graph skips (locals, parameters, plain variables).
pub fn kind(si: Option<&SymbolInformation>, sym: &Symbol) -> Option<&'static str> {
    let last = sym.last()?;
    // object-literal members (`method0:`) are not addressable; Python's `__init__:` is the module
    if matches!(last.suffix, Suffix::Param | Suffix::TypeParam) || last.suffix == Suffix::Meta && last.name != "__init__" {
        return None;
    }
    if let Some(k) = si.and_then(|s| s.kind.enum_value().ok()) {
        let name: &'static str = kind_name(&format!("{k:?}"));
        if name != "UnspecifiedKind" {
            return (!SKIP.contains(&name)).then_some(name);
        }
    }
    let parent_is_type = sym.descs.len() > 1 && sym.descs[sym.descs.len() - 2].suffix == Suffix::Type;
    if let Some(sig) = si.map(|s| signature(Some(s))).filter(|s| !s.is_empty()) {
        let sig = sig.trim_start_matches("export ").trim_start_matches("declare ").trim_start_matches("abstract ");
        let word = sig.split([' ', '(']).next().unwrap_or("");
        let k = match word {
            "class" => Some("Class"),
            "interface" => Some("Interface"),
            "function" => Some("Function"),
            "def" | "async" if parent_is_type => Some("Method"),
            "def" | "async" => Some("Function"),
            "constructor" => Some("Constructor"),
            "module" | "namespace" => Some("Module"),
            "type" => Some("TypeAlias"),
            "enum" => Some("Enum"),
            _ if sig.starts_with("(method)") => Some("Method"),
            _ if sig.starts_with("(enum member)") => Some("EnumMember"),
            _ if sig.starts_with("(module)") => Some("Module"),
            // `const Panel = () => <div/>`: a React component / arrow function is callable
            _ if (sig.starts_with("const ") || sig.starts_with("var ") || sig.starts_with("let ")) && sig.contains("=>") => {
                Some("Function")
            }
            _ if sig.starts_with("(property)") => Some("Field"),
            _ if sig.starts_with("const ") => Some("Constant"),
            _ if sig.starts_with("var ") || sig.starts_with("let ") || sig.starts_with("(variable)") => {
                Some(if parent_is_type { "Field" } else { "Constant" })
            }
            _ => None,
        };
        if k.is_some() {
            return k;
        }
    }
    Some(guess_kind(&sym.descs))
}

fn kind_name(k: &str) -> &'static str {
    // leak-free mapping onto a fixed vocabulary
    const KNOWN: &[&str] = &[
        "Function", "Method", "StaticMethod", "TraitMethod", "AbstractMethod", "Constructor", "Macro", "Struct", "Enum",
        "EnumMember", "Field", "Trait", "TypeAlias", "Type", "Union", "Class", "Interface", "Constant", "StaticVariable",
        "Module", "Namespace", "Package", "AssociatedType", "Variable", "Parameter", "SelfParameter", "TypeParameter",
        "Property", "Impl",
    ];
    KNOWN.iter().find(|&&n| n == k).copied().unwrap_or(if k == "UnspecifiedKind" { "UnspecifiedKind" } else { "Term" })
}

/// Kind from descriptor suffixes only (external symbols, symbols without hover text).
pub fn guess_kind(descs: &[Desc]) -> &'static str {
    let Some(last) = descs.last() else { return "Module" };
    match last.suffix {
        Suffix::Method if descs.len() > 1 && descs[descs.len() - 2].suffix == Suffix::Type => "Method",
        Suffix::Method => "Function",
        Suffix::Type => "Type",
        Suffix::Macro => "Macro",
        Suffix::Namespace => "Module",
        Suffix::Meta if last.name == "__init__" => "Module",
        _ => "Term",
    }
}

pub fn is_impl(descs: &[Desc]) -> bool {
    descs.iter().any(|d| d.name == "impl" && d.suffix == Suffix::Type)
}

/// (module descriptors, self type, trait, descriptors after the impl) of a rust-analyzer impl# symbol.
pub fn impl_parts(descs: &[Desc]) -> (&[Desc], String, Option<String>, &[Desc]) {
    let k = descs.iter().position(|d| d.name == "impl" && d.suffix == Suffix::Type).unwrap_or(0);
    let mut j = k + 1;
    let mut brackets = Vec::new();
    while j < descs.len() && descs[j].suffix == Suffix::TypeParam {
        brackets.push(descs[j].name.clone());
        j += 1;
    }
    let ty = brackets.first().cloned().unwrap_or_else(|| "?".into());
    (&descs[..k], ty, brackets.get(1).cloned(), &descs[j..])
}

/// `Operation<'_>` -> `Operation`, `Page<Vec<T>>` -> `Page`: paths stay typeable.
pub fn strip_generics(ty: &str) -> String {
    let mut depth = 0;
    ty.chars()
        .filter(|&c| {
            match c {
                '<' => depth += 1,
                '>' if depth > 0 => {
                    depth -= 1;
                    return false;
                }
                _ => {}
            }
            depth == 0
        })
        .collect()
}

/// Human path used for querying: `crate::module::Type::method` for Rust,
/// `web/src/api.ts::HttpSource::list` (repo-relative file) / `app::App::get` for TypeScript / Python.
pub fn display_path(lang: Option<Lang>, sym: &Symbol) -> String {
    let named = |d: &&Desc| !matches!(d.suffix, Suffix::TypeParam | Suffix::Param) && !d.name.is_empty();
    if lang == Some(Lang::Rust) || lang.is_none() && sym.scheme == "rust-analyzer" {
        let krate = sym.package.replace('-', "_");
        let mut names: Vec<String> = vec![krate];
        if is_impl(&sym.descs) {
            let (mods, ty, _, rest) = impl_parts(&sym.descs);
            names.extend(mods.iter().filter(|d| matches!(d.suffix, Suffix::Namespace | Suffix::Type)).map(|d| d.name.clone()));
            names.push(strip_generics(&ty));
            names.extend(rest.iter().filter(named).map(|d| d.name.clone()));
        } else {
            names.extend(sym.descs.iter().filter(named).map(|d| d.name.clone()));
        }
        return names.join("::");
    }
    let ns: Vec<&str> =
        sym.descs.iter().take_while(|d| d.suffix == Suffix::Namespace).map(|d| d.name.as_str()).collect();
    let rest: Vec<&str> = sym.descs[ns.len()..]
        .iter()
        .filter(named)
        .filter(|d| !(d.suffix == Suffix::Meta && d.name == "__init__"))
        .map(|d| d.name.as_str())
        .collect();
    let head = ns.join("/");
    match (head.is_empty(), rest.is_empty()) {
        (true, _) => rest.join("::"),
        (false, true) => head,
        (false, false) => format!("{head}::{}", rest.join("::")),
    }
}

// ------------------------------------------------------------------ Rust source scanning

static IMPL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?impl\b").unwrap());
static STRIP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)'|//.*$"#).unwrap());
/// End (line, col) of the item whose name sits at `line`: the matching `}` of its first `{`, or the
/// first `;` when that comes first (declarations). Used when the indexer's range is unusable.
pub fn rust_item_end(lines: &[&str], line: usize) -> (i32, i32) {
    let mut depth = 0i32;
    for (li, raw) in lines.iter().enumerate().skip(line) {
        let clean = STRIP_RE.replace_all(raw, |c: &regex::Captures| " ".repeat(c[0].len()));
        for (ci, ch) in clean.char_indices() {
            match ch {
                ';' if depth == 0 => return (li as i32, ci as i32 + 1),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return (li as i32, ci as i32 + 1);
                    }
                }
                _ => {}
            }
        }
    }
    (line as i32, 0)
}

static MACRO_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\$crate|\bcrate|\b[a-z_][a-z0-9_]*)((?:::[A-Za-z_][A-Za-z0-9_]*)+)\s*(?:!|\()").unwrap());

/// Qualified calls in a `macro_rules!` body as Rust paths (`$crate::a::f(` -> `<krate>::a::f`).
/// rust-analyzer emits no occurrences inside macro bodies.
/// ponytail: paths only; `.method()` calls in a macro need types and stay invisible.
pub fn macro_body_calls(body: &str, krate: &str) -> Vec<String> {
    MACRO_CALL
        .captures_iter(body)
        .map(|c| {
            let head = match &c[1] {
                "$crate" | "crate" => krate,
                other => other,
            };
            format!("{head}{}", &c[2])
        })
        .collect()
}

static MACRO_PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(\w+)\s*:\s*\w+").unwrap());
static MACRO_FN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bfn\s+\$(\w+)").unwrap());
static PLAIN_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[^.\w$:])([a-z_][a-z0-9_]*)\s*\(").unwrap());
const NOT_CALLS: &[&str] = &["if", "while", "match", "return", "for", "in", "as", "move", "async", "await", "let", "loop", "unsafe", "where", "impl"];

/// What a `macro_rules!` generates from ident arguments: its first arm's parameters in order, and for every
/// `fn $param` item in its body the calls that item makes (`name` unqualified, or a crate-resolved path).
#[derive(Debug, Default, PartialEq)]
pub struct MacroItems {
    pub params: Vec<String>,
    pub items: Vec<(String, Vec<String>)>,
}

pub fn macro_fn_items(body: &str, krate: &str) -> MacroItems {
    let matcher = body.split("=>").next().unwrap_or("");
    let params = MACRO_PARAM.captures_iter(matcher).map(|c| c[1].to_string()).collect();
    let starts: Vec<(usize, String)> = MACRO_FN.captures_iter(body).map(|c| (c.get(0).map_or(0, |m| m.end()), c[1].to_string())).collect();
    let items = starts.iter().enumerate().map(|(i, (at, param))| {
        let chunk = &body[*at..starts.get(i + 1).map_or(body.len(), |n| n.0)];
        let mut calls: Vec<String> = PLAIN_CALL.captures_iter(chunk).map(|c| c[1].to_string()).filter(|n| !NOT_CALLS.contains(&n.as_str())).collect();
        calls.extend(macro_body_calls(chunk, krate));
        calls.sort();
        calls.dedup();
        (param.clone(), calls)
    }).collect();
    MacroItems { params, items }
}

/// The arguments of the macro invocation whose name starts at `lines[line][col..]`, split at top-level commas,
/// and the line its closing delimiter is on. None when no delimited invocation starts there.
pub fn macro_invocation(lines: &[&str], line: usize, col: usize) -> Option<(Vec<String>, usize)> {
    let first = lines.get(line)?.get(col..)?;
    let bang = first.find('!')?;
    let mut stream = lines.iter().enumerate().skip(line).take(400).flat_map(move |(n, l)| {
        let text = if n == line { &first[bang + 1..] } else { *l };
        text.chars().chain(std::iter::once('\n')).map(move |c| (n, c))
    });
    // only whitespace between `!` and the opening delimiter
    let (_, open) = stream.find(|(_, c)| !c.is_whitespace())?;
    if !matches!(open, '(' | '[' | '{') {
        return None;
    }
    let (mut args, mut cur, mut depth, mut quote, mut prev) = (Vec::new(), String::new(), 1usize, false, ' ');
    for (n, ch) in stream {
        let escaped = prev == '\\';
        prev = if escaped { ' ' } else { ch };
        if quote {
            quote = !(ch == '"' && !escaped);
            cur.push(ch);
            continue;
        }
        match ch {
            '"' => quote = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    let last = cur.trim();
                    if !last.is_empty() {
                        args.push(last.to_string());
                    }
                    return Some((args, n));
                }
            }
            ',' if depth == 1 => {
                args.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(if ch == '\n' { ' ' } else { ch });
    }
    None
}

pub static MOD_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;").unwrap());

static CFG_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#\[cfg\((.*?)\)\]").unwrap());
static NOT_TEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"not\(\s*test\s*\)").unwrap());
static TEST_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\btest\b").unwrap());

/// `#[cfg(test)]`, `#[cfg(all(test, not(target_arch = "wasm32")))]`, but not `#[cfg(not(test))]`.
pub fn is_cfg_test(attrs: &str) -> bool {
    CFG_ATTR.captures_iter(attrs).any(|c| TEST_WORD.is_match(&NOT_TEST.replace_all(&c[1], "")))
}

/// The conditions of every `#[cfg(...)]` in `attrs`, as written.
pub fn cfg_conditions(attrs: &str) -> Vec<String> {
    CFG_ATTR.captures_iter(attrs).map(|c| c[1].trim().to_string()).collect()
}

static FEATURE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^feature\s*=\s*"([^"]+)"$"#).unwrap());

/// Cargo features a cfg condition requires: `feature = "x"` and the conjuncts of `all(...)`.
/// `any(...)` and `not(...)` require none of theirs (the item may build without each).
pub fn required_features(cond: &str) -> Vec<String> {
    let cond = cond.trim();
    if let Some(c) = FEATURE.captures(cond) {
        return vec![c[1].to_string()];
    }
    let Some(inner) = cond.strip_prefix("all(").and_then(|r| r.strip_suffix(')')) else {
        return Vec::new();
    };
    let (mut parts, mut depth, mut start) = (Vec::new(), 0i32, 0);
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts.into_iter().flat_map(required_features).collect()
}

/// `#[...]` attributes / `@decorator` lines directly above a definition (comments skipped).
pub fn leading_attrs(lines: &[&str], line: usize) -> String {
    let mut out = Vec::new();
    let mut i = line;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        let attr_tail = !out.is_empty() && t.ends_with(")]");
        let attr = t.starts_with("#[") || t.starts_with('@') || attr_tail;
        if attr || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') || t.starts_with("# ") {
            if attr {
                out.push(t);
            }
            continue;
        }
        break;
    }
    out.reverse();
    out.join(" ")
}

#[derive(Clone, Debug)]
pub struct ImplBlock {
    pub line: i32, // 0-based
    pub end: (i32, i32),
    pub signature: String,
    /// end of `impl<...>` generics, position of the impl's own `for`, start of `{`
    pub gen_end: (i32, i32),
    pub for_pos: Option<(i32, i32)>,
    pub hdr_end: (i32, i32),
}

/// impl blocks from Rust source (rust-analyzer emits no definition for them): header split at
/// `for`, span by brace balance.
/// ponytail: brace counting on string/comment-blanked lines; a raw string with unbalanced braces
/// mis-sizes one span (spans only steer reference ownership).
pub fn scan_impls(lines: &[&str]) -> Vec<ImplBlock> {
    let clean: Vec<Vec<char>> = lines
        .iter()
        .map(|l| STRIP_RE.replace_all(l, |c: &regex::Captures| " ".repeat(c[0].chars().count())).chars().collect())
        .collect();
    let mut out = Vec::new();
    for (i, ln) in clean.iter().enumerate() {
        let s: String = ln.iter().collect();
        let Some(m) = IMPL_RE.find(&s) else { continue };
        let start_col = s[..m.end()].chars().count();
        let mut chars: Vec<((i32, i32), char)> = Vec::new();
        for (li, row) in clean.iter().enumerate().skip(i).take(30) {
            for (ci, &ch) in row.iter().chain(std::iter::once(&' ')).enumerate() {
                if li > i || ci >= start_col {
                    chars.push(((li as i32, ci as i32), ch));
                }
            }
        }
        let mut k = 0;
        while k < chars.len() && chars[k].1 == ' ' {
            k += 1;
        }
        let mut angle = 0i32;
        if k < chars.len() && chars[k].1 == '<' {
            while k < chars.len() {
                let (ch, prev) = (chars[k].1, if k > 0 { chars[k - 1].1 } else { ' ' });
                angle += i32::from(ch == '<') - i32::from(ch == '>' && prev != '-');
                k += 1;
                if angle == 0 {
                    break;
                }
            }
        }
        let gen_end = chars.get(k).map_or((i as i32, start_col as i32), |c| c.0);
        let text: String = chars[k.min(chars.len())..].iter().map(|c| c.1).collect();
        let text: Vec<char> = text.chars().collect();
        let (mut for_pos, mut after_where, mut hdr_end) = (None, false, None);
        let mut j = k;
        while j < chars.len() {
            let (pos, ch) = chars[j];
            let prev = if j > 0 { chars[j - 1].1 } else { ' ' };
            angle += i32::from(ch == '<') - i32::from(ch == '>' && prev != '-');
            if angle == 0 {
                let word: String = text[j - k..(j - k + 6).min(text.len())].iter().collect();
                if word.starts_with("where") && !prev.is_alphanumeric() {
                    after_where = true; // a `for` after `where` is an HRTB, not the impl's
                }
                let next = word.chars().nth(3).unwrap_or(' ');
                if for_pos.is_none() && !after_where && word.starts_with("for")
                    && !(prev.is_alphanumeric() || prev == '_')
                    && !(next.is_alphanumeric() || next == '_' || next == '<')
                {
                    for_pos = Some(pos);
                }
                if ch == '{' || ch == ';' {
                    hdr_end = Some((pos, ch));
                    break;
                }
            }
            j += 1;
        }
        let Some(((hl, hc), ch)) = hdr_end else { continue };
        let mut end = (hl, hc + 1);
        if ch == '{' {
            let mut depth = 0i32;
            'outer: for (lj, row) in clean.iter().enumerate().skip(hl as usize) {
                let from = if lj == hl as usize { hc as usize } else { 0 };
                for (cj, &c) in row.iter().enumerate().skip(from) {
                    depth += i32::from(c == '{') - i32::from(c == '}');
                    if depth == 0 {
                        end = (lj as i32, cj as i32 + 1);
                        break 'outer;
                    }
                }
            }
        }
        let header: String = text[..j - k].iter().collect();
        let header = header.split_whitespace().collect::<Vec<_>>().join(" ");
        let header = header.split(" where ").next().unwrap_or("").to_string();
        out.push(ImplBlock {
            line: i as i32,
            end,
            signature: format!("impl {header}").chars().take(300).collect(),
            gen_end,
            for_pos,
            hdr_end: (hl, hc),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macro_generated_fns_map_to_their_calls() {
        let body = r#"macro_rules! route4 {
    ($create:ident, $retrieve:ident, $kind:expr, $c:literal) => {
        #[doc = concat!("HTTP handler for ", $c)]
        pub async fn $create(State(st): State<AppState>, body: Bytes) -> Response {
            create(&st, $kind, &body).await.unwrap_or_else(|e| e.into_response())
        }
        pub async fn $retrieve(Path(id): Path<String>) -> Response {
            if ok(id) { retrieve(&st, $kind, &id).await } else { $crate::policy::deny() }
        }
    };
}"#;
        let m = macro_fn_items(body, "antares_api");
        assert_eq!(m.params, ["create", "retrieve", "kind", "c"]);
        assert_eq!(m.items, [
            ("create".to_string(), vec!["create".to_string()]),
            ("retrieve".to_string(), vec!["antares_api::policy::deny".to_string(), "ok".to_string(), "retrieve".to_string()]),
        ]);
    }

    #[test]
    fn invocation_args_split_at_top_level_commas_only() {
        let lines = ["    route4!(", "        create_sub,", r#"        Kind::Sub, f(a, "x,)"),"#, "        \"C\"", "    );", "x"];
        assert_eq!(macro_invocation(&lines, 0, 4), Some((vec!["create_sub".into(), "Kind::Sub".into(), r#"f(a, "x,)")"#.into(), "\"C\"".into()], 4)));
        assert_eq!(macro_invocation(&["let x = y;"], 0, 0), None);
        assert_eq!(macro_invocation(&["m! { a, b }"], 0, 0), Some((vec!["a".into(), "b".into()], 0)));
    }
    use crate::scip::parse;

    #[test]
    fn paths_per_language() {
        let p = |s: &str| display_path(Lang::from_scheme(&parse(s).scheme), &parse(s));
        assert_eq!(p("rust-analyzer cargo antares-store 0.1 impl#[MemStore][Driver]get()."), "antares_store::MemStore::get");
        assert_eq!(p("rust-analyzer cargo api 0.1 batch/parse_batch()."), "api::batch::parse_batch");
        assert_eq!(p("scip-typescript npm web 0.1 src/`api.ts`/HttpSource#list()."), "src/api.ts::HttpSource::list");
        assert_eq!(p("scip-python python w 0.1 app/App#get()."), "app::App::get");
        assert_eq!(p("scip-python python w 0.1 app/__init__:"), "app");
    }

    #[test]
    fn test_files_and_symbols() {
        assert!(Lang::Rust.is_test_file("crates/a/tests/x.rs"));
        assert!(!Lang::Rust.is_test_file("crates/a/src/tests_util.rs"));
        assert!(Lang::Typescript.is_test_file("web/src/api.test.ts"));
        assert!(Lang::Python.is_test_file("worker/test_app.py"));
        assert!(Lang::Rust.is_test_symbol("x", "#[tokio::test]"));
        assert!(Lang::Python.is_test_symbol("test_on_created", ""));
    }

    #[test]
    fn complexity_counts_branches() {
        assert_eq!(Lang::Rust.complexity("if a && b { x? ; } else { for i in v {} }"), 4);
        assert_eq!(Lang::Python.complexity("if a and b:\n  pass\nelif c: pass"), 4);
        assert_eq!(Lang::Typescript.complexity("const x = a ?? b; if (c) {}"), 3);
    }

    #[test]
    fn impl_headers_and_spans() {
        let src = "impl<T: Into<String>> Store for Mem<T> where for<'a> T: Fn(&'a str) {\n    fn f() { \"}\"; }\n}\nimpl Plain {}\n";
        let lines: Vec<&str> = src.lines().collect();
        let got = scan_impls(&lines);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].signature, "impl Store for Mem<T>");
        assert_eq!(got[0].for_pos, Some((0, 28)));
        assert_eq!(got[0].end, (2, 1));
        assert_eq!(got[1].signature, "impl Plain");
        assert_eq!(got[1].for_pos, None);
    }

    #[test]
    fn generics_leave_paths() {
        assert_eq!(strip_generics("Operation<'_>"), "Operation");
        assert_eq!(strip_generics("Page<Vec<T>>"), "Page");
        assert_eq!(strip_generics("Plain"), "Plain");
    }

    #[test]
    fn item_end_and_macro_calls() {
        let lines = ["    async fn a(&self) -> R {", "        let s = \"}\";", "        x.get(1)", "    }", "    fn b();"];
        assert_eq!(rust_item_end(&lines, 0), (3, 5));
        assert_eq!(rust_item_end(&lines, 4), (4, 11));
        let body = "match &$st.policy { Some(e) => $crate::policy::gate(e, &$crate::snapshots::asking_tenant(&$st).await), _ => std::mem::take(x) }";
        assert_eq!(macro_body_calls(body, "antares_api"),
                   ["antares_api::policy::gate", "antares_api::snapshots::asking_tenant", "std::mem::take"]);
    }

    #[test]
    fn cfg_test_predicates() {
        assert!(is_cfg_test("#[cfg(test)]"));
        assert!(is_cfg_test("#[allow(x)] #[cfg(all(test, not(target_arch = \"wasm32\")))]"));
        assert!(!is_cfg_test("#[cfg(not(test))]"));
        assert!(!is_cfg_test("#[cfg(feature = \"testing\")]"));
    }

    #[test]
    fn cfg_features_required_only_by_conjunction() {
        let attrs = r#"#[allow(x)] #[cfg(feature = "odata")] #[cfg(all(feature = "views", not(feature = "x"), any(feature = "a", test)))]"#;
        assert_eq!(cfg_conditions(attrs), [r#"feature = "odata""#, r#"all(feature = "views", not(feature = "x"), any(feature = "a", test))"#]);
        let req: Vec<String> = cfg_conditions(attrs).iter().flat_map(|c| required_features(c)).collect();
        assert_eq!(req, ["odata", "views"]);
        assert!(required_features(r#"not(feature = "odata")"#).is_empty());
        assert!(required_features(r#"any(feature = "a", feature = "b")"#).is_empty());
        assert!(required_features("").is_empty());
    }

    #[test]
    fn index_inputs_per_language() {
        assert!(Lang::Rust.is_index_input("crates/a/src/lib.rs") && Lang::Rust.is_index_input("Cargo.lock"));
        assert!(!Lang::Rust.is_index_input("docs/a.md") && !Lang::Rust.is_index_input("src/app.ts"));
        assert!(Lang::Typescript.is_index_input("www/src/App.tsx") && Lang::Typescript.is_index_input("package.json"));
        assert!(Lang::Python.is_index_input("worker/app.py") && !Lang::Python.is_index_input("worker/app.pyc"));
    }

    #[test]
    fn attrs_above_mod_declaration() {
        let lines = ["mod render;", "", "#[cfg(test)]", "/// doc", "mod tests;"];
        assert_eq!(leading_attrs(&lines, 4), "#[cfg(test)]");
        assert!(MOD_DECL_RE.is_match("pub(crate) mod tests;"));
    }
}

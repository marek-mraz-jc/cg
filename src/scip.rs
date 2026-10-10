//! SCIP index reading and symbol-string parsing (language independent).

use anyhow::{Context, Result, bail};
use protobuf::Message;
use std::path::Path;

pub use scip::types::{Document, Occurrence, SymbolInformation};

pub const ROLE_DEFINITION: i32 = 1;

/// Call `f` for every Document of an index without materialising the whole Index message.
pub fn for_each_document(path: &Path, mut f: impl FnMut(Document) -> Result<()>) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut i = 0;
    while i < data.len() {
        let key = varint(&data, &mut i)?;
        match key & 7 {
            2 => {
                let len = varint(&data, &mut i)? as usize;
                let end = i.checked_add(len).filter(|&e| e <= data.len()).context("truncated SCIP index")?;
                if key >> 3 == 2 {
                    f(Document::parse_from_bytes(&data[i..end])?)?;
                }
                i = end;
            }
            0 => {
                varint(&data, &mut i)?;
            }
            w => bail!("unexpected wire type {w} in SCIP index {}", path.display()),
        }
    }
    Ok(())
}

fn varint(b: &[u8], i: &mut usize) -> Result<u64> {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let c = *b.get(*i).context("truncated varint in SCIP index")?;
        *i += 1;
        v |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            bail!("varint overflow in SCIP index");
        }
    }
}

/// SCIP range -> (start_line, start_char, end_line, end_char).
pub fn range4(r: &[i32]) -> (i32, i32, i32, i32) {
    match r {
        [l, c, e] => (*l, *c, *l, *e),
        [l, c, el, ec, ..] => (*l, *c, *el, *ec),
        _ => (0, 0, 0, 0),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suffix {
    Namespace, // /
    Type,      // #
    Term,      // .
    Meta,      // :
    Macro,     // !
    Method,    // name(disambiguator).
    TypeParam, // [name]
    Param,     // (name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    pub name: String,
    pub suffix: Suffix,
    /// Byte offset where this descriptor starts: `sym[..start]` is its parent symbol.
    pub start: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Symbol {
    pub scheme: String,
    pub package: String,
    pub descs: Vec<Desc>,
}

impl Symbol {
    pub fn last(&self) -> Option<&Desc> {
        self.descs.last()
    }

    pub fn name(&self) -> &str {
        self.last().map_or(self.package.as_str(), |d| d.name.as_str())
    }
}

/// `scheme manager package version descriptors`; two spaces escape a space in the header.
pub fn parse(sym: &str) -> Symbol {
    let b = sym.as_bytes();
    let (mut parts, mut cur, mut i) = (Vec::with_capacity(4), String::new(), 0);
    while parts.len() < 4 && i < b.len() {
        if b[i] == b' ' {
            if b.get(i + 1) == Some(&b' ') {
                cur.push(' ');
                i += 2;
                continue;
            }
            parts.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        let ch = sym[i..].chars().next().unwrap_or(' ');
        cur.push(ch);
        i += ch.len_utf8();
    }
    let mut descs = Vec::new();
    while i < b.len() {
        let start = i;
        if b[i] == b'[' || b[i] == b'(' {
            let (name, j) = read_name(sym, i + 1);
            let suffix = if b[i] == b'[' { Suffix::TypeParam } else { Suffix::Param };
            descs.push(Desc { name, suffix, start });
            i = j + 1; // closing bracket
            continue;
        }
        let (name, j) = read_name(sym, i);
        i = j;
        if i >= b.len() {
            descs.push(Desc { name, suffix: Suffix::Term, start });
            break;
        }
        let suffix = match b[i] {
            b'(' => {
                i = sym[i..].find(')').map_or(b.len(), |k| i + k + 1);
                Suffix::Method
            }
            b'/' => Suffix::Namespace,
            b'#' => Suffix::Type,
            b':' => Suffix::Meta,
            b'!' => Suffix::Macro,
            _ => Suffix::Term,
        };
        i += 1;
        descs.push(Desc { name, suffix, start });
    }
    let scheme = parts.first().cloned().unwrap_or_default();
    let package = parts.get(2).cloned().unwrap_or_default();
    Symbol { scheme, package, descs }
}

/// One descriptor name at `s[i..]`: a plain identifier or a backtick-quoted string (`` ` `` doubled).
fn read_name(s: &str, i: usize) -> (String, usize) {
    let b = s.as_bytes();
    if b.get(i) == Some(&b'`') {
        let (mut out, mut j) = (String::new(), i + 1);
        while j < b.len() {
            if b[j] == b'`' {
                if b.get(j + 1) == Some(&b'`') {
                    out.push('`');
                    j += 2;
                    continue;
                }
                return (out, j + 1);
            }
            let ch = s[j..].chars().next().unwrap_or('?');
            out.push(ch);
            j += ch.len_utf8();
        }
        return (out, j);
    }
    let mut j = i;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'+' | b'-' | b'$')) {
        j += 1;
    }
    (s[i..j].to_string(), j)
}

/// Same symbol with `@<file>` appended to the version: separates same-named definitions in
/// different files that one indexer gave a single symbol (Rust integration tests, Python scripts).
pub fn with_file_tag(sym: &str, file: &str) -> String {
    let mut parts = sym.splitn(5, ' ');
    let head: Vec<&str> = parts.by_ref().take(4).collect();
    let rest = parts.next();
    if head.len() < 4 {
        return format!("{sym}@{file}");
    }
    let tagged = format!("{}@{}", head[3], file.replace(' ', "  "));
    let mut out = format!("{} {} {} {}", head[0], head[1], head[2], tagged);
    if let Some(r) = rest {
        out.push(' ');
        out.push_str(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(s: &str) -> Vec<(String, Suffix)> {
        parse(s).descs.into_iter().map(|d| (d.name, d.suffix)).collect()
    }

    #[test]
    fn rust_method_in_impl() {
        let s = parse("rust-analyzer cargo antares-store 0.1.2 impl#[MemStore][CurrentStateDriver]get().");
        assert_eq!(s.scheme, "rust-analyzer");
        assert_eq!(s.package, "antares-store");
        assert_eq!(
            shape("rust-analyzer cargo p 0.1 impl#[MemStore][Driver]get()."),
            vec![
                ("impl".into(), Suffix::Type),
                ("MemStore".into(), Suffix::TypeParam),
                ("Driver".into(), Suffix::TypeParam),
                ("get".into(), Suffix::Method)
            ]
        );
    }

    #[test]
    fn typescript_backticked_file() {
        let s = parse("scip-typescript npm poly-web 0.1.0 src/`api.ts`/HttpSource#list().");
        assert_eq!(s.package, "poly-web");
        let names: Vec<_> = s.descs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["src", "api.ts", "HttpSource", "list"]);
        assert_eq!(&"scip-typescript npm poly-web 0.1.0 src/`api.ts`/HttpSource#list()."[..s.descs[3].start],
                   "scip-typescript npm poly-web 0.1.0 src/`api.ts`/HttpSource#");
    }

    #[test]
    fn python_params_meta_and_escapes() {
        assert_eq!(
            shape("scip-python python w 0.1 app/App#get().(path)"),
            vec![
                ("app".into(), Suffix::Namespace),
                ("App".into(), Suffix::Type),
                ("get".into(), Suffix::Method),
                ("path".into(), Suffix::Param)
            ]
        );
        assert_eq!(shape("scip-python python w 0.1 app/__init__:"), vec![
            ("app".into(), Suffix::Namespace),
            ("__init__".into(), Suffix::Meta)
        ]);
        assert_eq!(parse("a b my  pkg 1 x.").package, "my pkg");
        assert_eq!(shape("a b p 1 `we``ird`."), vec![("we`ird".into(), Suffix::Term)]);
    }

    #[test]
    fn file_tag_keeps_descriptors() {
        let t = with_file_tag("rust-analyzer cargo api 0.1.2 send().", "tests/a b.rs");
        assert_eq!(t, "rust-analyzer cargo api 0.1.2@tests/a  b.rs send().");
        assert_eq!(parse(&t).package, "api");
        assert_eq!(parse(&t).name(), "send");
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("cg-scip-trunc");
        std::fs::write(&dir, [0x12, 0x80]).unwrap();
        assert!(for_each_document(&dir, |_| Ok(())).is_err());
    }
}

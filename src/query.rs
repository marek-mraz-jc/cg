//! `cg q` / `cg schema`: many Cypher statements per call, compact tables, staleness and typo hints.

use anyhow::{Context, Result, bail};
use lbug::{Connection, Database, SystemConfig, Value};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

pub const NOTES: &str = "\
Conventions
- Query by name / path, not id. path: Rust `crate::module::Type::method`, TypeScript
  `web/src/api.ts::Class::method` (repo-relative file), Python `module::Class::method`. lang: rust | typescript | python.
  Rust paths carry no generic args (`Page::new`). When a type has the same method from two impls, the trait
  impl's path reads `crate::<Type as Trait>::method`; trait_name / self_type hold the parts.
- kind: Function Method StaticMethod TraitMethod Constructor Macro Struct Enum EnumMember Field Trait Class
  Interface TypeAlias Constant Module Impl; external=true marks dependency/std symbols (no file, no outgoing edges).
  Dependencies named in `cg build --deps a,b` (or `deps = [...]` in .cg/config.toml) are indexed too: their
  symbols have external=false, a file under ~/.cache/cg/deps, and Package.workspace=false for their package.
- CALLS = a callable used inside a callable body (calls, fn pointers, JSX components, macro invocations).
  Dynamic dispatch lands on the trait/interface method; static calls (`Trait::m(&x)`, concrete receivers)
  land on the impl method. All callers of a trait method: (c)-[:CALLS]->()-[:IMPLEMENTS*0..1]->(t).
  Calls written inside a Rust macro_rules! body count as calls of the macro AND of every macro caller.
- DISPATCHES: trait/interface/base method -> every implementation or override (IMPLEMENTS reversed, methods
  only). Reachability through dynamic dispatch: (h)-[:CALLS|DISPATCHES*1..6]->(x). Plain [:CALLS*] stops at
  the trait method and silently misses the impl bodies.
- REFERENCES = any other use (types, fields, constants). IMPORTS = module-level uses (`use`, `import`, module code).
- IMPLEMENTS: type->trait/interface/base class and method->the method it implements or overrides.
  HAS_METHOD: type -> its methods. CONTAINS: package->file, module->item, type/impl->member.
- Service links come from the project's .cg/links/*.toml rules: (Symbol|File)-[:PROVIDES|USES]->(Channel {channel, key}).
  LINKS_TO is the resolved cross-service edge user -> provider (client call -> route handler, publisher -> subscriber),
  so [:CALLS|LINKS_TO*1..6] follows a request across languages. Channel.providers / users count endpoints.
- Channel keys are normalised: path params of any framework become {}, HTTP keys read \"VERB /path\".
- is_test: test files, test functions, everything inside cfg(test) modules / test classes.
- cfg: the #[cfg(...)] conditions of the item and every enclosing module, joined by \" && \" (Rust).
  features: cargo features those conditions require, comma-separated (`feature = \"x\"` and all(...) conjuncts;
  not(...)/any(...) require none). '' = compiled in every build. Crate-level gating (optional crates,
  required-features, #![cfg] at a file top) is not in it: check Cargo.toml.
  cc = approximate branch count. Bound variable-length paths; output stops at --max-rows rows.
Blind spots: the graph has no edge for these, so check with GREP before answering \"none\" or \"all\"
- `.method()` calls inside a macro_rules! body; derived impls (#[derive]) have no source or callers.
- calls through closures or fn pointers stored in fields / maps (only the place that names the fn has an edge).
- routes / subjects built at run time, unless the project pins them in .cg/links ([[endpoint]]).
- files no indexer covers (YAML, SQL, shell, Markdown) have File nodes only via link rules: GREP searches them.
Every truncation, skipped file and stale file is printed; a result without such a line is complete for what
the graph models.
Workflow
- Use the graph for relations (who calls / implements / reaches / serves what, what a change touches); it
  answers in milliseconds where reading would take many files. Then read the code you will change (SHOW, or
  open the file): the graph says where to look, not what the code does.
- Ask everything a step needs in ONE call: several -e, or `cg q - <<'EOF'` with one statement per line.
- `cg build --reuse` is safe: an index whose sources changed since it was written is rebuilt. STALE in the
  header lists files edited since the build; rebuild before trusting answers about them.";

pub const EXAMPLES: &str = "\
Examples (one `cg q` call runs them all; each statement ends with ';' at the end of a line)
Besides Cypher a batch takes two statements, so one call can answer AND verify:
  SHOW <path|name|file|file:a-b>, ...  source of symbols / files / ranges (5 symbols per name, 150 lines each)
  GREP <regex> [IN <glob>, ...]    text in all git-tracked files; each hit names its enclosing symbol, test and comment
                                   flags; the totals line counts distinct non-test enclosing symbols
// read the code behind an answer
SHOW poly_server::store::MemStore::put, on_created;
// text the graph has no edge for (macro bodies, strings, config keys), with owners
GREP publish\\( IN worker/**/*.py;
// callers of a function
MATCH (c:Symbol)-[r:CALLS]->(f:Symbol {name:'create_entity'}) RETURN c.path, c.lang, c.file, r.line;
// every caller of a trait method, dynamic and static dispatch
MATCH (c:Symbol)-[:CALLS]->(m:Symbol)-[:IMPLEMENTS*0..1]->(t:Symbol {path:'poly_server::store::Store::get'}) RETURN DISTINCT c.path, m.path;
// implementations of a trait / interface / base class
MATCH (t:Symbol)-[:IMPLEMENTS]->(i:Symbol {name:'Store'}) RETURN t.path, t.lang, t.file;
// UI component -> backend handler -> everything downstream, across languages
MATCH p=(u:Symbol {name:'EntityList'})-[:CALLS|LINKS_TO*1..6]->(h:Symbol) WHERE h.lang <> u.lang RETURN p LIMIT 10;
// who calls this HTTP route / subscribes to this subject
MATCH (a:Symbol)-[l:LINKS_TO]->(b:Symbol) RETURN l.channel, l.key, a.path, b.path ORDER BY l.key;
// client calls with no server, routes nobody calls
MATCH (c:Channel) WHERE c.providers = 0 OR c.users = 0 RETURN c.channel, c.key, c.providers, c.users;
// everything a handler can reach, through trait objects / overrides too
MATCH (h:Symbol {name:'create_entity'})-[:CALLS|DISPATCHES*1..4]->(x:Symbol) WHERE NOT x.external RETURN DISTINCT x.path LIMIT 50;
// blast radius: non-test code reaching a function within 3 hops
MATCH (c:Symbol)-[:CALLS*1..3]->(f:Symbol {name:'put'}) WHERE NOT c.is_test RETURN DISTINCT c.path LIMIT 50;
// tests that exercise a function
MATCH (t:Symbol {is_test:true})-[:CALLS*1..3]->(f:Symbol {name:'on_created'}) RETURN DISTINCT t.path;
// methods of a type
MATCH (t:Symbol {name:'MemStore'})-[:HAS_METHOD]->(m) RETURN m.name, m.signature;
// most complex functions with churn
MATCH (f:File)-[:DEFINES]->(s:Symbol) WHERE s.kind IN ['Function','Method'] AND NOT s.is_test
RETURN s.path, s.cc, s.loc, f.commits_1y ORDER BY s.cc DESC LIMIT 10;
// package dependencies inside the workspace
MATCH (a:Package {workspace:true})-[d:DEPENDS_ON]->(b:Package {workspace:true}) RETURN a.name, d.kind, b.name;
// what a cargo feature adds, and feature-only code that reaches a given crate (layering check)
MATCH (s:Symbol) WHERE s.features CONTAINS 'odata' AND NOT s.is_test RETURN s.kind, count(*);
MATCH (s:Symbol)-[:CALLS*1..3]->(x:Symbol) WHERE s.features <> '' AND x.package = 'antares-store' RETURN DISTINCT s.path, s.features LIMIT 20;
// invariant as a query that must return no rows: route handlers that never reach the policy gate
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel {channel:'http'}) WHERE NOT EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..6]->(g:Symbol {name:'gate'}) } RETURN c.key, h.path;
// inside a dependency indexed with --deps: what our code reaches in it
MATCH (c:Symbol)-[:CALLS]->(f:Symbol {package:'serde_json'})-[:CALLS*1..2]->(g:Symbol {package:'serde_json'}) WHERE NOT c.is_test AND NOT f.external RETURN f.path, count(DISTINCT g) ORDER BY count(DISTINCT g) DESC LIMIT 10;";

pub fn db_file(db: &Path) -> PathBuf {
    if db.is_dir() { db.join("graph.lbug") } else { db.to_path_buf() }
}

pub fn open(db: &Path) -> Result<Database> {
    let f = db_file(db);
    if !f.exists() {
        bail!("no graph at {}; run: cg build", f.display());
    }
    Database::new(&f, SystemConfig::default().read_only(true)).with_context(|| format!("opening {}", f.display()))
}

pub fn rows(c: &Connection, q: &str, params: Vec<(&str, Value)>) -> Result<Vec<Vec<Value>>> {
    if params.is_empty() {
        return Ok(c.query(q)?.collect());
    }
    let mut stmt = c.prepare(q)?;
    Ok(c.execute(&mut stmt, params)?.collect())
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null(_) => String::new(),
        other => other.to_string(),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        Value::Int32(i) => (*i).into(),
        Value::UInt64(i) => *i as i64,
        Value::Int128(i) => *i as i64,
        _ => 0,
    }
}

/// Header line: build time and commit, plus indexed files edited since (mtime, then content hash).
pub fn staleness(c: &Connection) -> Result<String> {
    let meta: HashMap<String, String> =
        rows(c, "MATCH (m:Meta) RETURN m.key, m.value", vec![])?.iter().map(|r| (s(&r[0]), s(&r[1]))).collect();
    let repo = PathBuf::from(meta.get("repo").cloned().unwrap_or_default());
    let files: Vec<(String, f64, String)> = rows(c, "MATCH (f:File) RETURN f.path, f.mtime, f.hash", vec![])?
        .into_iter()
        .map(|r| (s(&r[0]), if let Value::Double(d) = r[1] { d } else { 0.0 }, s(&r[2])))
        .collect();
    // stat is I/O-bound: threads hide network / VM filesystem latency on big repos
    let chunk = files.len().div_ceil(32).max(1);
    let mut changed: Vec<String> = std::thread::scope(|sc| {
        let handles: Vec<_> = files.chunks(chunk).map(|part| {
            let repo = &repo;
            sc.spawn(move || {
                part.iter().filter_map(|(p, mtime, hash)| {
                    let full = repo.join(p);
                    let Ok(m) = std::fs::metadata(&full) else { return Some(format!("{p} (deleted)")) };
                    let same_time = m.modified().ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .is_some_and(|d| d.as_secs_f64() == *mtime);
                    if same_time {
                        return None;
                    }
                    let now = crate::build::file_stamp(&full).map(|x| x.1).unwrap_or_default();
                    (now != *hash).then(|| p.clone())
                }).collect::<Vec<_>>()
            })
        }).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    changed.sort();
    let commit: String = meta.get("git_commit").map(|c| c.chars().take(10).collect()).unwrap_or_default();
    let mut line = format!("graph: built {} @ {commit} [{}]", meta.get("built_at").map_or("?", String::as_str), meta.get("indexes").map_or("", String::as_str));
    if !changed.is_empty() {
        line += &format!(" | STALE: {} files edited since build (rebuild: cg build): {}", changed.len(), changed.iter().take(8).cloned().collect::<Vec<_>>().join(", "));
    }
    Ok(line)
}

static QSPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r";[ \t]*(?:\n|$)").unwrap());
static LITERAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)""#).unwrap());

/// Statements end with ';' at line end; leading `//` or `--` lines label the statement.
pub fn split_queries(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in QSPLIT.split(text) {
        let (mut label, mut body) = (Vec::new(), Vec::new());
        for ln in chunk.lines() {
            let t = ln.trim();
            if (t.starts_with("//") || t.starts_with("--")) && body.is_empty() {
                label.push(t.trim_start_matches(['/', '-', ' ']));
            } else if !t.is_empty() {
                body.push(ln);
            }
        }
        if !body.is_empty() {
            out.push((label.join(" "), body.join("\n")));
        }
    }
    out
}

fn prop<'a>(props: &'a [(String, Value)], keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| props.iter().find(|(n, v)| n == k && !matches!(v, Value::Null(_)) && !s(v).is_empty()).map(|(_, v)| v))
}

pub fn fmt(v: &Value) -> String {
    let out = match v {
        Value::Node(n) => {
            let shown = prop(n.get_properties(), &["path", "name", "key", "id"]).map(s).unwrap_or_default();
            format!("{}:{shown}", n.get_label_name())
        }
        Value::Rel(r) => r.get_label_name().clone(),
        Value::RecursiveRel { nodes, rels } => {
            let by_id: HashMap<String, String> = nodes.iter().map(|n| (format!("{:?}", n.get_node_id()), fmt(&Value::Node(n.clone())))).collect();
            let name = |id: &lbug::InternalID| by_id.get(&format!("{id:?}")).cloned().unwrap_or_else(|| "?".into());
            match rels.first() {
                None => nodes.iter().map(|n| fmt(&Value::Node(n.clone()))).collect::<Vec<_>>().join(" "),
                Some(first) => {
                    let mut p = name(first.get_src_node());
                    for r in rels {
                        p += &format!(" -{}-> {}", r.get_label_name(), name(r.get_dst_node()));
                    }
                    p
                }
            }
        }
        Value::List(_, xs) | Value::Array(_, xs) => format!("[{}]", xs.iter().map(fmt).collect::<Vec<_>>().join(", ")),
        other => s(other),
    };
    if out.chars().count() > 220 { out.chars().take(217).collect::<String>() + "..." } else { out }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Literals in an empty-result query that match nothing, with similar symbol names.
pub fn hint_empty(c: &Connection, q: &str) -> Result<Vec<String>> {
    let mut hints = Vec::new();
    for m in LITERAL.captures_iter(q) {
        let lit = m.get(1).or(m.get(2)).map_or("", |x| x.as_str());
        if lit.len() < 3 || lit == "true" || lit == "false" {
            continue;
        }
        let hit = |q: &str| -> Result<bool> {
            Ok(rows(c, q, vec![("l", Value::String(lit.into()))])?.first().is_some_and(|r| int(&r[0]) > 0))
        };
        if hit("MATCH (s:Symbol) WHERE s.name=$l OR s.path=$l OR s.kind=$l OR s.package=$l OR s.file=$l OR s.lang=$l RETURN count(*)")?
            || hit("MATCH (c:Channel) WHERE c.key=$l OR c.channel=$l RETURN count(*)")?
            || hit("MATCH (p:Package) WHERE p.name=$l RETURN count(*)")?
        {
            continue;
        }
        let frag: String = lit.rsplit("::").next().unwrap_or(lit).chars().filter(|ch| ch.is_alphanumeric() || *ch == '_').collect::<String>().to_lowercase();
        let mut near: Vec<String> = if frag.is_empty() {
            vec![]
        } else {
            rows(c, "MATCH (s:Symbol) WHERE lower(s.name) CONTAINS $f RETURN DISTINCT s.name LIMIT 6", vec![("f", Value::String(frag.clone()))])?
                .iter().map(|r| s(&r[0])).collect()
        };
        if near.len() < 6 && frag.chars().count() >= 3 {
            // typos: edit distance among names sharing the first three letters
            let p: String = frag.chars().take(3).collect();
            let pool = rows(c, "MATCH (s:Symbol) WHERE lower(s.name) STARTS WITH $p RETURN DISTINCT s.name LIMIT 5000", vec![("p", Value::String(p))])?;
            let mut scored: Vec<(usize, String)> = pool.iter().map(|r| s(&r[0])).map(|n| (levenshtein(&frag, &n.to_lowercase()), n))
                .filter(|(d, n)| *d * 4 <= n.len().max(frag.len()) && !near.contains(n)).collect();
            scored.sort();
            near.extend(scored.into_iter().take(6).map(|x| x.1));
        }
        let mut h = format!("'{lit}' matches no Symbol name/path/kind/package/file, Channel key or Package");
        if !near.is_empty() {
            h += &format!("; similar names: {}", near.iter().take(8).cloned().collect::<Vec<_>>().join(", "));
        }
        hints.push(h);
    }
    Ok(hints)
}

pub fn compact_schema(c: &Connection) -> Result<String> {
    let mut parts = Vec::new();
    for t in rows(c, "CALL show_tables() RETURN *", vec![])? {
        let (name, typ) = (s(&t[1]), s(&t[2]));
        if typ == "NODE" {
            let props: Vec<String> = rows(c, &format!("CALL table_info('{name}') RETURN *"), vec![])?.iter().map(|p| s(&p[1])).collect();
            parts.push(format!("{name}({})", props.join(", ")));
        } else if typ == "REL" {
            let conn: Vec<String> = rows(c, &format!("CALL show_connection('{name}') RETURN *"), vec![])?.iter().map(|r| format!("{}->{}", s(&r[0]), s(&r[1]))).collect();
            parts.push(format!("{name}[{}]", conn.join(", ")));
        }
    }
    Ok(parts.join("; "))
}

pub fn run(db: &Path, text: &str, max_rows: usize, timeout_s: u64) -> Result<()> {
    let database = open(db)?;
    let c = Connection::new(&database)?;
    c.set_query_timeout(timeout_s * 1000);
    let text = if text.trim_end().ends_with(';') { text.to_string() } else { format!("{text};") };
    println!("{}", staleness(&c)?);
    let mut grep_cache = None;
    for (n, (label, q)) in split_queries(&text).iter().enumerate() {
        println!("\n### Q{}{}", n + 1, if label.is_empty() { String::new() } else { format!(": {label}") });
        let t = Instant::now();
        let head = q.split_whitespace().next().unwrap_or("").to_uppercase();
        if head == "SHOW" || head == "GREP" {
            let arg = q.trim_start()[4..].trim();
            let done = if head == "SHOW" { show(&c, arg) } else { grep(&c, arg, max_rows, &mut grep_cache) };
            if let Err(e) = done {
                println!("ERROR {e:#}");
            }
            continue;
        }
        let res = match c.query(q) {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                let first = msg.lines().next().unwrap_or("");
                println!("ERROR {first}");
                let l = first.to_lowercase();
                if l.contains("property") || l.contains("table") || l.contains("label") {
                    println!("hint: {}", compact_schema(&c)?);
                }
                continue;
            }
        };
        let cols = res.get_column_names();
        let mut got = Vec::new();
        let mut more = false;
        for row in res {
            if got.len() == max_rows {
                more = true;
                break;
            }
            got.push(row);
        }
        println!("{}{} rows, {} ms", got.len(), if more { "+" } else { "" }, t.elapsed().as_millis());
        if !got.is_empty() {
            println!("{}", cols.join(" | "));
            for row in &got {
                println!("{}", row.iter().map(fmt).collect::<Vec<_>>().join(" | "));
            }
        }
        if more {
            println!("... truncated at {max_rows} rows; add LIMIT / aggregate, or --max-rows");
        }
        if got.is_empty() {
            for h in hint_empty(&c, q)? {
                println!("hint: {h}");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- SHOW / GREP

/// GREP skips bigger files (databases, bundles, dumps) and names them in its totals line.
const GREP_MAX_BYTES: u64 = 2_000_000;
/// Lines printed per symbol by SHOW; the rest is one `SHOW file:a-b` away.
const SHOW_LINES: i64 = 150;
static FILE_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?):(\d+)(?:-(\d+))?$").unwrap());
static GREP_IN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^(.*?)\s+IN\s+(\S+(?:\s*,\s*\S+)*)$").unwrap());

fn repo_root(c: &Connection) -> Result<PathBuf> {
    let r = rows(c, "MATCH (m:Meta {key:'repo'}) RETURN m.value", vec![])?;
    r.first().map(|r| PathBuf::from(s(&r[0]))).context("graph has no repo path; rebuild with cg build")
}

fn print_lines(root: &Path, file: &str, from: i64, to: i64) -> Result<()> {
    let text = std::fs::read_to_string(root.join(file)).with_context(|| format!("reading {file}"))?;
    let lines: Vec<&str> = text.lines().collect();
    let (from, to) = (from.max(1), to.min(lines.len() as i64));
    let stop = to.min(from + SHOW_LINES - 1);
    for n in from..=stop {
        println!("{n:>5}  {}", lines[n as usize - 1]);
    }
    if stop < to {
        println!("  ... {} more lines: SHOW {file}:{}-{to}", to - stop, stop + 1);
    }
    Ok(())
}

/// `SHOW a::b::f, Type, src/x.rs:120-160, src/y.ts`: source of symbols (by path, else by name; 5 per target)
/// or of a file range, so checking a graph answer needs no separate file read.
fn show(c: &Connection, arg: &str) -> Result<()> {
    let root = repo_root(c)?;
    for t in arg.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if root.join(t).is_file() {
            println!("== {t}");
            print_lines(&root, t, 1, i64::MAX)?;
            continue;
        }
        if let Some(m) = FILE_LINES.captures(t).filter(|m| root.join(&m[1]).is_file()) {
            let a: i64 = m[2].parse()?;
            let b: i64 = m.get(3).map_or(Ok(a + 20), |x| x.as_str().parse())?;
            println!("== {}:{a}-{b}", &m[1]);
            print_lines(&root, &m[1], a, b)?;
            continue;
        }
        let q = "MATCH (s:Symbol) WHERE (s.path = $t OR s.name = $t) AND NOT s.external \
                 RETURN s.path, s.kind, s.file, s.line, s.end_line, s.path = $t AS exact ORDER BY exact DESC, s.path LIMIT 6";
        let mut found = rows(c, q, vec![("t", Value::String(t.into()))])?;
        if found.first().is_some_and(|r| matches!(r[5], Value::Bool(true))) {
            found.retain(|r| matches!(r[5], Value::Bool(true)));
        }
        if found.is_empty() {
            println!("== {t}: no symbol with that path or name");
            for h in hint_empty(c, &format!("'{}'", t.replace('\'', "")))? {
                println!("hint: {h}");
            }
        }
        for r in found.iter().take(5) {
            println!("== {} ({}) {}:{}-{}", s(&r[0]), s(&r[1]), s(&r[2]), int(&r[3]), int(&r[4]));
            print_lines(&root, &s(&r[2]), int(&r[3]), int(&r[4]))?;
        }
        if found.len() > 5 {
            println!("  ... more symbols named {t}; use the full path");
        }
    }
    Ok(())
}

/// A line that is only a comment in Rust/TS/Python (`#[attr]` and `#!` are code).
fn is_comment(trimmed: &str) -> bool {
    ["//", "/*", "*"].iter().any(|p| trimmed.starts_with(p))
        || trimmed.starts_with('#') && !trimmed.starts_with("#[") && !trimmed.starts_with("#!")
}

/// Per file: (line, end_line, path, is_test) of every symbol with a source range, for innermost lookups.
type Spans = HashMap<String, Vec<(i64, i64, String, bool)>>;

/// Built once per `cg q` call: symbol spans per file and the file list GREP searches.
pub struct GrepCtx {
    spans: Spans,
    files: Vec<String>,
    source: &'static str,
}

fn grep_ctx(c: &Connection, root: &Path) -> Result<GrepCtx> {
    let mut spans: Spans = HashMap::new();
    for r in rows(c, "MATCH (s:Symbol) WHERE NOT s.external AND s.end_line >= s.line RETURN s.file, s.line, s.end_line, s.path, s.is_test", vec![])? {
        spans.entry(s(&r[0])).or_default().push((int(&r[1]), int(&r[2]), s(&r[3]), matches!(r[4], Value::Bool(true))));
    }
    // every git-tracked file (configs, docs, unindexed code too); a directory walk like `cg build`'s when
    // git lists nothing (no repo, or the tree is untracked inside another one). The graph's own files are
    // always included, so a hit the graph knows about can't be missed.
    let tracked: Vec<String> = std::process::Command::new("git").arg("-C").arg(root).args(["ls-files", "-z"]).output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).split('\0').filter(|f| !f.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    let source = if tracked.is_empty() { "directory walk" } else { "git ls-files" };
    let mut all: std::collections::BTreeSet<String> =
        if tracked.is_empty() { crate::build::walk_files(root).into_iter().collect() } else { tracked.into_iter().collect() };
    all.extend(rows(c, "MATCH (f:File) RETURN f.path", vec![])?.iter().map(|r| s(&r[0])));
    Ok(GrepCtx { spans, files: all.into_iter().collect(), source })
}

/// `GREP regex [IN glob, glob]`: regex over the git-tracked files, each hit with its innermost enclosing
/// symbol and test flag, plus totals; the count of distinct non-test enclosing symbols answers
/// "how many functions use X" for text a graph edge does not carry (macro bodies, strings, comments).
fn grep(c: &Connection, arg: &str, max_rows: usize, ctx: &mut Option<GrepCtx>) -> Result<()> {
    let (pat, globs) = match GREP_IN.captures(arg) {
        Some(m) => (m[1].to_string(), m[2].split(',').map(|g| glob::Pattern::new(g.trim())).collect::<Result<Vec<_>, _>>()?),
        None => (arg.to_string(), vec![]),
    };
    let re = Regex::new(pat.trim()).with_context(|| format!("bad regex {pat}"))?;
    let root = repo_root(c)?;
    let ctx = match ctx {
        Some(x) => x,
        None => ctx.insert(grep_ctx(c, &root)?),
    };
    let (spans, source) = (&ctx.spans, ctx.source);
    let opts = glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };
    let files: Vec<&String> = ctx.files.iter().filter(|f| globs.is_empty() || globs.iter().any(|g| g.matches_with(f, opts))).collect();
    if files.is_empty() {
        bail!("no file matches IN {} under {} ({source}); nothing was searched", globs.iter().map(|g| g.as_str()).collect::<Vec<_>>().join(", "), root.display());
    }
    // reads are I/O-bound (VM / network filesystems): 32 threads, results kept in file order
    let chunk = files.len().div_ceil(32);
    // Err(true): over GREP_MAX_BYTES; Err(false): unreadable or not UTF-8 (binary)
    let found: Vec<Result<Vec<(i64, String)>, bool>> = std::thread::scope(|sc| {
        let handles: Vec<_> = files.chunks(chunk).map(|part| {
            let (re, root) = (&re, &root);
            sc.spawn(move || part.iter().map(|f| {
                let p = root.join(f.as_str());
                if std::fs::metadata(&p).is_ok_and(|m| m.len() > GREP_MAX_BYTES) {
                    return Err(true);
                }
                std::fs::read_to_string(&p).map_err(|_| false).map(|text| {
                    text.lines().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, l)| (i as i64 + 1, l.trim().chars().take(160).collect())).collect()
                })
            }).collect::<Vec<_>>())
        }).collect();
        handles.into_iter().flat_map(|h| h.join().expect("grep thread")).collect()
    });
    let (mut hits, mut nfiles, mut unread, mut big) = (Vec::new(), 0, 0, Vec::new());
    for (f, got) in files.iter().zip(found) {
        let got = match got {
            Ok(g) => g,
            Err(true) => {
                big.push(f.as_str());
                continue;
            }
            Err(false) => {
                unread += 1;
                continue;
            }
        };
        nfiles += usize::from(!got.is_empty());
        for (n, text) in got {
            let owner = spans.get(f.as_str()).and_then(|v| v.iter().filter(|s| s.0 <= n && n <= s.1).min_by_key(|s| s.1 - s.0));
            let (path, test) = owner.map_or((String::new(), false), |o| (o.2.clone(), o.3));
            let comment = is_comment(&text);
            hits.push((format!("{f}:{n}"), path, test, comment, text));
        }
    }
    let owners = |test: bool| hits.iter().filter(|h| !h.1.is_empty() && h.2 == test && !h.3).map(|h| &h.1).collect::<std::collections::BTreeSet<_>>().len();
    let comments = hits.iter().filter(|h| h.3).count();
    println!("{} hits in {nfiles} files ({} searched via {source}, {unread} unreadable/binary, {comments} hits in comments); \
              enclosing symbols outside comments: {} non-test, {} test",
             hits.len(), files.len() - big.len() - unread, owners(false), owners(true));
    if !big.is_empty() {
        println!("NOT searched, over {GREP_MAX_BYTES} bytes: {}", big.join(", "));
    }
    if !hits.is_empty() {
        println!("at | in | test | comment | text");
    }
    let yes = |b: bool| if b { "True" } else { "False" };
    for h in hits.iter().take(max_rows) {
        println!("{} | {} | {} | {} | {}", h.0, h.1, yes(h.2), yes(h.3), h.4);
    }
    if hits.len() > max_rows {
        println!("... truncated at {max_rows} hits; narrow with IN <glob>, or --max-rows");
    }
    Ok(())
}

pub fn schema(db: &Path) -> Result<()> {
    let database = open(db)?;
    let c = Connection::new(&database)?;
    println!("{}", staleness(&c)?);
    let tables = rows(&c, "CALL show_tables() RETURN *", vec![])?;
    for (title, want) in [("Node tables", "NODE"), ("Relationship tables", "REL")] {
        println!("\n{title}");
        for t in tables.iter().filter(|t| s(&t[2]) == want) {
            let name = s(&t[1]);
            let props: Vec<String> = rows(&c, &format!("CALL table_info('{name}') RETURN *"), vec![])?.iter().map(|p| format!("{} {}", s(&p[1]), s(&p[2]))).collect();
            if want == "NODE" {
                let n = int(&rows(&c, &format!("MATCH (x:{name}) RETURN count(*)"), vec![])?[0][0]);
                println!("  {name} [{n}]: {}", props.join(", "));
            } else {
                let conn: Vec<String> = rows(&c, &format!("CALL show_connection('{name}') RETURN *"), vec![])?.iter().map(|r| format!("{}->{}", s(&r[0]), s(&r[1]))).collect();
                let n = int(&rows(&c, &format!("MATCH ()-[r:{name}]->() RETURN count(*)"), vec![])?[0][0]);
                let p = if props.is_empty() { String::new() } else { format!(" {{{}}}", props.join(", ")) };
                println!("  {name} [{n}]: {}{p}", conn.join(", "));
            }
        }
    }
    let kinds = rows(&c, "MATCH (s:Symbol) WHERE NOT s.external RETURN s.lang, s.kind, count(*) AS n ORDER BY s.lang, n DESC", vec![])?;
    let mut by_lang: Vec<(String, Vec<String>)> = Vec::new();
    for k in kinds {
        let lang = s(&k[0]);
        if by_lang.last().is_none_or(|(l, _)| *l != lang) {
            by_lang.push((lang.clone(), vec![]));
        }
        by_lang.last_mut().expect("pushed").1.push(format!("{}={}", s(&k[1]), int(&k[2])));
    }
    println!();
    for (lang, ks) in by_lang {
        println!("Symbol.kind {lang}: {}", ks.join(", "));
    }
    let pk = rows(&c, "MATCH (p:Package {workspace:true}) RETURN p.lang, p.name ORDER BY p.lang, p.name", vec![])?;
    println!("Workspace packages: {}", pk.iter().map(|r| format!("{} ({})", s(&r[1]), s(&r[0]))).collect::<Vec<_>>().join(", "));
    let ch = rows(&c, "MATCH (c:Channel) RETURN c.channel, count(*), sum(c.providers), sum(c.users) ORDER BY c.channel", vec![])?;
    if !ch.is_empty() {
        println!("Channels: {}", ch.iter().map(|r| format!("{} ({} keys, {} providers, {} users)", s(&r[0]), int(&r[1]), int(&r[2]), int(&r[3]))).collect::<Vec<_>>().join(", "));
    }
    println!("\n{NOTES}\n\n{EXAMPLES}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_line_final_semicolons_and_labels() {
        let q = split_queries("// callers\nMATCH (a) WHERE a.x = 'a;b' RETURN a;\n-- two\nMATCH (b)\nRETURN b;  \n");
        assert_eq!(q, [("callers".to_string(), "MATCH (a) WHERE a.x = 'a;b' RETURN a".to_string()),
                       ("two".to_string(), "MATCH (b)\nRETURN b".to_string())]);
    }

    #[test]
    fn edit_distance() {
        assert_eq!(levenshtein("create_entiy", "create_entity"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }
}

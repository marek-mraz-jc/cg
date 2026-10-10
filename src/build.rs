//! `cg build`: SCIP indexes (+ package manifests, git churn, link rules) -> LadybugDB graph.

use crate::lang::{self, Lang, is_callable, is_type};
use crate::links::{self, Links, Role};
use crate::scip::{self, Document, ROLE_DEFINITION, Suffix};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

pub const SCHEMA: &[&str] = &[
    "CREATE NODE TABLE Meta(key STRING PRIMARY KEY, value STRING)",
    "CREATE NODE TABLE Package(name STRING PRIMARY KEY, lang STRING, version STRING, workspace BOOLEAN, path STRING)",
    "CREATE NODE TABLE File(path STRING PRIMARY KEY, package STRING, lang STRING, lines INT64, is_test BOOLEAN, \
     commits_1y INT64, last_change STRING, mtime DOUBLE, hash STRING)",
    "CREATE NODE TABLE Symbol(id STRING PRIMARY KEY, name STRING, path STRING, kind STRING, lang STRING, \
     package STRING, external BOOLEAN, file STRING, line INT64, end_line INT64, loc INT64, visibility STRING, \
     is_async BOOLEAN, is_unsafe BOOLEAN, is_test BOOLEAN, trait_name STRING, self_type STRING, \
     signature STRING, attrs STRING, doc STRING, cc INT64)",
    "CREATE NODE TABLE Channel(id STRING PRIMARY KEY, channel STRING, key STRING, providers INT64, users INT64)",
    "CREATE REL TABLE DEPENDS_ON(FROM Package TO Package, kind STRING)",
    "CREATE REL TABLE CONTAINS(FROM Package TO File, FROM Symbol TO Symbol)",
    "CREATE REL TABLE DEFINES(FROM File TO Symbol)",
    "CREATE REL TABLE CALLS(FROM Symbol TO Symbol, count INT64, line INT64)",
    "CREATE REL TABLE REFERENCES(FROM Symbol TO Symbol, count INT64, line INT64)",
    "CREATE REL TABLE IMPORTS(FROM Symbol TO Symbol, line INT64)",
    "CREATE REL TABLE IMPLEMENTS(FROM Symbol TO Symbol, file STRING, line INT64)",
    "CREATE REL TABLE HAS_METHOD(FROM Symbol TO Symbol)",
    "CREATE REL TABLE DISPATCHES(FROM Symbol TO Symbol)",
    "CREATE REL TABLE PROVIDES(FROM Symbol TO Channel, FROM File TO Channel, file STRING, line INT64, rule STRING, raw STRING)",
    "CREATE REL TABLE USES(FROM Symbol TO Channel, FROM File TO Channel, file STRING, line INT64, rule STRING, raw STRING)",
    "CREATE REL TABLE LINKS_TO(FROM Symbol TO Symbol, channel STRING, key STRING, file STRING, line INT64)",
];

// ---------------------------------------------------------------- project config

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub db: Option<String>,
    #[serde(default)]
    pub index: Vec<IndexSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSpec {
    pub lang: Lang,
    /// directory the indexer runs in; SCIP paths are relative to it
    #[serde(default = "dot")]
    pub root: String,
    /// indexer command override; `{out}` and `{name}` are substituted
    pub cmd: Option<String>,
    /// prebuilt index (repo-relative); skips running the indexer
    pub scip: Option<String>,
    /// project name for scip-python (default: root directory name)
    pub name: Option<String>,
}

fn dot() -> String {
    ".".into()
}

pub fn load_config(repo: &Path) -> Result<Config> {
    let p = repo.join(".cg/config.toml");
    match std::fs::read_to_string(&p) {
        Ok(t) => toml::from_str(&t).with_context(|| format!("parsing {}", p.display())),
        Err(_) => Ok(Config::default()),
    }
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git", ".venv", "venv", "__pycache__", "dist", "build", ".cg"];

/// Without `[[index]]` entries: the topmost Cargo.toml / tsconfig.json|package.json /
/// pyproject.toml|setup.py per directory branch, searched two levels deep.
pub fn detect_indexes(repo: &Path) -> Vec<IndexSpec> {
    let mut out: Vec<IndexSpec> = Vec::new();
    let mut dirs = vec![(repo.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let rel = rel_of(repo, &dir);
        let has = |f: &str| dir.join(f).is_file();
        let covered = |out: &[IndexSpec], l: Lang| {
            out.iter().any(|s| s.lang == l && (s.root == "." || rel.starts_with(&format!("{}/", s.root))))
        };
        for (l, found) in [
            (Lang::Rust, has("Cargo.toml")),
            (Lang::Typescript, has("tsconfig.json") || has("package.json")),
            (Lang::Python, has("pyproject.toml") || has("setup.py")),
        ] {
            if found && !covered(&out, l) {
                out.push(IndexSpec { lang: l, root: rel.clone(), cmd: None, scip: None, name: None });
            }
        }
        if depth < 2 {
            let mut subs: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && !SKIP_DIRS.contains(&n)))
                .collect();
            subs.sort();
            subs.reverse(); // stack: keep alphabetical processing order
            dirs.extend(subs.into_iter().map(|p| (p, depth + 1)));
        }
    }
    out
}

fn rel_of(repo: &Path, p: &Path) -> String {
    let r = p.strip_prefix(repo).unwrap_or(p).to_string_lossy().replace('\\', "/");
    if r.is_empty() { ".".into() } else { r }
}

fn join_rel(root: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches("./");
    if root == "." || root.is_empty() { rel.to_string() } else { format!("{}/{rel}", root.trim_end_matches('/')) }
}

// ---------------------------------------------------------------- graph rows

struct Sym {
    id: String,
    name: String,
    path: String,
    kind: &'static str,
    lang: Lang,
    package: String,
    file: String,
    line: i32,
    end_line: i32,
    visibility: &'static str,
    is_async: bool,
    is_unsafe: bool,
    is_test: bool,
    trait_name: String,
    self_type: String,
    signature: String,
    attrs: String,
    doc: String,
    cc: i64,
    span: i32,
    descs: Vec<scip::Desc>,
    imp: Option<lang::ImplBlock>,
    self_id: Option<String>,
    trait_id: Option<String>,
}

struct Ext {
    name: String,
    path: String,
    kind: &'static str,
    lang: &'static str,
    package: String,
}

struct FileRow {
    package: String,
    lang: &'static str,
    lines: usize,
    is_test: bool,
    mtime: f64,
    hash: String,
}

struct Pkg {
    lang: &'static str,
    version: String,
    workspace: bool,
    path: String,
}

enum Owner {
    Sym(String),
    File(String),
}

struct Endpoint {
    rule: usize,
    key: String,
    owner: Owner,
    file: String,
    line: i32,
    raw: String,
    /// innermost symbol around the match: a route registered inside a test is a test endpoint
    site: Option<String>,
}

#[derive(Default)]
struct Graph {
    sym: HashMap<String, Sym>,
    ext: HashMap<String, Ext>,
    files: HashMap<String, FileRow>,
    pkgs: HashMap<String, Pkg>,
    deps: BTreeSet<(String, String, String)>,
    calls: HashMap<(String, String), (i64, i32)>,
    refs: HashMap<(String, String), (i64, i32)>,
    imports: HashMap<(String, String), i32>,
    implements: HashMap<(String, String), (String, i32)>,
    has_method: BTreeSet<(String, String)>,
    impl_refs: HashMap<String, Vec<(Pos, String)>>,
    mod_attrs: HashMap<String, String>,
    /// macro id -> (Rust path called in its body, line)
    macro_calls: Vec<(String, String, i32)>,
    endpoints: Vec<Endpoint>,
}

fn parent_id(s: &Sym) -> Option<String> {
    let cut = if s.kind == "Impl" {
        s.descs.iter().find(|d| d.name == "impl" && d.suffix == Suffix::Type).map_or(0, |d| d.start)
    } else if s.descs.len() > 1 {
        s.descs.last().map_or(0, |d| d.start)
    } else {
        0
    };
    (cut > 0).then(|| s.id[..cut].to_string())
}

impl Graph {
    fn kind_of(&self, id: &str) -> &'static str {
        self.sym.get(id).map_or_else(|| lang::guess_kind(&scip::parse(id).descs), |s| s.kind)
    }

    fn name_of(&self, id: &str) -> String {
        self.sym.get(id).map_or_else(|| scip::parse(id).name().to_string(), |s| s.name.clone())
    }

    /// Make sure `id` is a node: workspace symbols already are, anything else becomes external.
    fn node(&mut self, id: &str) {
        if self.sym.contains_key(id) || self.ext.contains_key(id) {
            return;
        }
        let p = scip::parse(id);
        let lang = Lang::from_scheme(&p.scheme);
        self.ext.insert(id.to_string(), Ext {
            name: p.name().to_string(),
            path: lang::display_path(lang, &p),
            kind: lang::guess_kind(&p.descs),
            lang: lang.map_or("", Lang::name),
            package: p.package.clone(),
        });
    }

    fn package_of(&self, rel: &str) -> String {
        self.pkgs
            .iter()
            .filter(|(_, p)| p.workspace && (p.path == "." || rel.starts_with(&format!("{}/", p.path))))
            .max_by_key(|(n, p)| (p.path.len(), std::cmp::Reverse((*n).clone())))
            .map(|(n, _)| n.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------- build

pub struct Opts {
    pub repo: PathBuf,
    pub db: PathBuf,
    /// reuse SCIP files from a previous build instead of re-running indexers
    pub reuse: bool,
}

pub fn build(opts: &Opts) -> Result<()> {
    let t0 = Instant::now();
    let repo = opts.repo.canonicalize().with_context(|| format!("repo {}", opts.repo.display()))?;
    let cfg = load_config(&repo)?;
    let specs = if cfg.index.is_empty() { detect_indexes(&repo) } else { cfg.index.clone() };
    if specs.is_empty() {
        bail!("no Cargo.toml / package.json / pyproject.toml found; add [[index]] entries to .cg/config.toml");
    }
    let links = Links::load_dir(&repo.join(".cg/links"))?;
    std::fs::create_dir_all(opts.db.join("scip"))?;
    // the graph is a local build artifact: keep `git add -A` in the indexed repo from committing it
    let ignore = opts.db.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }

    let mut indexes: Vec<(IndexSpec, PathBuf)> = Vec::new();
    for spec in &specs {
        match run_indexer(&repo, &opts.db, spec, opts.reuse) {
            Ok(p) => indexes.push((spec.clone(), p)),
            Err(e) => eprintln!("warning: skipping {} index at {}: {e:#}", spec.lang.name(), spec.root),
        }
    }
    if indexes.is_empty() {
        bail!("every indexer failed; see warnings above");
    }

    let mut g = Graph::default();
    for (spec, _) in &indexes {
        if let Err(e) = load_packages(&repo, spec, &mut g) {
            eprintln!("warning: packages of {} ({}): {e:#}", spec.root, spec.lang.name());
        }
    }
    let (churn, last) = git_churn(&repo);
    let commit = sh(&repo, "git", &["rev-parse", "HEAD"]).unwrap_or_default().trim().to_string();

    // pass 1: symbols one indexer defined in several files get split per file
    let t1 = Instant::now();
    let mut def_files: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (spec, path) in &indexes {
        scip::for_each_document(path, |doc| {
            let rel = join_rel(&spec.root, &doc.relative_path);
            for o in &doc.occurrences {
                if o.symbol_roles & ROLE_DEFINITION != 0 && !o.symbol.is_empty() && !o.symbol.starts_with("local ") {
                    def_files.entry(o.symbol.clone()).or_default().insert(rel.clone());
                }
            }
            Ok(())
        })?;
    }
    let dup: HashMap<String, Vec<String>> =
        def_files.into_iter().filter(|(_, f)| f.len() > 1).map(|(s, f)| (s, f.into_iter().collect())).collect();

    let mut seen_files = HashSet::new();
    for (spec, path) in &indexes {
        scip::for_each_document(path, |mut doc| {
            let rel = join_rel(&spec.root, &doc.relative_path);
            split_dups(&mut doc, &rel, &dup);
            seen_files.insert(rel.clone());
            process_document(&repo, &rel, spec, &doc, &mut g, &links)
        })?;
    }
    // link rules can also match files no indexer covers (YAML, .env, shell): endpoints hang off the File
    if !links.rules.is_empty() {
        for rel in walk_files(&repo) {
            if seen_files.contains(&rel) || !links.applies(&rel) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(repo.join(&rel)) else { continue };
            for h in links.extract(&rel, &text) {
                g.endpoints.push(Endpoint { rule: h.rule, key: h.key, owner: Owner::File(rel.clone()), file: rel.clone(), line: h.line + 1, raw: h.raw, site: None });
            }
            if !g.files.contains_key(&rel) {
                g.files.insert(rel.clone(), file_row(&repo, &rel, &text, None, &g));
            }
        }
    }
    println!("  SCIP -> rows {:.1}s ({} symbols, {} endpoints)", t1.elapsed().as_secs_f64(), g.sym.len(), g.endpoints.len());

    finish(&mut g);
    // after finish: pinned paths see the final (disambiguated) paths and test flags
    if !links.fixed.is_empty() {
        let by_path: HashMap<&str, &String> = g.sym.iter().filter(|(_, s)| !s.is_test).map(|(id, s)| (s.path.as_str(), id)).collect();
        let mut pinned = Vec::new();
        for f in &links.fixed {
            let Some(&id) = by_path.get(f.symbol.as_str()) else {
                eprintln!("  warn: [[endpoint]] {} names unknown symbol {}", f.key, f.symbol);
                continue;
            };
            let s = &g.sym[id];
            pinned.push(Endpoint { rule: f.rule, key: f.key.clone(), owner: Owner::Sym(id.clone()), file: s.file.clone(), line: s.line,
                                   raw: format!("[[endpoint]] {}", f.key), site: None });
        }
        g.endpoints.extend(pinned);
    }
    let (channels, provides, uses, links_to) = resolve_links(&g, &links);

    let stats = format!(
        r#"{{"symbols":{},"external_symbols":{},"calls":{},"references":{},"implements":{},"channels":{},"links":{}}}"#,
        g.sym.len(), g.ext.len(), g.calls.len(), g.refs.len(), g.implements.len(), channels.len(), links_to.len()
    );
    let idx_meta: Vec<String> = indexes.iter().map(|(s, _)| format!("{}:{}", s.lang.name(), s.root)).collect();
    let meta = vec![
        ("git_commit".to_string(), commit),
        ("built_at".to_string(), now_iso()),
        ("repo".to_string(), repo.to_string_lossy().into_owned()),
        ("indexes".to_string(), idx_meta.join(", ")),
        ("stats".to_string(), stats.clone()),
    ];
    let t2 = Instant::now();
    write_db(&opts.db, &g, &churn, &last, &meta, &channels, &provides, &uses, &links_to)?;
    println!("  load {:.1}s -> {}\n  {stats}\n  total {:.1}s", t2.elapsed().as_secs_f64(), opts.db.join("graph.lbug").display(), t0.elapsed().as_secs_f64());
    Ok(())
}

fn run_indexer(repo: &Path, db: &Path, spec: &IndexSpec, reuse: bool) -> Result<PathBuf> {
    if let Some(p) = &spec.scip {
        let p = repo.join(p);
        if !p.is_file() {
            bail!("scip file {} not found", p.display());
        }
        return Ok(p);
    }
    let tag: String = spec.root.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let out = db.join("scip").join(format!("{}-{tag}.scip", spec.lang.name()));
    if reuse && out.is_file() {
        return Ok(out);
    }
    let root = repo.join(&spec.root);
    let name = spec.name.clone().unwrap_or_else(|| {
        root.file_name().and_then(|n| n.to_str()).unwrap_or("project").to_string()
    });
    let cmd = spec.cmd.as_deref().unwrap_or(spec.lang.default_cmd())
        .replace("{out}", &shell_quote(&out.to_string_lossy()))
        .replace("{name}", &shell_quote(&name));
    let t = Instant::now();
    println!("{} index ({}) ...", spec.lang.name(), spec.root);
    let res = Command::new("sh").arg("-c").arg(&cmd).current_dir(&root).output().with_context(|| format!("running `{cmd}`"))?;
    if !res.status.success() || !out.is_file() {
        let err = String::from_utf8_lossy(&res.stderr);
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        bail!("`{cmd}` failed ({}): {}", res.status, tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
    }
    println!("  {:.1}s", t.elapsed().as_secs_f64());
    Ok(out)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn sh(dir: &Path, prog: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(prog).args(args).current_dir(dir).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn now_iso() -> String {
    sh(Path::new("."), "date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn git_churn(repo: &Path) -> (HashMap<String, i64>, HashMap<String, String>) {
    let (mut churn, mut last) = (HashMap::new(), HashMap::new());
    let log = sh(repo, "git", &["log", "--since=1.year", "--no-renames", "--name-only", "--format=@%cs", "--relative"]).unwrap_or_default();
    let mut date = String::new();
    for ln in log.lines() {
        if let Some(d) = ln.strip_prefix('@') {
            date = d.to_string();
        } else if !ln.is_empty() {
            *churn.entry(ln.to_string()).or_insert(0) += 1;
            last.entry(ln.to_string()).or_insert_with(|| date.clone());
        }
    }
    (churn, last)
}

/// Repo files minus SKIP_DIRS. Each directory level is read by 32 threads: opendir latency dominates
/// on VM / network filesystems (a serial walk of 3k files took 4 s on a WSL mount, this one well under 1 s).
pub(crate) fn walk_files(repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut level = vec![repo.to_path_buf()];
    while !level.is_empty() {
        let chunk = level.len().div_ceil(32);
        let found: Vec<(Vec<PathBuf>, Vec<String>)> = std::thread::scope(|sc| {
            let handles: Vec<_> = level.chunks(chunk).map(|part| sc.spawn(move || {
                let (mut dirs, mut files) = (Vec::new(), Vec::new());
                for e in part.iter().flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten()) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    match e.file_type() {
                        Ok(t) if t.is_dir() && !SKIP_DIRS.contains(&name.as_str()) => dirs.push(e.path()),
                        Ok(t) if t.is_file() => files.push(rel_of(repo, &e.path())),
                        _ => {}
                    }
                }
                (dirs, files)
            })).collect();
            handles.into_iter().map(|h| h.join().expect("walk thread")).collect()
        });
        level = Vec::new();
        for (dirs, files) in found {
            level.extend(dirs);
            out.extend(files);
        }
    }
    out.sort();
    out
}

fn fnv(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn file_stamp(path: &Path) -> Option<(f64, String)> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    let mtime = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64();
    Some((mtime, fnv(&std::fs::read(path).ok()?)))
}

fn file_row(repo: &Path, rel: &str, text: &str, lang: Option<Lang>, g: &Graph) -> FileRow {
    let (mtime, hash) = file_stamp(&repo.join(rel)).unwrap_or((0.0, fnv(text.as_bytes())));
    FileRow {
        package: g.package_of(rel),
        lang: lang.map_or("", Lang::name),
        lines: text.lines().count(),
        is_test: lang.is_some_and(|l| l.is_test_file(rel)),
        mtime,
        hash,
    }
}

// ---------------------------------------------------------------- packages

fn load_packages(repo: &Path, spec: &IndexSpec, g: &mut Graph) -> Result<()> {
    let root = repo.join(&spec.root);
    match spec.lang {
        Lang::Rust => {
            let out = Command::new("cargo").args(["metadata", "--format-version", "1"]).current_dir(&root).output()?;
            if !out.status.success() {
                bail!("cargo metadata: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
            }
            let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
            let ws: HashSet<&str> = meta["workspace_members"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
            for p in meta["packages"].as_array().into_iter().flatten() {
                let name = p["name"].as_str().unwrap_or_default().to_string();
                let is_ws = p["id"].as_str().is_some_and(|id| ws.contains(id));
                let path = if is_ws {
                    let m = Path::new(p["manifest_path"].as_str().unwrap_or_default());
                    rel_of(repo, m.parent().unwrap_or(m))
                } else {
                    String::new()
                };
                if is_ws {
                    for d in p["dependencies"].as_array().into_iter().flatten() {
                        let kind = d["kind"].as_str().unwrap_or("normal").to_string();
                        g.deps.insert((name.clone(), d["name"].as_str().unwrap_or_default().to_string(), kind));
                    }
                }
                let entry = g.pkgs.entry(name).or_insert(Pkg { lang: "rust", version: String::new(), workspace: false, path: String::new() });
                if is_ws || entry.version.is_empty() {
                    *entry = Pkg { lang: "rust", version: p["version"].as_str().unwrap_or_default().to_string(), workspace: is_ws, path };
                }
            }
        }
        Lang::Typescript => {
            let mut stack = vec![root.clone()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) && !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                        stack.push(e.path());
                    } else if name == "package.json" {
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(e.path())?) else { continue };
                        let pname = v["name"].as_str().map(str::to_string).unwrap_or_else(|| rel_of(repo, &d));
                        for (section, kind) in [("dependencies", "normal"), ("devDependencies", "dev"), ("peerDependencies", "peer")] {
                            for dep in v[section].as_object().into_iter().flatten().map(|(k, _)| k) {
                                g.deps.insert((pname.clone(), dep.clone(), kind.to_string()));
                                g.pkgs.entry(dep.clone()).or_insert(Pkg { lang: "typescript", version: String::new(), workspace: false, path: String::new() });
                            }
                        }
                        g.pkgs.insert(pname, Pkg {
                            lang: "typescript",
                            version: v["version"].as_str().unwrap_or_default().to_string(),
                            workspace: true,
                            path: rel_of(repo, &d),
                        });
                    }
                }
            }
        }
        Lang::Python => {
            let text = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
            let v: toml::Value = toml::from_str(&text).unwrap_or(toml::Value::Table(Default::default()));
            let name = v.get("project").and_then(|p| p.get("name")).and_then(|n| n.as_str()).map(str::to_string)
                .or_else(|| spec.name.clone())
                .unwrap_or_else(|| root.file_name().and_then(|n| n.to_str()).unwrap_or("python").to_string());
            let version = v.get("project").and_then(|p| p.get("version")).and_then(|n| n.as_str()).unwrap_or("").to_string();
            let deps = v.get("project").and_then(|p| p.get("dependencies")).and_then(|d| d.as_array()).cloned().unwrap_or_default();
            for d in deps.iter().filter_map(|d| d.as_str()) {
                let dep: String = d.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
                g.deps.insert((name.clone(), dep.clone(), "normal".into()));
                g.pkgs.entry(dep).or_insert(Pkg { lang: "python", version: String::new(), workspace: false, path: String::new() });
            }
            g.pkgs.insert(name, Pkg { lang: "python", version, workspace: true, path: spec.root.clone() });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- one document

fn split_dups(doc: &mut Document, rel: &str, dup: &HashMap<String, Vec<String>>) {
    if dup.is_empty() {
        return;
    }
    // ponytail: references resolve to the definition sharing the longest path prefix; exact for
    // same-file helpers and tests/common modules, a guess for anything stranger
    let split = |s: &str| -> Option<String> {
        let files = dup.get(s)?;
        let f = if files.iter().any(|f| f == rel) {
            rel.to_string()
        } else {
            files.iter().max_by_key(|f| f.chars().zip(rel.chars()).take_while(|(a, b)| a == b).count())?.clone()
        };
        Some(scip::with_file_tag(s, &f))
    };
    for o in doc.occurrences.iter_mut() {
        if let Some(t) = split(&o.symbol) {
            o.symbol = t;
        }
    }
    for s in doc.symbols.iter_mut() {
        if let Some(t) = split(&s.symbol) {
            s.symbol = t;
        }
    }
}

type Pos = (i32, i32);

struct Span {
    start: Pos,
    end: Pos,
    id: String,
}

fn process_document(repo: &Path, rel: &str, spec: &IndexSpec, doc: &Document, g: &mut Graph, links: &Links) -> Result<()> {
    let lang = spec.lang;
    let text = std::fs::read_to_string(repo.join(rel)).unwrap_or_else(|_| doc.text.clone());
    let lines: Vec<&str> = text.lines().collect();
    let fr = file_row(repo, rel, &text, Some(lang), g);
    let is_test_file = fr.is_test;
    let package = fr.package.clone();
    g.files.insert(rel.to_string(), fr);
    let info: HashMap<&str, &scip::SymbolInformation> = doc.symbols.iter().map(|s| (s.symbol.as_str(), s)).collect();
    let last_line = lines.len().saturating_sub(1) as i32;

    // #[async_trait] gives every async fn of an impl the whole impl as enclosing range
    let mut shared: HashMap<&[i32], usize> = HashMap::new();
    for o in doc.occurrences.iter().filter(|o| o.symbol_roles & ROLE_DEFINITION != 0 && !o.enclosing_range.is_empty()) {
        *shared.entry(o.enclosing_range.as_slice()).or_insert(0) += 1;
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut doc_defs: Vec<String> = Vec::new();
    for o in &doc.occurrences {
        if o.symbol_roles & ROLE_DEFINITION == 0 || o.symbol.is_empty() || o.symbol.starts_with("local ") {
            continue;
        }
        let si = info.get(o.symbol.as_str()).copied();
        let parsed = scip::parse(&o.symbol);
        let Some(kind) = lang::kind(si, &parsed) else { continue };
        let r = scip::range4(&o.range);
        let mut er = if o.enclosing_range.is_empty() { r } else { scip::range4(&o.enclosing_range) };
        if lang == Lang::Rust && is_callable(kind) && shared.get(o.enclosing_range.as_slice()).is_some_and(|&n| n > 1) {
            let end = lang::rust_item_end(&lines, r.0.max(0) as usize);
            er = (r.0, 0, end.0, end.1);
        }
        if lang == Lang::Rust && kind == "Macro" {
            let krate = parsed.package.replace('-', "_");
            for (i, l) in lines.iter().enumerate().take((er.2 + 1).max(0) as usize).skip((r.0 + 1).max(0) as usize) {
                for path in lang::macro_body_calls(l, &krate) {
                    g.macro_calls.push((o.symbol.clone(), path, i as i32 + 1));
                }
            }
        }
        if kind == "Module" && o.enclosing_range.is_empty() && r.0 == 0 && r.1 == 0 {
            er = (0, 0, last_line + 1, 0); // module without a range (scip-python): the whole file
        }
        let span = er.2 - er.0;
        if g.sym.get(&o.symbol).is_some_and(|p| p.span >= span) {
            continue;
        }
        let sig = lang::signature(si);
        let name = parsed.name().to_string();
        let attrs: String = lang::leading_attrs(&lines, r.0.max(0) as usize).chars().take(300).collect();
        let (mut self_type, mut trait_name) = (String::new(), String::new());
        if lang::is_impl(&parsed.descs) {
            let (_, ty, tr, _) = lang::impl_parts(&parsed.descs);
            self_type = ty;
            trait_name = tr.unwrap_or_default();
        } else if lang != Lang::Rust && parsed.descs.len() > 1 && parsed.descs[parsed.descs.len() - 2].suffix == Suffix::Type {
            self_type = parsed.descs[parsed.descs.len() - 2].name.clone();
        }
        let cc = if is_callable(kind) {
            let (a, b) = ((er.0 + 1).max(0) as usize, ((er.2 + 1).max(0) as usize).min(lines.len()));
            lang.complexity(&lines.get(a.min(b)..b).unwrap_or(&[]).join("\n"))
        } else {
            0
        };
        let row = Sym {
            id: o.symbol.clone(),
            // TypeScript paths start with the file path relative to the index root: make it repo-relative
            path: match lang {
                Lang::Typescript if spec.root != "." => format!("{}/{}", spec.root.trim_end_matches('/'), lang::display_path(Some(lang), &parsed)),
                _ => lang::display_path(Some(lang), &parsed),
            },
            kind,
            lang,
            package: if parsed.package.is_empty() { package.clone() } else { parsed.package.clone() },
            file: rel.to_string(),
            line: er.0 + 1,
            end_line: er.2 + 1,
            visibility: lang.visibility(&sig, &name),
            is_async: lang.is_async(&sig),
            is_unsafe: sig.split('(').next().unwrap_or("").contains("unsafe "),
            is_test: is_test_file || lang.is_test_symbol(&name, &attrs),
            trait_name,
            self_type,
            signature: sig.chars().take(300).collect(),
            doc: lang::doc_text(si).chars().take(300).collect(),
            attrs,
            cc,
            span,
            name,
            descs: parsed.descs,
            imp: None,
            self_id: None,
            trait_id: None,
        };
        doc_defs.push(row.id.clone());
        spans.push(Span { start: (er.0, er.1), end: (er.2, er.3), id: row.id.clone() });
        if let Some(si) = si {
            for rl in si.relationships.iter().filter(|rl| rl.is_implementation) {
                g.implements.insert((row.id.clone(), rl.symbol.clone()), (rel.to_string(), row.line));
            }
        }
        g.sym.insert(row.id.clone(), row);
    }

    if lang == Lang::Rust {
        for imp in lang::scan_impls(&lines) {
            // rust-analyzer emits no definition for impl blocks; their id is the prefix of a method's
            let first = doc_defs.iter().filter_map(|id| g.sym.get(id)).filter(|r| {
                r.kind != "Impl" && lang::is_impl(&r.descs) && imp.line < r.line - 1 && r.line - 1 <= imp.end.0
            }).min_by_key(|r| r.line);
            let iid = match first {
                Some(r) => r.id[..r.descs.last().map_or(r.id.len(), |d| d.start)].to_string(),
                None => format!("rust-analyzer cargo {package} impl@{rel}:{}", imp.line + 1),
            };
            if g.sym.contains_key(&iid) {
                continue;
            }
            let parsed = scip::parse(&iid);
            spans.push(Span { start: (imp.line, 0), end: imp.end, id: iid.clone() });
            g.sym.insert(iid.clone(), Sym {
                id: iid.clone(),
                name: imp.signature.clone(),
                path: String::new(),
                kind: "Impl",
                lang,
                package: if parsed.package.is_empty() { package.clone() } else { parsed.package.clone() },
                file: rel.to_string(),
                line: imp.line + 1,
                end_line: imp.end.0 + 1,
                visibility: "",
                is_async: false,
                is_unsafe: imp.signature.starts_with("unsafe"),
                is_test: is_test_file,
                trait_name: String::new(),
                self_type: String::new(),
                signature: imp.signature.clone(),
                attrs: lang::leading_attrs(&lines, imp.line as usize).chars().take(300).collect(),
                doc: String::new(),
                cc: 0,
                span: 0,
                descs: parsed.descs,
                imp: Some(imp),
                self_id: None,
                trait_id: None,
            });
        }
    }

    // innermost enclosing definition for every reference
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut occs: Vec<&scip::Occurrence> = doc.occurrences.iter()
        .filter(|o| !o.symbol.is_empty() && !o.symbol.starts_with("local ") && o.symbol_roles & ROLE_DEFINITION == 0)
        .collect();
    occs.sort_by_key(|o| (o.range.first().copied().unwrap_or(0), o.range.get(1).copied().unwrap_or(0)));
    let mut stack: Vec<&Span> = Vec::new();
    let mut k = 0;
    for o in occs {
        let r = scip::range4(&o.range);
        let p = (r.0, r.1);
        if lang == Lang::Rust && lines.get(r.0 as usize).is_some_and(|l| lang::MOD_DECL_RE.is_match(l)) {
            g.mod_attrs.insert(o.symbol.clone(), lang::leading_attrs(&lines, r.0 as usize));
        }
        while k < spans.len() && spans[k].start <= p {
            while stack.last().is_some_and(|s| s.end <= spans[k].start) {
                stack.pop();
            }
            stack.push(&spans[k]);
            k += 1;
        }
        while stack.last().is_some_and(|s| s.end <= p) {
            stack.pop();
        }
        let Some(owner) = stack.last().map(|s| s.id.clone()) else { continue };
        let okind = g.sym[&owner].kind;
        let tkind = g.kind_of(&o.symbol);
        let key = (owner.clone(), o.symbol.clone());
        let line = r.0 + 1;
        if okind == "Module" {
            g.imports.entry(key).or_insert(line);
        } else if is_callable(okind) && is_callable(tkind) {
            let e = g.calls.entry(key).or_insert((0, line));
            e.0 += 1;
        } else {
            let e = g.refs.entry(key).or_insert((0, line));
            e.0 += 1;
        }
        if okind == "Impl" {
            let hdr = g.sym[&owner].imp.as_ref().map(|i| i.hdr_end);
            if hdr.is_some_and(|h| p < h) {
                g.impl_refs.entry(owner).or_default().push((p, o.symbol.clone()));
            }
        }
    }

    if links.applies(rel) {
        for h in links.extract(rel, &text) {
            let site = innermost(&spans, (h.line, h.col));
            let owner = h.symbol.as_ref()
                .and_then(|(l, c, name)| symbol_at(doc, *l, *c).or_else(|| def_named(g, rel, name)))
                .or_else(|| site.clone())
                .map_or_else(|| Owner::File(rel.to_string()), Owner::Sym);
            g.endpoints.push(Endpoint { rule: h.rule, key: h.key, owner, file: rel.to_string(), line: h.line + 1, raw: h.raw, site });
        }
    }
    Ok(())
}

/// The symbol whose occurrence starts at (line, col): the handler a route names, resolved by SCIP.
fn symbol_at(doc: &Document, line: i32, col: i32) -> Option<String> {
    doc.occurrences.iter().find(|o| {
        let r = scip::range4(&o.range);
        r.0 == line && r.1 <= col && col < r.3.max(r.1 + 1) && !o.symbol.starts_with("local ")
    }).map(|o| o.symbol.clone())
}

fn def_named(g: &Graph, rel: &str, name: &str) -> Option<String> {
    let name = name.rsplit([':', '.']).next().unwrap_or(name);
    let mut hits = g.sym.values().filter(|s| s.file == rel && s.name == name && is_callable(s.kind));
    let first = hits.next()?;
    hits.next().is_none().then(|| first.id.clone())
}

fn innermost(spans: &[Span], p: Pos) -> Option<String> {
    spans.iter().filter(|s| s.start <= p && p < s.end)
        .min_by_key(|s| (s.end.0 - s.start.0, s.end.1 - s.start.1))
        .map(|s| s.id.clone())
}

// ---------------------------------------------------------------- whole-graph passes

fn finish(g: &mut Graph) {
    // `ExampleStore::delete` from two trait impls (or an inherent one next to a trait one): the trait
    // impl methods take Rust's qualified form, `crate::<ExampleStore as TemporalDriver>::delete`
    let mut by_path: HashMap<String, Vec<String>> = HashMap::new();
    for (id, s) in &g.sym {
        if s.lang == Lang::Rust {
            by_path.entry(s.path.clone()).or_default().push(id.clone());
        }
    }
    for id in by_path.into_values().filter(|ids| ids.len() > 1).flatten() {
        let s = g.sym.get_mut(&id).expect("present");
        if s.trait_name.is_empty() || !lang::is_impl(&s.descs) {
            continue;
        }
        if let Some((owner, method)) = s.path.rsplit_once("::") {
            let (prefix, ty) = owner.rsplit_once("::").unwrap_or(("", owner));
            let q = format!("<{ty} as {}>::{method}", s.trait_name);
            s.path = if prefix.is_empty() { q } else { format!("{prefix}::{q}") };
        }
    }
    // macro bodies run inside their callers: macro -> callee, and every caller of the macro -> callee
    if !g.macro_calls.is_empty() {
        let mut by_path: HashMap<&str, Option<&str>> = HashMap::new();
        for s in g.sym.values().filter(|s| is_callable(s.kind)) {
            by_path.entry(s.path.as_str()).and_modify(|v| *v = None).or_insert(Some(s.id.as_str()));
        }
        let resolved: Vec<(String, String, i32)> = g.macro_calls.iter()
            .filter_map(|(m, p, l)| by_path.get(p.as_str()).copied().flatten().map(|t| (m.clone(), t.to_string(), *l)))
            .collect();
        let callers: Vec<(String, String, i32)> = g.calls.iter()
            .filter(|((_, t), _)| resolved.iter().any(|(m, _, _)| m == t))
            .map(|((c, m), (_, l))| (c.clone(), m.clone(), *l))
            .collect();
        for (m, t, l) in &resolved {
            g.calls.entry((m.clone(), t.clone())).or_insert((0, *l)).0 += 1;
            for (c, _, cl) in callers.iter().filter(|(_, cm, _)| cm == m) {
                g.calls.entry((c.clone(), t.clone())).or_insert((0, *cl)).0 += 1;
            }
        }
    }

    // test scope flows down: cfg(test) modules, test classes, files
    let mut order: Vec<String> = g.sym.keys().cloned().collect();
    order.sort_by_key(|id| (g.sym[id].descs.len(), id.clone()));
    for id in &order {
        let parent_test = parent_id(&g.sym[id]).and_then(|p| g.sym.get(&p)).is_some_and(|p| p.is_test);
        let s = &g.sym[id];
        let own = s.kind == "Module"
            && lang::is_cfg_test(&format!("{} {}", s.attrs, g.mod_attrs.get(id).map_or("", String::as_str)));
        if parent_test || own {
            g.sym.get_mut(id).expect("present").is_test = true;
        }
    }

    // Rust impl blocks -> IMPLEMENTS (type->trait, method->trait method) and HAS_METHOD
    let impl_ids: Vec<String> = g.sym.values().filter(|s| s.kind == "Impl").map(|s| s.id.clone()).collect();
    for iid in impl_ids {
        let imp = g.sym[&iid].imp.clone().expect("impl rows carry their header");
        // type references in the header after the generics: trait before `for`, self type after
        let tys: Vec<(Pos, String)> = g.impl_refs.get(&iid).into_iter().flatten()
            .filter(|(p, t)| *p >= imp.gen_end && is_type(g.kind_of(t)))
            .cloned()
            .collect();
        let (tr, slf) = match imp.for_pos {
            Some(f) => (tys.iter().find(|(p, _)| *p < f).map(|x| x.1.clone()), tys.iter().find(|(p, _)| *p > f).map(|x| x.1.clone())),
            None => (None, tys.first().map(|x| x.1.clone())),
        };
        if slf.is_none() && tr.is_none() && !iid.contains("impl#") {
            g.sym.remove(&iid); // impl text inside a macro_rules! body: nothing resolvable
            continue;
        }
        let self_name = slf.as_deref().map(|t| g.name_of(t)).unwrap_or_default();
        let trait_name = tr.as_deref().map(|t| g.name_of(t)).unwrap_or_default();
        let self_path = slf.as_deref().and_then(|t| g.sym.get(t)).map(|s| s.path.clone()).unwrap_or_else(|| self_name.clone());
        if let (Some(s), Some(t)) = (&slf, &tr) {
            let r = &g.sym[&iid];
            g.implements.insert((s.clone(), t.clone()), (r.file.clone(), r.line));
        }
        let r = g.sym.get_mut(&iid).expect("present");
        r.name = if tr.is_some() { format!("impl {trait_name} for {self_name}") } else { format!("impl {self_name}") };
        r.path = if tr.is_some() { format!("{self_path} as {trait_name}") } else { self_path };
        r.self_type = self_name;
        r.trait_name = trait_name;
        r.self_id = slf;
        r.trait_id = tr;
    }
    let methods: Vec<(String, Option<String>)> = g.sym.values()
        .filter(|r| is_callable(r.kind))
        .map(|r| (r.id.clone(), parent_id(r)))
        .collect();
    for (mid, parent) in methods {
        let Some(parent) = parent else { continue };
        let Some(p) = g.sym.get(&parent) else { continue };
        if p.kind == "Impl" {
            let (slf, tr) = (p.self_id.clone(), p.trait_id.clone());
            let (file, line, mname) = { let m = &g.sym[&mid]; (m.file.clone(), m.line, m.name.clone()) };
            if let Some(s) = slf {
                g.has_method.insert((s, mid.clone()));
            }
            if let Some(t) = tr {
                g.implements.insert((mid.clone(), format!("{t}{mname}().")), (file, line));
            }
        } else if is_type(p.kind) {
            g.has_method.insert((parent, mid));
        }
    }
    let targets: Vec<String> = g.calls.keys().chain(g.refs.keys()).map(|k| k.1.clone())
        .chain(g.imports.keys().map(|k| k.1.clone()))
        .chain(g.implements.keys().flat_map(|k| [k.0.clone(), k.1.clone()]))
        .chain(g.has_method.iter().flat_map(|k| [k.0.clone(), k.1.clone()]))
        .collect();
    for t in targets {
        g.node(&t);
    }
}

type ChannelRow = (String, String, String, i64, i64);
type EndpointRow = (String, bool, String, String, i32, String, String); // owner, owner_is_file, channel id, file, line, rule, raw
type LinkRow = (String, String, String, String, String, i32);
type Resolved = (Vec<ChannelRow>, Vec<EndpointRow>, Vec<EndpointRow>, Vec<LinkRow>);

fn resolve_links(g: &Graph, links: &Links) -> Resolved {
    let mut channels: HashMap<String, ChannelRow> = HashMap::new();
    let (mut provides, mut uses) = (Vec::new(), Vec::new());
    // ponytail: test mocks and test clients are dropped, not flagged; add an is_test edge property if
    // "which tests hit this route" becomes a question
    let test = |id: &String| g.sym.get(id).is_some_and(|s| s.is_test);
    let live: Vec<&Endpoint> = g.endpoints.iter()
        .filter(|e| !e.site.as_ref().is_some_and(test)
            && !matches!(&e.owner, Owner::Sym(s) if test(s))
            && !g.files.get(&e.file).is_some_and(|f| f.is_test))
        .collect();
    for &e in &live {
        let rule = &links.rules[e.rule];
        let cid = format!("{}:{}", rule.spec.channel, e.key);
        let c = channels.entry(cid.clone()).or_insert((cid.clone(), rule.spec.channel.clone(), e.key.clone(), 0, 0));
        let (owner, is_file) = match &e.owner {
            Owner::Sym(s) if g.sym.contains_key(s) || g.ext.contains_key(s) => (s.clone(), false),
            Owner::Sym(_) => (e.file.clone(), true),
            Owner::File(f) => (f.clone(), true),
        };
        let row = (owner, is_file, cid, e.file.clone(), e.line, rule.id.clone(), e.raw.chars().take(200).collect());
        match rule.spec.role {
            Role::Provides => {
                c.3 += 1;
                provides.push(row);
            }
            Role::Uses => {
                c.4 += 1;
                uses.push(row);
            }
        }
    }
    // ponytail: every uses x every provides of a channel; index by first segment if this gets slow
    let mut seen = HashSet::new();
    let mut links_to = Vec::new();
    for u in live.iter().filter(|e| links.rules[e.rule].spec.role == Role::Uses) {
        let ur = &links.rules[u.rule].spec;
        let Owner::Sym(from) = &u.owner else { continue };
        for p in live.iter().filter(|e| links.rules[e.rule].spec.role == Role::Provides) {
            let pr = &links.rules[p.rule].spec;
            let Owner::Sym(to) = &p.owner else { continue };
            if pr.channel == ur.channel && links::key_matches(&u.key, &p.key, &ur.sep) && seen.insert((from.clone(), to.clone(), u.key.clone())) {
                links_to.push((from.clone(), to.clone(), ur.channel.clone(), u.key.clone(), u.file.clone(), u.line));
            }
        }
    }
    let mut ch: Vec<ChannelRow> = channels.into_values().collect();
    ch.sort();
    (ch, provides, uses, links_to)
}

// ---------------------------------------------------------------- database

#[allow(clippy::too_many_arguments)]
fn write_db(
    db_dir: &Path, g: &Graph, churn: &HashMap<String, i64>, last: &HashMap<String, String>, meta: &[(String, String)],
    channels: &[ChannelRow], provides: &[EndpointRow], uses: &[EndpointRow], links_to: &[LinkRow],
) -> Result<()> {
    let tmp = db_dir.join(format!("csv-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let mut csvs: Vec<(&str, String, &str)> = Vec::new(); // table, file, extra COPY options
    let mut w = |table: &'static str, name: &str, extra: &'static str, header: &[&str], rows: &mut dyn Iterator<Item = Vec<Cell>>| -> Result<()> {
        let mut s = header.join(",");
        s.push('\n');
        for r in rows {
            let line: Vec<String> = r.into_iter().map(Cell::csv).collect();
            s.push_str(&line.join(","));
            s.push('\n');
        }
        let p = tmp.join(name);
        std::fs::write(&p, s)?;
        csvs.push((table, p.to_string_lossy().into_owned(), extra));
        Ok(())
    };
    use Cell::{B, I, S};
    let alive = |id: &String| g.sym.contains_key(id);

    w("Meta", "meta.csv", "", &["key", "value"], &mut meta.iter().map(|(k, v)| vec![S(k.clone()), S(v.clone())]))?;
    w("Package", "package.csv", "", &["name", "lang", "version", "workspace", "path"],
      &mut g.pkgs.iter().map(|(n, p)| vec![S(n.clone()), S(p.lang.into()), S(p.version.clone()), B(p.workspace), S(p.path.clone())]))?;
    w("File", "file.csv", "", &["path", "package", "lang", "lines", "is_test", "commits_1y", "last_change", "mtime", "hash"],
      &mut g.files.iter().map(|(p, f)| vec![
          S(p.clone()), S(f.package.clone()), S(f.lang.into()), I(f.lines as i64), B(f.is_test),
          I(churn.get(p).copied().unwrap_or(0)), S(last.get(p).cloned().unwrap_or_default()), Cell::F(f.mtime), S(f.hash.clone()),
      ]))?;
    let mut syms = g.sym.values().map(|s| vec![
        S(s.id.clone()), S(s.name.clone()), S(s.path.clone()), S(s.kind.into()), S(s.lang.name().into()), S(s.package.clone()), B(false),
        S(s.file.clone()), I(s.line.into()), I(s.end_line.into()), I((s.end_line - s.line + 1).into()), S(s.visibility.into()),
        B(s.is_async), B(s.is_unsafe), B(s.is_test), S(s.trait_name.clone()), S(s.self_type.clone()), S(s.signature.clone()),
        S(s.attrs.clone()), S(s.doc.clone()), I(s.cc),
    ]).chain(g.ext.iter().map(|(id, e)| vec![
        S(id.clone()), S(e.name.clone()), S(e.path.clone()), S(e.kind.into()), S(e.lang.into()), S(e.package.clone()), B(true),
        S(String::new()), I(0), I(0), I(0), S(String::new()), B(false), B(false), B(false), S(String::new()), S(String::new()),
        S(String::new()), S(String::new()), S(String::new()), I(0),
    ]));
    w("Symbol", "symbol.csv", "", &["id", "name", "path", "kind", "lang", "package", "external", "file", "line", "end_line", "loc",
      "visibility", "is_async", "is_unsafe", "is_test", "trait_name", "self_type", "signature", "attrs", "doc", "cc"], &mut syms)?;
    w("Channel", "channel.csv", "", &["id", "channel", "key", "providers", "users"],
      &mut channels.iter().map(|c| vec![S(c.0.clone()), S(c.1.clone()), S(c.2.clone()), I(c.3), I(c.4)]))?;
    w("DEPENDS_ON", "depends.csv", "", &["from", "to", "kind"],
      &mut g.deps.iter().filter(|d| g.pkgs.contains_key(&d.0) && g.pkgs.contains_key(&d.1)).map(|d| vec![S(d.0.clone()), S(d.1.clone()), S(d.2.clone())]))?;
    w("CONTAINS", "pkg_file.csv", ", from='Package', to='File'", &["from", "to"],
      &mut g.files.iter().filter(|(_, f)| g.pkgs.contains_key(&f.package)).map(|(p, f)| vec![S(f.package.clone()), S(p.clone())]))?;
    w("CONTAINS", "contains.csv", ", from='Symbol', to='Symbol'", &["from", "to"],
      &mut g.sym.values().filter_map(|s| parent_id(s).filter(|p| g.sym.contains_key(p)).map(|p| vec![S(p), S(s.id.clone())])))?;
    w("DEFINES", "defines.csv", "", &["from", "to"],
      &mut g.sym.values().filter(|s| g.files.contains_key(&s.file)).map(|s| vec![S(s.file.clone()), S(s.id.clone())]))?;
    w("CALLS", "calls.csv", "", &["from", "to", "count", "line"],
      &mut g.calls.iter().filter(|(k, _)| alive(&k.0)).map(|((a, b), (n, l))| vec![S(a.clone()), S(b.clone()), I(*n), I((*l).into())]))?;
    w("REFERENCES", "refs.csv", "", &["from", "to", "count", "line"],
      &mut g.refs.iter().filter(|(k, _)| alive(&k.0)).map(|((a, b), (n, l))| vec![S(a.clone()), S(b.clone()), I(*n), I((*l).into())]))?;
    w("IMPORTS", "imports.csv", "", &["from", "to", "line"],
      &mut g.imports.iter().filter(|(k, _)| alive(&k.0)).map(|((a, b), l)| vec![S(a.clone()), S(b.clone()), I((*l).into())]))?;
    w("IMPLEMENTS", "implements.csv", "", &["from", "to", "file", "line"],
      &mut g.implements.iter().map(|((a, b), (f, l))| vec![S(a.clone()), S(b.clone()), S(f.clone()), I((*l).into())]))?;
    // trait / interface / base method -> each implementation or override: CALLS stops at the abstract
    // method on dynamic dispatch; [:CALLS|DISPATCHES*] keeps going into the bodies that can run
    w("DISPATCHES", "dispatches.csv", "", &["from", "to"],
      &mut g.implements.keys().filter(|(a, b)| is_callable(g.kind_of(a)) && is_callable(g.kind_of(b)))
          .map(|(a, b)| vec![S(b.clone()), S(a.clone())]))?;
    w("HAS_METHOD", "has_method.csv", "", &["from", "to"], &mut g.has_method.iter().map(|(a, b)| vec![S(a.clone()), S(b.clone())]))?;
    for (table, rows) in [("PROVIDES", provides), ("USES", uses)] {
        for (is_file, suffix) in [(false, "sym"), (true, "file")] {
            let name = format!("{}_{suffix}.csv", table.to_lowercase());
            let extra = if is_file { ", from='File', to='Channel'" } else { ", from='Symbol', to='Channel'" };
            w(table, &name, extra, &["from", "to", "file", "line", "rule", "raw"],
              &mut rows.iter().filter(|r| r.1 == is_file).map(|r| vec![S(r.0.clone()), S(r.2.clone()), S(r.3.clone()), I(r.4.into()), S(r.5.clone()), S(r.6.clone())]))?;
        }
    }
    w("LINKS_TO", "links_to.csv", "", &["from", "to", "channel", "key", "file", "line"],
      &mut links_to.iter().map(|l| vec![S(l.0.clone()), S(l.1.clone()), S(l.2.clone()), S(l.3.clone()), S(l.4.clone()), I(l.5.into())]))?;

    let out = db_dir.join("graph.lbug");
    let new = db_dir.join("graph.lbug.new");
    for p in [&new, &db_dir.join("graph.lbug.new.wal")] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
    {
        let db = lbug::Database::new(&new, lbug::SystemConfig::default())?;
        let c = lbug::Connection::new(&db)?;
        for stmt in SCHEMA {
            c.query(stmt).with_context(|| format!("schema: {stmt}"))?;
        }
        for (table, file, extra) in &csvs {
            let q = format!("COPY {table} FROM '{file}' (header=true, quote='\"', escape='\"', auto_detect=false{extra})");
            c.query(&q).with_context(|| format!("loading {table} from {file}"))?;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    for p in [&out, &db_dir.join("graph.lbug.wal")] {
        let _ = std::fs::remove_file(p);
    }
    std::fs::rename(&new, &out)?;
    if db_dir.join("graph.lbug.new.wal").exists() {
        std::fs::rename(db_dir.join("graph.lbug.new.wal"), db_dir.join("graph.lbug.wal"))?;
    }
    Ok(())
}

enum Cell {
    S(String),
    I(i64),
    F(f64),
    B(bool),
}

impl Cell {
    fn csv(self) -> String {
        match self {
            Cell::S(s) => {
                let mut out = String::with_capacity(s.len() + 2);
                out.push('"');
                let mut first = true;
                for word in s.split_whitespace() {
                    if !first {
                        out.push(' ');
                    }
                    first = false;
                    let _ = write!(out, "{}", word.replace('"', "\"\""));
                }
                out.push('"');
                out
            }
            Cell::I(i) => i.to_string(),
            Cell::F(f) => format!("{f}"),
            Cell::B(b) => b.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_polyglot_roots() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/poly");
        let got: Vec<(Lang, String)> = detect_indexes(&repo).into_iter().map(|s| (s.lang, s.root)).collect();
        assert_eq!(got, [(Lang::Rust, "server".into()), (Lang::Typescript, "web".into()), (Lang::Python, "worker".into())]);
    }

    #[test]
    fn csv_cells_escape_quotes_and_newlines() {
        assert_eq!(Cell::S("a \"b\"\n  c".into()).csv(), "\"a \"\"b\"\" c\"");
        assert_eq!(Cell::B(true).csv(), "true");
    }

    #[test]
    fn rel_paths_join_under_index_root() {
        assert_eq!(join_rel(".", "src/a.rs"), "src/a.rs");
        assert_eq!(join_rel("web/", "./src/a.ts"), "web/src/a.ts");
    }
}

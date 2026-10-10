//! Per-project service links: regex rules from `.cg/links/*.toml` turn source text into
//! endpoints (`provides` / `uses` on a channel such as http or nats) which the graph connects
//! across languages. Nothing framework-specific lives here; rule packs ship in `links/`.

use anyhow::{Context, Result, bail};
use regex::{Captures, Regex};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// serves / handles: HTTP route handler, message subscriber
    Provides,
    /// calls / sends: HTTP client call, message publisher
    Uses,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub channel: String,
    pub role: Role,
    /// globs relative to the repo root; `**` crosses directories
    pub files: Vec<String>,
    /// regex with named groups: `key` (required here or in `each`), optional `verb`, `symbol`, `body`
    pub pattern: String,
    /// optional regex run over the `body` group (else the whole match), one endpoint per match
    pub each: Option<String>,
    /// key template over capture names, e.g. "{verb} {key}"
    #[serde(default = "default_key")]
    pub key: String,
    /// prepended to the `key` capture (router nesting, base URLs)
    #[serde(default)]
    pub prefix: String,
    /// regexes deleted from the `key` capture before normalisation
    #[serde(default)]
    pub strip: Vec<String>,
    /// segment separator for wildcard matching: "/" for paths, "." for NATS subjects
    #[serde(default = "default_sep")]
    pub sep: String,
    /// values for captures that did not participate, e.g. { verb = "GET" }
    #[serde(default)]
    pub defaults: HashMap<String, String>,
    /// regexes: a `key` capture matching any of them yields no endpoint (routes the prefix gets wrong)
    #[serde(default)]
    pub skip: Vec<String>,
    pub name: Option<String>,
}

/// `[[endpoint]]`: an endpoint no regex can see (paths from a const, a config file), pinned to a
/// symbol by its graph path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    pub channel: String,
    pub role: Role,
    /// the full key, e.g. "GET /q/health"
    pub key: String,
    /// graph path of the owner, e.g. "antares_api::health"
    pub symbol: String,
    #[serde(default = "default_sep")]
    pub sep: String,
}

/// A pinned endpoint: `rule` indexes a rule that matches no file and carries channel/role/sep.
pub struct Fixed {
    pub rule: usize,
    pub key: String,
    pub symbol: String,
}

fn default_key() -> String {
    "{key}".into()
}

fn default_sep() -> String {
    "/".into()
}

#[derive(Deserialize)]
struct RuleFile {
    #[serde(default)]
    link: Vec<RuleSpec>,
    #[serde(default)]
    endpoint: Vec<EndpointSpec>,
}

pub struct Rule {
    pub spec: RuleSpec,
    pub id: String,
    re: Regex,
    each: Option<Regex>,
    strip: Vec<Regex>,
    skip: Vec<Regex>,
    globs: Vec<glob::Pattern>,
}

#[derive(Default)]
pub struct Links {
    pub rules: Vec<Rule>,
    pub fixed: Vec<Fixed>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub rule: usize,
    pub key: String,
    pub raw: String,
    pub line: i32, // 0-based
    pub col: i32,
    /// position and text of the `symbol` capture: the handler named at the call site
    pub symbol: Option<(i32, i32, String)>,
}

const GLOB: glob::MatchOptions =
    glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };

impl Links {
    /// Rules from `[[link]]` tables in every `*.toml` under `dir` (sorted, so ids are stable).
    pub fn load_dir(dir: &Path) -> Result<Links> {
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect(),
            Err(_) => return Ok(Links::default()),
        };
        files.sort();
        let mut links = Links::default();
        for f in files {
            let text = std::fs::read_to_string(&f)?;
            let parsed: RuleFile = toml::from_str(&text).with_context(|| format!("parsing {}", f.display()))?;
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("links").to_string();
            for (n, spec) in parsed.link.into_iter().enumerate() {
                let id = spec.name.clone().unwrap_or_else(|| format!("{stem}#{}", n + 1));
                links.rules.push(Rule::compile(spec, id).with_context(|| format!("in {}", f.display()))?);
            }
            for (n, e) in parsed.endpoint.into_iter().enumerate() {
                let key = e.key.split_whitespace().map(|t| normalize_key(t, &e.sep)).collect::<Vec<_>>().join(" ");
                let spec = RuleSpec {
                    channel: e.channel, role: e.role, files: vec![], pattern: String::new(), each: None, key: default_key(),
                    prefix: String::new(), strip: vec![], sep: e.sep, defaults: HashMap::new(), skip: vec![], name: None,
                };
                let rule = Rule { spec, id: format!("{stem}#endpoint{}", n + 1), re: NEVER.clone(),
                                  each: None, strip: vec![], skip: vec![], globs: vec![] };
                links.fixed.push(Fixed { rule: links.rules.len(), key, symbol: e.symbol });
                links.rules.push(rule);
            }
        }
        Ok(links)
    }

    pub fn applies(&self, rel: &str) -> bool {
        self.rules.iter().any(|r| r.matches_file(rel))
    }

    /// Endpoints in one file's text, for every rule whose globs match `rel`.
    pub fn extract(&self, rel: &str, text: &str) -> Vec<Hit> {
        let starts: Vec<usize> =
            std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        let pos = |off: usize| {
            let line = starts.partition_point(|&s| s <= off) - 1;
            (line as i32, text[starts[line]..off].chars().count() as i32)
        };
        let mut out = Vec::new();
        for (ri, rule) in self.rules.iter().enumerate() {
            if !rule.matches_file(rel) {
                continue;
            }
            for outer in rule.re.captures_iter(text) {
                let whole = outer.get(0).expect("group 0");
                match &rule.each {
                    None => out.extend(rule.hit(ri, &outer, None, &pos)),
                    Some(each) => {
                        let scope = outer.name("body").unwrap_or(whole);
                        for inner in each.captures_iter(scope.as_str()) {
                            out.extend(rule.hit(ri, &outer, Some((&inner, scope.start())), &pos));
                        }
                    }
                }
            }
        }
        out
    }
}

impl Rule {
    /// One endpoint from an outer match and, with `each`, one inner match (offset `base` in text).
    fn hit(&self, ri: usize, outer: &Captures, inner: Option<(&Captures, usize)>, pos: &dyn Fn(usize) -> (i32, i32)) -> Option<Hit> {
        // (text, absolute byte offset) of a capture, the inner match winning over the outer one
        let get = |name: &str| -> Option<(String, usize)> {
            inner
                .and_then(|(c, base)| c.name(name).map(|m| (m.as_str().to_string(), base + m.start())))
                .or_else(|| outer.name(name).map(|m| (m.as_str().to_string(), m.start())))
        };
        let (key, key_at) = get("key")?;
        if self.skip.iter().any(|r| r.is_match(&key)) {
            return None;
        }
        let mut vals: HashMap<&str, String> = HashMap::new();
        let names = self.re.capture_names().flatten().chain(self.each.iter().flat_map(|e| e.capture_names().flatten()));
        for name in names {
            let v = get(name).map(|(t, _)| t).or_else(|| self.spec.defaults.get(name).cloned());
            vals.insert(name, v.unwrap_or_default());
        }
        if let Some(v) = vals.get_mut("verb") {
            *v = v.to_uppercase();
        }
        vals.insert("key", self.normalize(&key));
        let (line, col) = pos(key_at);
        // `entities::create_entity`, `self.on_msg`: SCIP resolves the last segment to the handler
        let symbol = get("symbol").map(|(t, at)| {
            let cut = t.rfind([':', '.']).map_or(0, |i| i + 1);
            let (l, c) = pos(at + cut);
            (l, c, t[cut..].to_string())
        });
        let raw = inner.and_then(|(c, _)| c.get(0)).or_else(|| outer.get(0)).map_or("", |m| m.as_str());
        Some(Hit { rule: ri, key: render(&self.spec.key, &vals), raw: raw.split_whitespace().collect::<Vec<_>>().join(" "), line, col, symbol })
    }

    fn compile(spec: RuleSpec, id: String) -> Result<Rule> {
        let re = Regex::new(&spec.pattern).with_context(|| format!("rule {id}: bad pattern"))?;
        let each = spec.each.as_deref().map(Regex::new).transpose().with_context(|| format!("rule {id}: bad each"))?;
        let has = |r: &Regex| r.capture_names().flatten().any(|n| n == "key");
        if !has(&re) && !each.as_ref().is_some_and(has) {
            bail!("rule {id}: pattern needs a (?P<key>...) group");
        }
        if spec.files.is_empty() {
            bail!("rule {id}: `files` is empty");
        }
        let strip = spec.strip.iter().map(|s| Regex::new(s)).collect::<Result<_, _>>().with_context(|| format!("rule {id}: bad strip"))?;
        let skip = spec.skip.iter().map(|s| Regex::new(s)).collect::<Result<_, _>>().with_context(|| format!("rule {id}: bad skip"))?;
        let globs = spec.files.iter().map(|g| glob::Pattern::new(g)).collect::<Result<_, _>>().with_context(|| format!("rule {id}: bad glob"))?;
        Ok(Rule { spec, id, re, each, strip, skip, globs })
    }

    pub fn matches_file(&self, rel: &str) -> bool {
        self.globs.iter().any(|g| g.matches_with(rel, GLOB))
    }

    fn normalize(&self, key: &str) -> String {
        let mut k = key.to_string();
        for s in &self.strip {
            k = s.replace_all(&k, "").into_owned();
        }
        normalize_key(&format!("{}{}", self.spec.prefix, k), &self.spec.sep)
    }
}

static NEVER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\s\S]").unwrap());
static PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{[^}]*\}|\{[^}]*\}|<[^>]*>|\[[^\]]*\]").unwrap());
static COLON_PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:\w+$").unwrap());
static ORIGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z][\w+.-]*://[^/]*").unwrap());

/// Path parameters in any framework's spelling become `{}`; for "/" keys also drop origin,
/// query, fragment and the trailing slash.
pub fn normalize_key(key: &str, sep: &str) -> String {
    let mut k = PARAM.replace_all(key.trim(), "{}").into_owned();
    if sep == "/" {
        k = ORIGIN.replace(&k, "").into_owned();
        k = k.split(['?', '#']).next().unwrap_or("").to_string();
        while k.contains("//") {
            k = k.replace("//", "/");
        }
        if k.len() > 1 {
            k = k.trim_end_matches('/').to_string();
        }
    }
    k.split(sep).map(|s| if COLON_PARAM.is_match(s) { "{}" } else { s }).collect::<Vec<_>>().join(sep)
}

fn render(tpl: &str, vals: &HashMap<&str, String>) -> String {
    let mut out = tpl.to_string();
    for (k, v) in vals {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does a `uses` key reach a `provides` key? Whitespace-separated tokens pair up; inside a token
/// `{}` and `*` match one segment, `>` and `**` the rest.
pub fn key_matches(a: &str, b: &str, sep: &str) -> bool {
    if a == b {
        return true;
    }
    let (ta, tb): (Vec<&str>, Vec<&str>) = (a.split_whitespace().collect(), b.split_whitespace().collect());
    ta.len() == tb.len() && ta.iter().zip(&tb).all(|(x, y)| token_matches(x, y, sep))
}

fn token_matches(a: &str, b: &str, sep: &str) -> bool {
    let wild = |s: &str| s == "{}" || s == "*";
    let rest = |s: &str| s == ">" || s == "**";
    let (sa, sb): (Vec<&str>, Vec<&str>) = (a.split(sep).collect(), b.split(sep).collect());
    for i in 0..sa.len().max(sb.len()) {
        match (sa.get(i), sb.get(i)) {
            (Some(x), _) if rest(x) => return true,
            (_, Some(y)) if rest(y) => return true,
            (Some(x), Some(y)) if x == y || wild(x) || wild(y) => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(toml_text: &str) -> Links {
        let dir = std::env::temp_dir().join(format!("cg-links-{}", std::process::id() as u64 + toml_text.len() as u64));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("r.toml"), toml_text).unwrap();
        Links::load_dir(&dir).unwrap()
    }

    #[test]
    fn normalizes_every_param_spelling() {
        assert_eq!(normalize_key("/entities/{id}/attrs/:attr", "/"), "/entities/{}/attrs/{}");
        assert_eq!(normalize_key("https://x.io/api/e/${id}?limit=1", "/"), "/api/e/{}");
        assert_eq!(normalize_key("/users/<int:uid>/", "/"), "/users/{}");
        assert_eq!(normalize_key("/blog/[slug]", "/"), "/blog/{}");
        assert_eq!(normalize_key("/", "/"), "/");
        assert_eq!(normalize_key("entities.created", "."), "entities.created");
    }

    #[test]
    fn wildcard_matching() {
        assert!(key_matches("GET /api/entities/{}", "GET /api/entities/{}", "/"));
        assert!(key_matches("entities.urn1.created", "entities.*.created", "."));
        assert!(key_matches("a.b.c", "a.>", "."));
        assert!(!key_matches("GET /api/e", "POST /api/e", "/"));
        assert!(!key_matches("/api/e/{}/x", "/api/e/{}", "/"));
        assert!(!key_matches("GET /a", "/a", "/"));
    }

    #[test]
    fn axum_route_chain_yields_one_endpoint_per_method() {
        let l = links(r#"
[[link]]
channel = "http"
role = "provides"
files = ["server/**/*.rs"]
pattern = '(?s)\.route\(\s*"(?P<key>[^"]+)"\s*,(?P<body>.*?)\)\s*,?\s*\)'
each = '(?P<verb>get|post|put|patch|delete)\(\s*(?P<symbol>[\w:]+)'
key = "{verb} {key}"
prefix = "/api"
"#);
        let src = "fn app() {\n  r.route(\"/entities/{id}\", get(get_entity).delete(delete_entity))\n}\n";
        let hits = l.extract("server/src/main.rs", src);
        let keys: Vec<_> = hits.iter().map(|h| (h.key.as_str(), h.symbol.as_ref().unwrap().2.as_str())).collect();
        assert_eq!(keys, [("GET /api/entities/{}", "get_entity"), ("DELETE /api/entities/{}", "delete_entity")]);
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[1].symbol.as_ref().unwrap().1, 51);
        let q = l.extract("server/src/lib.rs", "r.route(\"/x\", post(api::batch::upsert),\n)");
        assert_eq!(q[0].symbol, Some((0, 31, "upsert".to_string())), "last path segment, trailing comma");
        assert!(l.extract("web/src/a.rs", src).is_empty());
    }

    #[test]
    fn fetch_with_default_and_explicit_verb() {
        let l = links(r#"
[[link]]
channel = "http"
role = "uses"
files = ["web/**/*.ts", "web/**/*.tsx"]
pattern = '''fetch\(\s*[`"'](?P<key>[^`"']+)[`"'](?:\s*,\s*\{[^}]*?method:\s*["'](?P<verb>\w+)["'])?'''
key = "{verb} {key}"
defaults = { verb = "GET" }
"#);
        let src = "await fetch(`/api/entities/${id}`);\nawait fetch(\"/api/entities\", { method: \"post\", body });\n";
        let keys: Vec<_> = l.extract("web/src/api.ts", src).into_iter().map(|h| h.key).collect();
        assert_eq!(keys, ["GET /api/entities/{}", "POST /api/entities"]);
    }

    #[test]
    fn rejects_rules_without_key_group() {
        let dir = std::env::temp_dir().join("cg-links-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.toml"), "[[link]]\nchannel='x'\nrole='uses'\nfiles=['*']\npattern='foo'\n").unwrap();
        let err = Links::load_dir(&dir).err().unwrap();
        assert!(format!("{err:#}").contains("(?P<key>"), "{err:#}");
    }

    #[test]
    fn skip_drops_keys_and_endpoints_pin_by_path() {
        let l = links(r#"
[[link]]
channel = "http"
role = "provides"
files = ["*.rs"]
pattern = '\.route\("(?P<key>[^"]+)"'
prefix = "/api"
skip = ['^/ex/']

[[endpoint]]
channel = "http"
role = "provides"
key = "GET /q/tenants/:tenant/"
symbol = "k::tenant_get"
"#);
        let hits = l.extract("a.rs", r#".route("/x", h).route("/ex/v1/n", h)"#);
        assert_eq!(hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(), ["/api/x"]);
        assert_eq!(l.fixed.len(), 1);
        assert_eq!((l.fixed[0].key.as_str(), l.fixed[0].symbol.as_str()), ("GET /q/tenants/{}", "k::tenant_get"));
        let r = &l.rules[l.fixed[0].rule];
        assert!(r.spec.role == Role::Provides && !r.matches_file("a.rs"));
    }
}

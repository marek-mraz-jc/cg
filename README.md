# cg: a Cypher code graph for agents

`cg` turns compiler-grade indexes ([SCIP](https://github.com/sourcegraph/scip)) of a Rust, TypeScript/React
and Python codebase into one embedded graph database ([LadybugDB](https://ladybugdb.com), openCypher).
An agent asks many structural questions in one call: who calls this, what implements that trait, which UI
component reaches which backend handler, which routes nobody calls.

```
cg build                      # index the repo, load .cg/db/graph.lbug
cg schema                     # tables, counts, conventions, example queries
cg q -e "<cypher>" -e "..."   # many statements per call
cg q queries.cypher           # statements end with ';' at the end of a line
```

## Why SCIP and not tree-sitter

Callers come from the language's own compiler front end (rust-analyzer, the TypeScript compiler,
Pyright), so `MemStore::get` and `HashMap::get` are different nodes. In the lab benchmark on a 129k-line
Rust workspace, tree-sitter graphs reported callers of common names (`new`, `get`, `create`) at 5 to 17 %
precision; SCIP-derived edges are what rust-analyzer resolves.

## What is in the graph

| Node | Key properties |
|---|---|
| `Symbol` | `name`, `path`, `kind`, `lang`, `package`, `file`, `line`, `signature`, `doc`, `visibility`, `is_test`, `is_async`, `cc`, `external`, `cfg`, `features` |
| `File` | `path`, `lang`, `package`, `lines`, `is_test`, `commits_1y`, `last_change` |
| `Package` | `name`, `lang`, `version`, `workspace` |
| `Channel` | `channel` (`http`, `nats`, ...), `key` (`POST /api/entities`, `entities.created`), `providers`, `users` |

| Relationship | Meaning |
|---|---|
| `CALLS` | callable used in a callable body (calls, fn pointers, JSX components, macros) |
| `REFERENCES` | any other use: types, fields, constants |
| `IMPORTS` | module-level uses |
| `IMPLEMENTS` | type -> trait / interface / base class; method -> the method it implements or overrides |
| `DISPATCHES` | trait / interface / base method -> each implementation or override (dynamic dispatch) |
| `HAS_METHOD`, `CONTAINS`, `DEFINES`, `DEPENDS_ON` | structure |
| `PROVIDES`, `USES` | Symbol or File -> Channel, from the project's link rules |
| `LINKS_TO` | resolved cross-service edge: client call -> route handler, publisher -> subscriber |

`[:CALLS|DISPATCHES|LINKS_TO*1..6]` follows a request from a React component through the HTTP handler,
through trait objects into the store implementation, and into a message subscriber written in another
language. Leave out `DISPATCHES` and the path stops at the trait method: on Antares, `delete_entity`
reaches 128 functions within 4 hops over `CALLS` alone and 202 with `DISPATCHES`.

`cfg` holds a Rust symbol's `#[cfg(...)]` conditions together with those of every enclosing module,
joined by ` && `. `features` lists the cargo features those conditions require: `feature = "x"` and
the conjuncts of `all(...)`; `not(...)` and `any(...)` require none. An empty `features` means the
symbol compiles in every build. Optional crates, `required-features` and a `#![cfg]` at the top of a
file are not covered; Cargo.toml has those.

```cypher
// what the odata feature adds, and feature-only code that reaches the store crate
MATCH (s:Symbol) WHERE s.features CONTAINS 'odata' AND NOT s.is_test RETURN s.kind, count(*);
MATCH (s:Symbol)-[:CALLS*1..3]->(x:Symbol) WHERE s.features <> '' AND x.package = 'antares-store' RETURN DISTINCT s.path, s.features;
```

## Project configuration: `.cg/`

Everything project-specific lives in the project, not in `cg`.

`.cg/config.toml` (optional; without `[[index]]` entries `cg` finds the topmost `Cargo.toml`,
`tsconfig.json`/`package.json` and `pyproject.toml`/`setup.py` up to two directories deep):

```toml
db = ".cg/db"              # default
deps = ["serde_json"]      # Rust dependencies to index too (same as `cg build --deps serde_json`)

[[index]]
lang = "rust"              # rust | typescript | python
root = "server"            # indexer working directory; SCIP paths are relative to it

[[index]]
lang = "typescript"
root = "web"
cmd = "npx --yes @sourcegraph/scip-typescript index --output {out}"   # optional override

[[index]]
lang = "python"
root = "worker"
name = "worker"            # scip-python project name (default: directory name)
# scip = "path/to/prebuilt.scip"   # use an existing index instead of running the indexer
```

Indexers: `rust-analyzer` (`rustup component add rust-analyzer`); `scip-typescript` and `scip-python`
from npm (`@sourcegraph/scip-typescript`, `@sourcegraph/scip-python`) on `PATH`, otherwise `cg` runs
them through `npx`. An index whose indexer fails is skipped with a warning; the others still load.
`cg build --reuse` keeps the previous build's SCIP output for an index whose sources (and manifests)
have not changed since; an index with a newer source file is indexed again. Loading a stale SCIP
against current sources would shift lines and test flags, so `--reuse` never does that.

### Dependencies: `--deps`

By default a dependency's symbols are boundary nodes (`external=true`, no file, no edges out).
`cg build --deps serde_json,axum` (or `deps = [...]` in the config) indexes those Rust crates as well:
`cg` finds every version the workspace resolves through `cargo metadata`, copies each one into
`~/.cache/cg/deps/<name>-<version>-<hash>` (or `$CG_DEPS_CACHE`) and runs rust-analyzer there once per
version. Calls from the workspace then land on real definitions (`external=false`, `package` = crate
name, `Package.workspace=false`), and `CALLS` continues inside the dependency. On Antares, serde_json
took 17 s to index; 4,213 calls landed on 57 serde_json definitions.

```cypher
MATCH (c:Symbol)-[:CALLS]->(f:Symbol {package:'serde_json'})-[:CALLS*1..2]->(g:Symbol {package:'serde_json'})
WHERE NOT c.is_test AND NOT f.external RETURN f.path, count(DISTINCT g) ORDER BY count(DISTINCT g) DESC LIMIT 10;
```

### Link rules: `.cg/links/*.toml`

Each `[[link]]` turns regex matches in source files into endpoints. `links/` in this repository holds
starting points (axum, fetch/axios, FastAPI/Flask, requests/httpx, reqwest, NATS); copy the ones a
project uses into its `.cg/links/` and adjust prefixes, client names and file globs.

```toml
[[link]]
name = "axum-route"                 # shown in PROVIDES/USES.rule
channel = "http"                    # endpoints only match inside one channel
role = "provides"                   # provides (handler, subscriber) | uses (client call, publisher)
files = ["server/**/*.rs"]          # globs relative to the repo root
pattern = '(?s)\.route\(\s*"(?P<key>[^"]+)"\s*,(?P<body>.*?)\)\s*\)'
each = '(?P<verb>get|post|put|patch|delete)\(\s*(?P<symbol>[\w:]+)'   # optional: one endpoint per match in `body`
key = "{verb} {key}"                # template over capture names
prefix = "/api"                     # prepended to the key capture (router nesting)
strip = ['^\$\{API\}']              # regexes removed from the key capture
sep = "/"                           # "." for NATS subjects
defaults = { verb = "GET" }         # for captures that did not participate
skip = ['^/ex/']                    # key captures matching these give no endpoint (wrong prefix)

[[endpoint]]                        # an endpoint no regex can see: paths from a const, a config file
channel = "http"
role = "provides"
key = "GET /q/health"
symbol = "antares_api::health"      # graph path of the owner; `cg build` warns if unknown
```

- `key` is required. Path parameters in any spelling (`{id}`, `:id`, `<int:id>`, `${id}`, `[id]`)
  become `{}`; for `/` keys the origin, query string and trailing slash are dropped.
- The endpoint belongs to the symbol named by the `symbol` capture, resolved through SCIP at that
  position (`get(create_entity)` -> `create_entity`, `cb=on_created` -> `on_created`, the `def` under a
  decorator). Without it, the innermost enclosing definition owns it; outside any indexed file, the File.
- A `uses` key reaches a `provides` key when they are equal or match segment by segment, where `{}`
  and `*` match one segment and `>` / `**` the rest.
- Endpoints inside test code (test files, `#[cfg(test)]`, `test_*`) are dropped: a mock server or
  test client is not part of the service wiring, so it neither counts on a Channel nor links.

## Queries agents use

```cypher
// UI -> backend -> worker, across languages
MATCH p=(u:Symbol {name:'EntityList'})-[:CALLS|LINKS_TO*1..6]->(h:Symbol) WHERE h.lang <> u.lang RETURN p LIMIT 10;
// client calls with no server, routes nobody calls
MATCH (c:Channel) WHERE c.providers = 0 OR c.users = 0 RETURN c.channel, c.key, c.providers, c.users;
// blast radius of a change, production code only
MATCH (c:Symbol)-[:CALLS*1..3]->(f:Symbol {path:'poly_server::store::MemStore::put'}) WHERE NOT c.is_test RETURN DISTINCT c.path;
// every implementation of a trait method (dynamic dispatch)
MATCH (m:Symbol)-[:IMPLEMENTS]->(t:Symbol {path:'poly_server::store::Store::get'}) RETURN m.path, m.file;
```

`cg schema` prints the full list; a test runs every listed example against the fixture. Every `cg q` output starts with the build time and lists indexed
files edited since, so stale answers are visible. An empty result prints hints for literals that
match nothing, with similar names.

A batch also takes two non-Cypher statements, so one call can find an answer and check it against
the source:

```text
SHOW poly_server::store::MemStore::put, on_created, server/src/main.rs:10-30;
GREP gate!\( IN crates/antares-api/src/**/*.rs;
```

- `SHOW` prints the source of symbols (by graph path, else by name, 5 per name, 150 lines each), a
  whole file or a file range.
- `GREP` runs a regex over every git-tracked file (the graph's files without git). Each hit names its innermost enclosing symbol and
  carries test and comment flags; the totals line counts distinct non-test enclosing symbols outside
  comments, which answers "how many functions use X" for text no edge carries.

## Building

```
docker build --output dist .     # -> dist/cg: build, tests and clippy in GCC 14 on Debian 12
```

The binary links libstdc++ statically and runs on any Linux x86_64 with glibc 2.36 or newer.
CI does the same build on every push and attaches `cg` to a GitHub release for `v*` tags.

Without Docker: LadybugDB's C++ needs GCC 13 or newer (`<format>`), or Apple clang. Then plain
`cargo build --release` works. To make a host-independent binary, put a directory holding only
`libstdc++.a` on the link path: `RUSTFLAGS="-L native=<dir>" cargo build --release`.

`cargo test` runs unit tests plus an end-to-end test on `tests/fixtures/poly` (React + Rust + Python
over HTTP and NATS) from checked-in SCIP files, so no indexer is needed for the tests.

## Adding a language

SCIP indexers exist for Java/Kotlin/Scala, Go, C#, C/C++, Ruby and more. A new language is a `Lang`
variant in `src/lang.rs`: its SCIP scheme, indexer command, test-file and test-symbol rules, and a
branch regex for `cc`. Kinds and signatures come from the indexer's hover text, so most languages
need no more.

## Limits

- The graph is a snapshot; rebuild after edits (`cg q` lists the files changed since). Rebuilding a
  Rust index runs rust-analyzer over the whole workspace (about 90 s on a 140k-line workspace); there is
  no per-crate incremental index yet.
- Use the graph for relations, then read the code: it says where to look, not what the code does.
- Link rules are regexes: a URL built at run time from variables does not match. `strip`, `prefix`
  and `{}` normalisation cover base URLs and path parameters; anything else needs its own rule.
- Writes to fields are not recorded: none of the three indexers marks write access.
- Macro-generated Rust items appear only where rust-analyzer emits them. `macro_rules!` bodies get
  CALLS edges for qualified paths (`$crate::a::f(`); `.method()` calls inside a macro stay invisible.

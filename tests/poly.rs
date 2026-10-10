//! End to end on tests/fixtures/poly: React (TS) -> Rust HTTP server -> NATS -> Python worker.
//! Uses the prebuilt SCIP files in the fixture's .cg/scip, so no indexer has to be installed.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

fn cg(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_cg")).args(args).output().expect("run cg");
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "cg {args:?} failed:\n{text}");
    text
}

fn db() -> &'static str {
    static DB: OnceLock<String> = OnceLock::new();
    DB.get_or_init(|| {
        let db = std::env::temp_dir().join(format!("cg-poly-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&db);
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/poly");
        cg(&["build", "--repo", repo.to_str().unwrap(), "--db", db.to_str().unwrap()]);
        db.to_string_lossy().into_owned()
    })
}

fn q(stmt: &str) -> String {
    cg(&["q", "--db", db(), "-e", stmt])
}

#[test]
fn links_connect_services_across_languages() {
    let out = q("MATCH (a:Symbol)-[l:LINKS_TO]->(b:Symbol) RETURN l.key, a.path, b.path ORDER BY l.key");
    for row in [
        "GET /api/entities | web/src/api.ts::HttpSource::list | poly_server::list_entities",
        "GET /api/entities/{} | web/src/api.ts::loadEntity | poly_server::get_entity",
        "GET /api/stats | web/src/api.ts::loadStats | app::stats",
        "POST /api/entities | web/src/api.ts::createEntity | poly_server::create_entity",
        "entities.created | poly_server::create_entity | app::on_created",
        "stats.updated | app::on_created | poly_server::on_stats",
    ] {
        assert!(out.contains(row), "missing `{row}` in\n{out}");
    }
    assert!(out.contains("6 rows"), "{out}");
}

#[test]
fn request_is_followable_from_ui_to_worker() {
    let out = q("MATCH p=(u:Symbol {name:'EntityList'})-[:CALLS|LINKS_TO*1..4]->(h:Symbol {name:'on_created'}) RETURN p");
    assert!(out.contains("EntityList -CALLS-> Symbol:web/src/api.ts::createEntity -LINKS_TO-> Symbol:poly_server::create_entity -LINKS_TO-> Symbol:app::on_created"), "{out}");
}

#[test]
fn dangling_endpoints_are_visible() {
    let out = q("MATCH (c:Channel) WHERE c.providers = 0 OR c.users = 0 RETURN c.key, c.providers, c.users ORDER BY c.key");
    assert!(out.contains("DELETE /api/entities/{} | 1 | 0"), "{out}");
    assert!(out.contains("DELETE /api/purge | 0 | 1"), "{out}");
    assert!(out.contains("entities.deleted | 0 | 1"), "{out}");
    assert!(out.contains("3 rows"), "{out}");
}

#[test]
fn implements_in_every_language() {
    let out = q("MATCH (t:Symbol)-[:IMPLEMENTS]->(i:Symbol) RETURN t.lang, t.path, i.path ORDER BY t.path");
    for row in [
        "rust | poly_server::store::MemStore | poly_server::store::Store",
        "rust | poly_server::store::MemStore::put | poly_server::store::Store::put",
        "typescript | web/src/api.ts::HttpSource | web/src/api.ts::Source",
        "typescript | web/src/api.ts::HttpSource::list | web/src/api.ts::Source::list",
        "python | app::MemCounter | app::Counter",
        "python | app::MemCounter::bump | app::Counter::bump",
    ] {
        assert!(out.contains(row), "missing `{row}` in\n{out}");
    }
}

#[test]
fn tests_are_marked_and_production_code_is_not() {
    let out = q("MATCH (s:Symbol) WHERE s.name IN ['creates', 'test_on_created', 'create_entity', 'on_created'] AND s.lang <> 'typescript' RETURN s.name, s.is_test ORDER BY s.name");
    for row in ["create_entity | False", "creates | True", "on_created | False", "test_on_created | True"] {
        assert!(out.contains(row), "missing `{row}` in\n{out}");
    }
}

#[test]
fn kinds_and_components_from_hover_text() {
    let out = q("MATCH (s:Symbol) WHERE s.path IN ['web/src/api.ts::HttpSource', 'web/src/api.ts::Source', 'app::App', 'app::App::get', 'web/src/StatsPanel.tsx::StatsPanel'] RETURN s.path, s.kind ORDER BY s.path");
    for row in ["app::App | Class", "app::App::get | Method", "web/src/api.ts::HttpSource | Class", "web/src/api.ts::Source | Interface", "web/src/StatsPanel.tsx::StatsPanel | Function"] {
        assert!(out.contains(row), "missing `{row}` in\n{out}");
    }
    let jsx = q("MATCH (a:Symbol {name:'EntityList'})-[:CALLS]->(b:Symbol {name:'StatsPanel'}) RETURN count(*)");
    assert!(jsx.contains("\n1\n"), "JSX element should be a CALLS edge:\n{jsx}");
}

#[test]
fn batch_errors_and_typo_hints_do_not_stop_later_statements() {
    let out = cg(&["q", "--db", db(), "-e", "MATCH (s:Symbol {name:'create_entiy'}) RETURN s.path", "-e", "MATCH (s:Nope) RETURN s", "-e", "MATCH (s:Symbol {name:'on_stats'}) RETURN s.lang"]);
    assert!(out.contains("similar names: create_entity") || out.contains("similar names: createEntity"), "{out}");
    assert!(out.contains("ERROR"), "{out}");
    assert!(out.contains("### Q3") && out.contains("rust"), "{out}");
}

#[test]
fn colliding_impl_methods_get_qualified_paths_and_generics_leave_paths() {
    let out = q("MATCH (s:Symbol) WHERE s.name IN ['all', 'new'] AND s.lang = 'rust' AND s.kind <> 'TraitMethod' RETURN s.path ORDER BY s.path");
    for row in ["poly_server::store::<MemStore as Snapshot>::all", "poly_server::store::<MemStore as Store>::all", "poly_server::store::Page::new"] {
        assert!(out.contains(row), "missing `{row}` in\n{out}");
    }
    // no collision, no qualification
    assert!(q("MATCH (s:Symbol {path:'poly_server::store::MemStore::put'}) RETURN count(*)").contains("\n1\n"));
}

#[test]
fn show_and_grep_answer_and_verify_in_the_same_batch() {
    let out = cg(&["q", "--db", db(), "-e", "MATCH (s:Symbol {name:'on_created'}) RETURN s.lang", "-e", "SHOW poly_server::store::MemStore::put, on_creatd",
                   "-e", "GREP \\.publish\\( IN worker/**/*.py"]);
    assert!(out.contains("### Q1") && out.contains("python"), "{out}");
    assert!(out.contains("== poly_server::store::MemStore::put (Method) server/src/store.rs:13-15"), "{out}");
    assert!(out.contains("self.docs.push(doc.to_string());"), "{out}");
    assert!(out.contains("similar names: on_created"), "{out}");
    // the test publisher counts as a test owner, the production one as non-test
    assert!(out.contains("2 hits in 1 files (1 searched via "), "{out}");
    assert!(out.contains("0 hits in comments); enclosing symbols outside comments: 1 non-test, 1 test"), "{out}");
    // a scope that matches nothing is an error, never an empty answer
    let none = cg(&["q", "--db", db(), "-e", "GREP x IN nope/**"]);
    assert!(none.contains("ERROR no file matches IN nope/**"), "{none}");
    assert!(out.contains("worker/app.py:46 | app::on_created | False | False | nc.publish(\"stats.updated\", str(n))"), "{out}");
}

#[test]
fn dispatches_reaches_implementations_in_every_language() {
    let out = q("MATCH (a:Symbol)-[:DISPATCHES]->(b:Symbol) RETURN a.path, b.path ORDER BY a.path");
    for row in [
        "app::Counter::bump | app::MemCounter::bump",
        "poly_server::store::Store::put | poly_server::store::MemStore::put",
        "poly_server::store::Store::all | poly_server::store::<MemStore as Store>::all",
        "web/src/api.ts::Source::list | web/src/api.ts::HttpSource::list",
    ] {
        assert!(out.contains(row), "missing `{row}` in\n{out}");
    }
}

#[test]
fn graph_dir_is_git_ignored() {
    let ignore = std::path::Path::new(db()).join(".gitignore");
    assert_eq!(std::fs::read_to_string(ignore).unwrap(), "*\n");
}

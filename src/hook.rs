//! `cg hook`: a Claude Code PostToolUse hook (matcher `Bash|Grep`). When an agent text-searches code for a
//! symbol inside a repo that has a graph, it answers with the recipe that resolves the same question
//! through the compiler's index. At most once per 20 minutes per session; silent otherwise.

use regex::Regex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

const QUIET_SECS: u64 = 20 * 60;

static DECL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:fn|impl|struct|trait|enum|class|def|interface|type)\s+(?:\\s\+)?([A-Za-z_]\w*)").unwrap());
static CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)\s*\\?\(").unwrap());
static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([A-Za-z_]\w*(?:::[A-Za-z_]\w*)+)").unwrap());
static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\\b)?([A-Za-z_]\w{3,})(?:\\b)?$").unwrap());
static CD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\s*cd\s+("[^"]+"|'[^']+'|\S+)\s*&&"#).unwrap());

/// Shell words with '…' and "…" quoting; pipes and separators become their own words.
fn words(cmd: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    for ch in cmd.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => (quote, any) = (Some(ch), true),
            None if ch.is_whitespace() || matches!(ch, '|' | ';' | '&' | '(' | ')') => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
                if !ch.is_whitespace() {
                    out.push(ch.to_string());
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// The symbol a regex searches for, if it looks like a code symbol rather than text.
fn symbol_in(pattern: &str) -> Option<String> {
    let p = pattern.trim();
    if let Some(m) = DECL.captures(p) {
        return Some(m[1].to_string());
    }
    if let Some(m) = CALL.captures(p).filter(|m| !matches!(&m[1], "if" | "for" | "while" | "match" | "return")) {
        return Some(m[1].trim_start_matches("self::").to_string());
    }
    if let Some(m) = PATHLIKE.captures(p) {
        return Some(m[1].to_string());
    }
    let id = IDENT.captures(p)?.get(1)?.as_str();
    // snake_case or CamelCase: an identifier, not prose (`error`) or a marker (`TODO`)
    let camel = id.chars().next().is_some_and(char::is_uppercase) && id.chars().any(char::is_lowercase);
    (id.contains('_') && id.chars().any(char::is_lowercase) || camel).then(|| id.to_string())
}

/// The symbol a recursive code search in `cmd` looks for. Output filters (`… | grep x`) are not searches.
pub fn searched_symbol(cmd: &str) -> Option<String> {
    let w = words(cmd);
    for (i, word) in w.iter().enumerate() {
        let tool = match word.as_str() {
            "grep" | "egrep" if i > 0 && w[i - 1] == "git" => "git",
            "grep" | "egrep" => "grep",
            "rg" => "rg",
            _ => continue,
        };
        if i > 0 && w[i - 1] == "|" {
            continue;
        }
        let args: Vec<&String> = w[i + 1..].iter().take_while(|a| !matches!(a.as_str(), "|" | ";" | "&" | ")")).collect();
        let recursive = tool != "grep"
            || args.iter().any(|a| a.starts_with("--include") || a.as_str() == "--recursive"
                || (a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R'])));
        if !recursive {
            continue;
        }
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "-e" | "--regexp" => return it.next().and_then(|p| symbol_in(p)),
                "-A" | "-B" | "-C" | "-m" | "--max-count" | "-g" | "--glob" | "-t" | "--type" | "-f" => {
                    it.next();
                }
                s if s.starts_with('-') => {}
                pattern => return symbol_in(pattern),
            }
        }
    }
    None
}

fn graph_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|d| d.join(".cg/db/graph.lbug").exists()).map(Path::to_path_buf)
}

/// Hook output for one PostToolUse event, or None to stay silent.
pub fn respond(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let cwd = PathBuf::from(v["cwd"].as_str().unwrap_or("."));
    let (sym, dir) = match v["tool_name"].as_str()? {
        "Bash" => {
            let cmd = v["tool_input"]["command"].as_str()?;
            let dir = CD.captures(cmd).map_or(cwd.clone(), |m| cwd.join(m[1].trim_matches(['"', '\''])));
            (searched_symbol(cmd)?, dir)
        }
        "Grep" => {
            let dir = v["tool_input"]["path"].as_str().map_or(cwd.clone(), |p| cwd.join(p));
            (symbol_in(v["tool_input"]["pattern"].as_str()?)?, dir)
        }
        _ => return None,
    };
    let root = graph_root(&dir)?;
    let session: String = v["session_id"].as_str().unwrap_or("none").chars().filter(char::is_ascii_alphanumeric).collect();
    let stamp = std::env::temp_dir().join(format!("cg-hook-{session}"));
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let last: u64 = std::fs::read_to_string(&stamp).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if now.saturating_sub(last) < QUIET_SECS {
        return None;
    }
    std::fs::write(&stamp, now.to_string()).ok()?;
    let msg = format!(
        "cg: {} has a code graph. For `{sym}`, one call answers what text search misses or over-matches \
         (trait dispatch, macros, same-named methods): cd {} && cg q -e 'CALLERS {sym};' -e 'IMPACT {sym};' \
         (also IMPLS, TESTS, REACH, ROUTES; `HELP;` lists them). Then read the files it names. \
         Strings, config and docs stay with grep.",
        root.display(),
        root.display()
    );
    Some(json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": msg}}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_searches_are_recognised() {
        for (cmd, want) in [
            (r#"grep -rn "fn create_entity" crates/"#, "create_entity"),
            (r#"cd /w/a && grep -rn --include=*.rs "MemStore::put" ."#, "MemStore::put"),
            (r#"rg -n 'store\.put\(' crates"#, "put"),
            (r#"git grep -n "policy::gate(""#, "policy::gate"),
            ("grep -rn -A 3 delete_entity src", "delete_entity"),
            ("grep -rn -e 'impl Store for' src", "Store"),
            ("rg -t rust EntityStore", "EntityStore"),
        ] {
            assert_eq!(searched_symbol(cmd).as_deref(), Some(want), "{cmd}");
        }
    }

    #[test]
    fn text_searches_and_output_filters_are_not() {
        for cmd in [
            "cargo test 2>&1 | grep -E 'test result'",
            "grep -rn TODO src",
            r#"grep -rn "error" ."#,
            "grep create_entity notes.txt",
            "git log --oneline | rg fix",
            r#"grep -rn "if (x" src"#,
            "ls -la",
        ] {
            assert_eq!(searched_symbol(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn silent_without_a_graph_and_for_other_tools() {
        let dir = std::env::temp_dir().join(format!("cg-hook-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let ev = |tool: &str, cwd: &Path| {
            json!({"session_id": format!("t{}", std::process::id()), "tool_name": tool, "cwd": cwd,
                   "tool_input": {"command": "grep -rn create_entity src", "pattern": "create_entity"}}).to_string()
        };
        assert_eq!(respond(&ev("Bash", &dir)), None, "no graph yet");
        std::fs::create_dir_all(dir.join(".cg/db")).unwrap();
        std::fs::write(dir.join(".cg/db/graph.lbug"), "").unwrap();
        assert_eq!(respond(&ev("Read", &dir)), None);
        let out = respond(&ev("Bash", &dir.join("src"))).expect("hint");
        assert!(out.contains("CALLERS create_entity;") && out.contains("PostToolUse"), "{out}");
        assert_eq!(respond(&ev("Grep", &dir)), None, "quiet period");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("cg-hook-t{}", std::process::id())));
    }
}

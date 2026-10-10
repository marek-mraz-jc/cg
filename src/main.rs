//! cg: a Cypher-queryable code graph built from SCIP indexes (Rust, TypeScript, Python) with
//! per-project service links. Agents ask many structural questions in one call.

mod build;
mod lang;
mod links;
mod query;
mod scip;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "cg", version, about = "Cypher code graph for Rust / TypeScript / Python repos", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index the repo (runs rust-analyzer / scip-typescript / scip-python) and load the graph
    Build {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        db: Option<PathBuf>,
        /// reuse SCIP files from the previous build; an index whose sources changed since is rebuilt
        #[arg(long)]
        reuse: bool,
        /// Rust dependencies to index into the graph too (comma-separated crate names; also `deps`
        /// in .cg/config.toml); cached per version in ~/.cache/cg/deps
        #[arg(long, value_delimiter = ',')]
        deps: Vec<String>,
    },
    /// Node/relationship tables with counts, conventions and example queries
    Schema {
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Run statements: Cypher, `SHOW <symbol|file:a-b>, ...`, `GREP <regex> [IN <glob>]`; -e each, then FILE
    /// (or - for stdin); statements end with ';' at line end
    Q {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(short = 'e')]
        e: Vec<String>,
        file: Option<String>,
        #[arg(long, default_value_t = 60)]
        max_rows: usize,
        /// seconds per statement
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
}

/// --db, else $CG_DB, else `db` from <repo>/.cg/config.toml, else <git toplevel or cwd>/.cg/db.
fn resolve_db(db: Option<PathBuf>, repo: &Path) -> Result<PathBuf> {
    if let Some(d) = db.or_else(|| std::env::var_os("CG_DB").map(PathBuf::from)) {
        return Ok(d);
    }
    let cfg = build::load_config(repo)?;
    Ok(repo.join(cfg.db.unwrap_or_else(|| ".cg/db".into())))
}

/// Nearest ancestor of the working directory holding `.cg/`, else the git toplevel, else cwd.
fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(dir) = cwd.ancestors().find(|d| d.join(".cg").is_dir()) {
        return dir.to_path_buf();
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build { repo, db, reuse, deps } => {
            let repo = repo.canonicalize().with_context(|| format!("repo {}", repo.display()))?;
            let db = resolve_db(db, &repo)?;
            let db = if db.is_absolute() { db } else { std::env::current_dir()?.join(db) };
            build::build(&build::Opts { repo, db, reuse, deps })
        }
        Cmd::Schema { db } => query::schema(&resolve_db(db, &project_root())?),
        Cmd::Q { db, e, file, max_rows, timeout } => {
            let mut text: String = e.iter().map(|s| format!("{};\n", s.trim_end().trim_end_matches(';'))).collect();
            match file.as_deref() {
                Some("-") => {
                    std::io::stdin().read_to_string(&mut text)?;
                }
                Some(f) => text += &std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?,
                None if text.trim().is_empty() => {
                    std::io::stdin().read_to_string(&mut text)?;
                }
                None => {}
            }
            query::run(&resolve_db(db, &project_root())?, &text, max_rows, timeout)
        }
    }
}

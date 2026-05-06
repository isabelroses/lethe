use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS machines (
    id INTEGER PRIMARY KEY,
    identifier TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_machines_identifier ON machines(identifier);

CREATE TABLE IF NOT EXISTS store_paths (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    closure_size INTEGER NOT NULL,
    nar_size INTEGER NOT NULL,
    deriver TEXT,
    nar_hash TEXT NOT NULL,
    valid INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_store_paths_path ON store_paths(path);
CREATE INDEX IF NOT EXISTS idx_store_paths_deriver ON store_paths(deriver);
CREATE INDEX IF NOT EXISTS idx_store_paths_nar_hash ON store_paths(nar_hash);

CREATE TABLE IF NOT EXISTS deployments (
    id INTEGER PRIMARY KEY,
    target_machine_id INTEGER NOT NULL REFERENCES machines(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    toplevel TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_deployments_toplevel ON deployments(toplevel);
CREATE INDEX IF NOT EXISTS idx_deployments_machine ON deployments(target_machine_id);

CREATE TABLE IF NOT EXISTS closures (
    store_path_id INTEGER NOT NULL REFERENCES store_paths(id),
    deployment_id INTEGER NOT NULL REFERENCES deployments(id),
    PRIMARY KEY (store_path_id, deployment_id)
);

CREATE TABLE IF NOT EXISTS refs (
    referrer_id INTEGER NOT NULL REFERENCES store_paths(id),
    referenced_id INTEGER NOT NULL REFERENCES store_paths(id),
    PRIMARY KEY (referrer_id, referenced_id)
);
";

pub fn open(override_path: Option<PathBuf>) -> Result<Connection> {
    let path = match override_path {
        Some(p) => p,
        None => default_path()?,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&path)
        .with_context(|| format!("opening database at {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA).context("initializing schema")?;
    Ok(conn)
}

fn default_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "lethe")
        .context("could not determine data directory")?;
    Ok(dirs.data_dir().join("lethe.db"))
}

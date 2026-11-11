use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub(crate) const SCHEMA: &str = r"
-- `UNIQUE` already indexes the column, so no explicit index is declared for
-- `machines.identifier` or `store_paths.path`.
CREATE TABLE IF NOT EXISTS machines (
    id INTEGER PRIMARY KEY,
    identifier TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS store_paths (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    nar_size INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deployments (
    id INTEGER PRIMARY KEY,
    target_machine_id INTEGER NOT NULL REFERENCES machines(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    toplevel TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_deployments_machine_toplevel ON deployments(target_machine_id, toplevel);
CREATE INDEX IF NOT EXISTS idx_deployments_machine_id ON deployments(target_machine_id, id DESC);

CREATE TABLE IF NOT EXISTS closures (
    store_path_id INTEGER NOT NULL REFERENCES store_paths(id),
    deployment_id INTEGER NOT NULL REFERENCES deployments(id),
    selected INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (store_path_id, deployment_id)
);

-- Reads filter on `deployment_id`, which the primary key sorts second and so
-- cannot serve. The trailing columns make this index covering.
CREATE INDEX IF NOT EXISTS idx_closures_deployment
    ON closures(deployment_id, store_path_id, selected);
";

const SCHEMA_VERSION: i32 = 1;

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

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 268435456;
         PRAGMA cache_size = -64000;",
    )?;

    ensure_schema(&conn)?;
    Ok(conn)
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    conn.execute_batch(SCHEMA).context("initializing schema")?;
    migrate(conn).context("migrating schema")?;
    conn.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), [])?;
    Ok(())
}

/// Applies schema changes that `CREATE TABLE IF NOT EXISTS` can't express
/// against an already-existing database, since it is a no-op once the table
/// is there.
fn migrate(conn: &Connection) -> Result<()> {
    if !has_column(conn, "closures", "selected")? {
        conn.execute("ALTER TABLE closures ADD COLUMN selected INTEGER NOT NULL DEFAULT 0", [])?;
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS refs;
         DROP INDEX IF EXISTS idx_store_paths_deriver;
         DROP INDEX IF EXISTS idx_store_paths_nar_hash;
         DROP INDEX IF EXISTS idx_deployments_toplevel;
         DROP INDEX IF EXISTS idx_deployments_machine;
         DROP INDEX IF EXISTS idx_store_paths_path;
         DROP INDEX IF EXISTS idx_machines_identifier;",
    )?;

    for column in ["closure_size", "deriver", "nar_hash", "valid"] {
        if has_column(conn, "store_paths", column)? {
            conn.execute(&format!("ALTER TABLE store_paths DROP COLUMN {column}"), [])?;
        }
    }

    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?
        .exists(rusqlite::params![table, column])?)
}

fn default_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "lethe")
        .context("could not determine data directory")?;
    Ok(dirs.data_dir().join("lethe.db"))
}

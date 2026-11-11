//! Storage for machines, deployments and their closures. All SQL lives here;
//! callers speak in machines and deployments.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use dix::{Size, StorePath, StorePathInfo, StoreSnapshot};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = r"
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
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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

#[cfg(test)]
pub mod testing {
    use super::*;

    pub fn open_in_memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    pub fn path(index: usize, name: &str) -> StorePath {
        StorePath::try_from(format!("/nix/store/{index:032}-{name}")).unwrap()
    }

    pub fn snapshot(paths: &[(&StorePath, i64)], selected: &[&StorePath]) -> StoreSnapshot {
        StoreSnapshot {
            closure: paths
                .iter()
                .map(|(path, size)| StorePathInfo {
                    path: (*path).clone(),
                    nar_size: Size::from_bytes(*size),
                })
                .collect(),
            selected: selected.iter().map(|path| (*path).clone()).collect(),
        }
    }
}

pub struct Machine {
    pub identifier: String,
    pub deployments: i64,
    pub last_deployed: Option<String>,
}

#[derive(Debug)]
pub struct Deployment {
    pub id: i64,
    pub machine_id: i64,
    pub machine: String,
    pub created_at: String,
    pub toplevel: String,
    /// Sum of the NAR sizes of the closure, in bytes.
    pub size: i64,
}

pub fn machines(conn: &Connection) -> Result<Vec<Machine>> {
    let mut stmt = conn.prepare(
        "SELECT m.identifier, COUNT(d.id), MAX(d.created_at)
         FROM machines m
         LEFT JOIN deployments d ON d.target_machine_id = m.id
         GROUP BY m.id
         ORDER BY m.identifier",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Machine { identifier: r.get(0)?, deployments: r.get(1)?, last_deployed: r.get(2)? })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The deployments of a machine, newest first.
pub fn deployments(conn: &Connection, machine: &str) -> Result<Vec<Deployment>> {
    find_machine(conn, machine)?.with_context(|| format!("unknown machine: {machine}"))?;
    find_deployments(conn, "m.identifier = ?1 ORDER BY d.id DESC", machine)
}

pub fn deployment(conn: &Connection, id: i64) -> Result<Deployment> {
    find_deployments(conn, "d.id = ?1", id)?
        .pop()
        .with_context(|| format!("unknown deployment: {id}"))
}

pub fn closure_len(conn: &Connection, deployment_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM closures WHERE deployment_id = ?1",
        params![deployment_id],
        |r| r.get(0),
    )?)
}

/// Fail if `toplevel` is already the most recent deployment of `machine`.
pub fn ensure_new(conn: &Connection, machine: &str, toplevel: &str) -> Result<()> {
    let latest = find_deployments(conn, "m.identifier = ?1 ORDER BY d.id DESC LIMIT 1", machine)?;
    if latest.first().is_some_and(|d| d.toplevel == toplevel) {
        bail!("machine {machine} already has this system as the most recent deployment");
    }
    Ok(())
}

/// Store a new deployment of `machine` and its closure, creating the machine
/// on first sight. Refuses a repeat of the machine's latest deployment.
pub fn record(
    conn: &Connection,
    machine: &str,
    toplevel: &str,
    snapshot: &StoreSnapshot,
) -> Result<Deployment> {
    let tx = conn.unchecked_transaction()?;
    ensure_new(&tx, machine, toplevel)?;
    let machine_id = upsert_machine(&tx, machine)?;
    let id = insert_deployment(&tx, machine_id, toplevel, snapshot)?;
    tx.commit()?;
    deployment(conn, id)
}

/// Resolve the two sides of a diff. With only `old` given, it is either a
/// deployment (diffed against the latest of its machine) or a machine
/// (whose last two deployments are diffed).
pub fn resolve_diff_pair(
    conn: &Connection,
    old: &str,
    new: Option<&str>,
) -> Result<(Deployment, Deployment)> {
    if let Some(new) = new {
        return Ok((resolve_deployment(conn, old)?, resolve_deployment(conn, new)?));
    }

    if let Some(old_dep) = try_resolve_deployment(conn, old)? {
        let new_dep = latest_deployment_for_machine(conn, old_dep.machine_id)?
            .with_context(|| format!("no deployments for machine {}", old_dep.machine))?;
        return Ok((old_dep, new_dep));
    }

    let machine_id = find_machine(conn, old)?
        .with_context(|| format!("no deployment, toplevel, or machine matches: {old}"))?;
    let mut rows =
        find_deployments(conn, "d.target_machine_id = ?1 ORDER BY d.id DESC LIMIT 2", machine_id)?;
    // Newest first, so popping yields old then new.
    match (rows.pop(), rows.pop()) {
        (Some(old_dep), Some(new_dep)) => Ok((old_dep, new_dep)),
        (first, _) => bail!(
            "machine {old} has fewer than two deployments ({})",
            usize::from(first.is_some())
        ),
    }
}

/// Load a recorded closure back into the same shape `dix` produces when
/// querying a live store.
pub fn load_snapshot(conn: &Connection, deployment_id: i64) -> Result<StoreSnapshot> {
    let mut stmt = conn.prepare_cached(
        "SELECT sp.path, sp.nar_size, c.selected
         FROM closures c
         JOIN store_paths sp ON sp.id = c.store_path_id
         WHERE c.deployment_id = ?1",
    )?;
    let mut rows = stmt.query(params![deployment_id])?;

    let mut closure = Vec::with_capacity(1024);
    let mut selected = Vec::new();
    while let Some(r) = rows.next()? {
        let path = StorePath::try_from(r.get::<_, String>(0)?)
            .map_err(|e| anyhow!("invalid store path: {e}"))?;
        if r.get(2)? {
            selected.push(path.clone());
        }
        closure.push(StorePathInfo { path, nar_size: Size::from_bytes(r.get::<_, i64>(1)?) });
    }

    Ok(StoreSnapshot { closure, selected })
}

fn find_machine(conn: &Connection, identifier: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT id FROM machines WHERE identifier = ?1")?
        .query_row(params![identifier], |r| r.get(0))
        .optional()?)
}

/// Deployments matching `filter`: the body of a `WHERE` clause over
/// `deployments d` and `machines m`, plus any `ORDER BY`/`LIMIT`, with one
/// `?1` parameter.
fn find_deployments(
    conn: &Connection,
    filter: &str,
    param: impl rusqlite::ToSql,
) -> Result<Vec<Deployment>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT d.id, d.target_machine_id, m.identifier, d.created_at, d.toplevel, d.size
         FROM deployments d
         JOIN machines m ON m.id = d.target_machine_id
         WHERE {filter}"
    ))?;
    let rows = stmt
        .query_map(params![param], |r| {
            Ok(Deployment {
                id: r.get(0)?,
                machine_id: r.get(1)?,
                machine: r.get(2)?,
                created_at: r.get(3)?,
                toplevel: r.get(4)?,
                size: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn latest_deployment_for_machine(conn: &Connection, machine_id: i64) -> Result<Option<Deployment>> {
    Ok(find_deployments(conn, "d.target_machine_id = ?1 ORDER BY d.id DESC LIMIT 1", machine_id)?
        .pop())
}

fn try_resolve_deployment(conn: &Connection, reference: &str) -> Result<Option<Deployment>> {
    if let Ok(id) = reference.parse::<i64>() {
        return Ok(find_deployments(conn, "d.id = ?1", id)?.pop());
    }

    let mut rows = find_deployments(conn, "d.toplevel = ?1 ORDER BY d.id DESC", reference)?;
    if rows.len() > 1 {
        let ids: Vec<String> = rows.iter().map(|r| r.id.to_string()).collect();
        bail!(
            "toplevel {reference} matches {} deployments (ids: {}); pass an id to disambiguate",
            rows.len(),
            ids.join(", "),
        );
    }
    Ok(rows.pop())
}

fn resolve_deployment(conn: &Connection, reference: &str) -> Result<Deployment> {
    try_resolve_deployment(conn, reference)?
        .with_context(|| format!("no deployment found with id or toplevel: {reference}"))
}

fn upsert_machine(conn: &Connection, identifier: &str) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "INSERT INTO machines (identifier) VALUES (?1)
             ON CONFLICT(identifier) DO UPDATE SET id = id
             RETURNING id",
        )?
        .query_row(params![identifier], |r| r.get(0))?)
}

/// The caller provides the transaction.
fn insert_deployment(
    conn: &Connection,
    machine_id: i64,
    toplevel: &str,
    snapshot: &StoreSnapshot,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO deployments (target_machine_id, toplevel) VALUES (?1, ?2)",
        params![machine_id, toplevel],
    )?;
    let deployment_id = conn.last_insert_rowid();

    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS staged_closure (
             path TEXT NOT NULL,
             nar_size INTEGER NOT NULL,
             selected INTEGER NOT NULL
         );",
    )?;

    {
        let mut stage = conn.prepare_cached(
            "INSERT INTO staged_closure (path, nar_size, selected) VALUES (?1, ?2, ?3)",
        )?;
        let selected: HashSet<_> = snapshot.selected.iter().collect();
        for sp in &snapshot.closure {
            stage.execute(params![
                sp.path.as_str(),
                sp.nar_size.bytes(),
                selected.contains(&sp.path)
            ])?;
        }
    }

    // The WHERE clause disambiguates SELECT ... ON CONFLICT in SQLite.
    conn.execute(
        "INSERT INTO store_paths (path, nar_size)
         SELECT path, nar_size FROM staged_closure WHERE true
         ORDER BY rowid
         ON CONFLICT(path) DO UPDATE SET nar_size = excluded.nar_size
         WHERE store_paths.nar_size != excluded.nar_size",
        [],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO closures (store_path_id, deployment_id, selected)
         SELECT sp.id, ?1, sc.selected
         FROM staged_closure sc
         JOIN store_paths sp ON sp.path = sc.path",
        params![deployment_id],
    )?;
    conn.execute("DELETE FROM staged_closure", [])?;

    // Summed from the stored rows, so the size always matches the closure.
    conn.execute(
        "UPDATE deployments SET size = (
             SELECT COALESCE(SUM(sp.nar_size), 0)
             FROM closures c
             JOIN store_paths sp ON sp.id = c.store_path_id
             WHERE c.deployment_id = ?1
         ) WHERE id = ?1",
        params![deployment_id],
    )?;
    Ok(deployment_id)
}

#[cfg(test)]
mod tests {
    use super::testing::{open_in_memory, path as test_path, snapshot as test_snapshot};
    use super::*;

    #[test]
    fn schema_initialization_records_version_and_preserves_data() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        conn.execute("INSERT INTO machines (identifier) VALUES ('host')", []).unwrap();

        ensure_schema(&conn).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM machines", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn record_derives_the_size_and_refuses_a_repeat_of_the_latest() {
        let conn = open_in_memory();
        let first = test_path(0, "system");
        let second = test_path(1, "system");
        let dep = test_path(2, "dependency");
        let snapshot =
            |top: &StorePath| test_snapshot(&[(top, 100), (&dep, 200), (&dep, 200)], &[&dep]);

        let recorded = record(&conn, "host-a", first.as_str(), &snapshot(&first)).unwrap();
        assert_eq!(recorded.machine, "host-a");
        assert_eq!(recorded.size, 300, "a path listed twice is stored and counted once");
        assert_eq!(closure_len(&conn, recorded.id).unwrap(), 2);

        let err = record(&conn, "host-a", first.as_str(), &snapshot(&first)).unwrap_err();
        assert!(err.to_string().contains("already has this system"), "{err:#}");
        assert_eq!(deployments(&conn, "host-a").unwrap().len(), 1, "a refusal stores nothing");

        // Another machine may record the same system, and going back to an
        // earlier system after a different one is a new deployment.
        record(&conn, "host-b", first.as_str(), &snapshot(&first)).unwrap();
        record(&conn, "host-a", second.as_str(), &snapshot(&second)).unwrap();
        record(&conn, "host-a", first.as_str(), &snapshot(&first)).unwrap();
        let ids: Vec<i64> = deployments(&conn, "host-a").unwrap().iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.is_sorted_by(|a, b| a > b), "newest first: {ids:?}");

        let listed = machines(&conn).unwrap();
        assert_eq!(
            listed.iter().map(|m| (m.identifier.as_str(), m.deployments)).collect::<Vec<_>>(),
            [("host-a", 3), ("host-b", 1)]
        );
        assert!(deployments(&conn, "host-c").unwrap_err().to_string().contains("unknown machine"));
    }

    fn add_machine(conn: &Connection, identifier: &str) -> i64 {
        upsert_machine(conn, identifier).unwrap()
    }

    fn add_deployment(conn: &Connection, machine_id: i64, toplevel: &str, size: i64) -> i64 {
        conn.execute(
            "INSERT INTO deployments (target_machine_id, toplevel, size) VALUES (?1, ?2, ?3)",
            params![machine_id, toplevel, size],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn add_closure_path(
        conn: &Connection,
        deployment_id: i64,
        path: &str,
        nar_size: i64,
        selected: bool,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO store_paths (path, nar_size) VALUES (?1, ?2)",
            params![path, nar_size],
        )
        .unwrap();
        let store_path_id: i64 = conn
            .query_row("SELECT id FROM store_paths WHERE path = ?1", params![path], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO closures (store_path_id, deployment_id, selected) VALUES (?1, ?2, ?3)",
            params![store_path_id, deployment_id, i64::from(selected)],
        )
        .unwrap();
    }

    #[test]
    fn load_snapshot_separates_selected_from_dependencies() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let d = add_deployment(&conn, m, "/nix/store/aaa-system", 300);
        let selected_path = "/nix/store/00000000000000000000000000000000-foo-1.0";
        add_closure_path(&conn, d, selected_path, 100, true);
        add_closure_path(
            &conn,
            d,
            "/nix/store/00000000000000000000000000000001-bar-2.0",
            200,
            false,
        );

        let snapshot = load_snapshot(&conn, d).unwrap();

        assert_eq!(snapshot.closure.len(), 2);
        assert_eq!(snapshot.selected.len(), 1);
        assert_eq!(snapshot.selected[0].to_str().unwrap(), selected_path);
    }

    #[test]
    fn load_snapshot_rejects_paths_outside_the_nix_store() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let d = add_deployment(&conn, m, "/nix/store/aaa-system", 100);
        add_closure_path(&conn, d, "/etc/not-a-store-path", 100, false);

        let err = load_snapshot(&conn, d).unwrap_err();
        assert!(err.to_string().contains("invalid store path"));
    }

    #[test]
    fn insert_deployment_updates_changed_sizes_and_reuses_existing_paths() {
        let conn = open_in_memory();
        let machine = upsert_machine(&conn, "host-a").unwrap();
        let changed = test_path(0, "system");
        let unchanged = test_path(1, "dependency");
        let snapshot = |changed_size| {
            test_snapshot(&[(&changed, changed_size), (&unchanged, 200)], &[&changed])
        };
        let first = insert_deployment(&conn, machine, changed.as_str(), &snapshot(100))
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE size_updates (path TEXT);
             CREATE TEMP TRIGGER count_size_updates AFTER UPDATE ON store_paths
             BEGIN INSERT INTO size_updates VALUES (NEW.path); END;",
        )
        .unwrap();

        let second = insert_deployment(&conn, machine, changed.as_str(), &snapshot(150))
            .unwrap();

        for id in [first, second] {
            let loaded = load_snapshot(&conn, id).unwrap();
            assert_eq!(loaded.closure.len(), 2);
            assert_eq!(loaded.selected, vec![changed.clone()]);
            assert_eq!(
                loaded.closure.iter().find(|p| p.path == changed).unwrap().nar_size.bytes(),
                150
            );
            assert_eq!(
                loaded.closure.iter().find(|p| p.path == unchanged).unwrap().nar_size.bytes(),
                200
            );
        }
        let updated: Vec<String> = conn
            .prepare("SELECT path FROM size_updates")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(updated, vec![changed.as_str()]);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM store_paths", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn insert_deployment_uses_last_size_for_duplicate_paths() {
        let conn = open_in_memory();
        let machine = upsert_machine(&conn, "host-a").unwrap();
        let path = test_path(0, "system");
        let snapshot = test_snapshot(&[(&path, 100), (&path, 150)], &[&path]);
        let deployment = insert_deployment(&conn, machine, path.as_str(), &snapshot).unwrap();
        let loaded = load_snapshot(&conn, deployment).unwrap();
        assert_eq!(loaded.closure.len(), 1);
        assert_eq!(loaded.closure[0].nar_size.bytes(), 150);
        assert_eq!(loaded.selected, vec![path]);
    }

    #[test]
    fn insert_deployment_preserves_selected_paths() {
        let conn = open_in_memory();
        let machine = upsert_machine(&conn, "host-a").unwrap();
        let paths: Vec<_> = (0..7).map(|i| test_path(i, "package")).collect();

        for selected_count in [0, 1, 6] {
            let snapshot = StoreSnapshot {
                closure: paths
                    .iter()
                    .cloned()
                    .map(|path| StorePathInfo {
                        path,
                        nar_size: Size::from_bytes(100),
                    })
                    .collect(),
                selected: paths[..selected_count].to_vec(),
            };
            let id = insert_deployment(&conn, machine, paths[0].as_str(), &snapshot).unwrap();
            let loaded = load_snapshot(&conn, id).unwrap();
            assert_eq!(loaded.closure.len(), paths.len());
            assert_eq!(
                loaded.selected.iter().collect::<HashSet<_>>(),
                snapshot.selected.iter().collect()
            );
        }
    }

    #[test]
    fn resolve_by_id() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);

        let dep = resolve_deployment(&conn, &d1.to_string()).unwrap();
        assert_eq!(dep.id, d1);
        assert_eq!(dep.toplevel, "/nix/store/aaa-system");
    }

    #[test]
    fn resolve_by_toplevel() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);

        let dep = resolve_deployment(&conn, "/nix/store/aaa-system").unwrap();
        assert_eq!(dep.id, d1);
    }

    #[test]
    fn resolve_unknown_ref_errors() {
        let conn = open_in_memory();
        add_machine(&conn, "host-a");

        let err = resolve_deployment(&conn, "/nix/store/nope-system").unwrap_err();
        assert!(err.to_string().contains("no deployment"));
    }

    #[test]
    fn resolve_ambiguous_toplevel_errors() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        add_deployment(&conn, m, "/nix/store/dup-system", 100);
        add_deployment(&conn, m, "/nix/store/dup-system", 200);

        let err = resolve_deployment(&conn, "/nix/store/dup-system").unwrap_err();
        assert!(err.to_string().contains("matches 2 deployments"));
    }

    #[test]
    fn diff_pair_two_args_resolves_both() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);
        let d2 = add_deployment(&conn, m, "/nix/store/bbb-system", 200);

        let (old, new) =
            resolve_diff_pair(&conn, &d1.to_string(), Some("/nix/store/bbb-system")).unwrap();
        assert_eq!(old.id, d1);
        assert_eq!(new.id, d2);
    }

    #[test]
    fn diff_pair_single_id_uses_latest_for_machine() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);
        let _d2 = add_deployment(&conn, m, "/nix/store/bbb-system", 200);
        let d3 = add_deployment(&conn, m, "/nix/store/ccc-system", 300);

        let (old, new) = resolve_diff_pair(&conn, &d1.to_string(), None).unwrap();
        assert_eq!(old.id, d1);
        assert_eq!(new.id, d3);
    }

    #[test]
    fn diff_pair_machine_name_uses_last_two() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        let _d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);
        let d2 = add_deployment(&conn, m, "/nix/store/bbb-system", 200);
        let d3 = add_deployment(&conn, m, "/nix/store/ccc-system", 300);

        let (old, new) = resolve_diff_pair(&conn, "host-a", None).unwrap();
        assert_eq!(old.id, d2);
        assert_eq!(new.id, d3);
    }

    #[test]
    fn diff_pair_machine_with_one_deployment_errors() {
        let conn = open_in_memory();
        let m = add_machine(&conn, "host-a");
        add_deployment(&conn, m, "/nix/store/aaa-system", 100);

        let err = resolve_diff_pair(&conn, "host-a", None).unwrap_err();
        assert!(err.to_string().contains("fewer than two deployments"));
    }

    #[test]
    fn diff_pair_unknown_single_ref_errors() {
        let conn = open_in_memory();
        add_machine(&conn, "host-a");

        let err = resolve_diff_pair(&conn, "host-b", None).unwrap_err();
        assert!(err.to_string().contains("no deployment, toplevel, or machine"));
    }

    #[test]
    fn diff_pair_id_lookup_does_not_match_unrelated_machine() {
        let conn = open_in_memory();
        let m_a = add_machine(&conn, "host-a");
        let m_b = add_machine(&conn, "host-b");
        let d_a = add_deployment(&conn, m_a, "/nix/store/aaa-system", 100);
        let _d_b1 = add_deployment(&conn, m_b, "/nix/store/bbb-system", 200);
        let _d_b2 = add_deployment(&conn, m_b, "/nix/store/ccc-system", 300);

        let (old, new) = resolve_diff_pair(&conn, &d_a.to_string(), None).unwrap();
        assert_eq!(old.id, d_a);
        assert_eq!(new.id, d_a);
        assert_eq!(new.machine_id, m_a);
    }
}

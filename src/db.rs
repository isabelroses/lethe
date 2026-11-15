//! Storage for machines, deployments and their closures. All SQL lives here;
//! callers speak in machines and deployments.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use dix::{Size, StorePath, StorePathInfo, StoreSnapshot};
use rusqlite::{Connection, OptionalExtension, params};

const TABLES: &str = r"
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

-- `id` is the key closures point at. `number` counts the deployments of one
-- machine from 1 and is the name users see. `uuid` names the deployment on
-- every device it is synced to; nullable only because `ALTER TABLE` cannot add
-- a `NOT NULL` column without a default, and `migrate` fills it in.
CREATE TABLE IF NOT EXISTS deployments (
    id INTEGER PRIMARY KEY,
    target_machine_id INTEGER NOT NULL REFERENCES machines(id),
    number INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    toplevel TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    uuid TEXT
);

CREATE TABLE IF NOT EXISTS closures (
    deployment_id INTEGER NOT NULL REFERENCES deployments(id),
    store_path_id INTEGER NOT NULL REFERENCES store_paths(id),
    selected INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (deployment_id, store_path_id)
) WITHOUT ROWID;

-- How far this device has synced with each server: the server sequence it
-- has pulled up to, and the local deployment id it has pushed up to.
CREATE TABLE IF NOT EXISTS sync_state (
    remote TEXT PRIMARY KEY,
    last_pulled_seq INTEGER NOT NULL DEFAULT 0,
    last_pushed_id INTEGER NOT NULL DEFAULT 0
);
";

/// Created after `migrate`, which adds the columns they cover to databases
/// that predate them.
const INDEXES: &str = r"
CREATE INDEX IF NOT EXISTS idx_deployments_machine_toplevel ON deployments(target_machine_id, toplevel);
CREATE UNIQUE INDEX IF NOT EXISTS idx_deployments_machine_number ON deployments(target_machine_id, number);
CREATE UNIQUE INDEX IF NOT EXISTS idx_deployments_uuid ON deployments(uuid);

";

const SCHEMA_VERSION: i32 = 3;

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

    // One transaction, so an interrupted upgrade leaves the old schema intact.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(TABLES).context("initializing schema")?;
    migrate(&tx).context("migrating schema")?;
    tx.execute_batch(INDEXES).context("creating indexes")?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;

    // Rebuilding a table leaves the old copy's pages free; hand them back
    // once rather than carrying a file twice the size of its data.
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    if free * 4 > pages {
        conn.execute_batch("VACUUM").context("compacting the database after upgrading it")?;
    }
    Ok(())
}

/// Applies schema changes that `CREATE TABLE IF NOT EXISTS` can't express
/// against an already-existing database, since it is a no-op once the table
/// is there.
fn migrate(conn: &Connection) -> Result<()> {
    if !has_column(conn, "closures", "selected")? {
        conn.execute("ALTER TABLE closures ADD COLUMN selected INTEGER NOT NULL DEFAULT 0", [])?;
    }

    // Closures used to be a rowid table with a primary key led by the store
    // path, plus an index by deployment for the reads. Keyed by deployment
    // without a rowid, each row lives in one b-tree instead of three, which
    // halves the cost of storing a closure and shrinks the file.
    let without_rowid: bool = conn.query_row(
        "SELECT wr FROM pragma_table_list WHERE schema = 'main' AND name = 'closures'",
        [],
        |r| r.get(0),
    )?;
    if !without_rowid {
        conn.execute_batch(
            "CREATE TABLE closures_by_deployment (
                 deployment_id INTEGER NOT NULL REFERENCES deployments(id),
                 store_path_id INTEGER NOT NULL REFERENCES store_paths(id),
                 selected INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (deployment_id, store_path_id)
             ) WITHOUT ROWID;
             INSERT INTO closures_by_deployment (deployment_id, store_path_id, selected)
                 SELECT deployment_id, store_path_id, selected FROM closures
                 ORDER BY deployment_id, store_path_id;
             DROP TABLE closures;
             ALTER TABLE closures_by_deployment RENAME TO closures;",
        )?;
    }

    if !has_column(conn, "deployments", "number")? {
        conn.execute_batch(
            "ALTER TABLE deployments ADD COLUMN number INTEGER NOT NULL DEFAULT 0;
             UPDATE deployments SET number = (
                 SELECT COUNT(*) FROM deployments earlier
                 WHERE earlier.target_machine_id = deployments.target_machine_id
                   AND earlier.id <= deployments.id
             );",
        )?;
    }

    if !has_column(conn, "deployments", "uuid")? {
        conn.execute("ALTER TABLE deployments ADD COLUMN uuid TEXT", [])?;
        // In id order, so the time-ordered uuids keep the recorded order.
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM deployments ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut set = conn.prepare("UPDATE deployments SET uuid = ?1 WHERE id = ?2")?;
        for id in ids {
            set.execute(params![new_uuid(), id])?;
        }
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS refs;
         DROP INDEX IF EXISTS idx_store_paths_deriver;
         DROP INDEX IF EXISTS idx_store_paths_nar_hash;
         DROP INDEX IF EXISTS idx_deployments_toplevel;
         DROP INDEX IF EXISTS idx_deployments_machine;
         DROP INDEX IF EXISTS idx_deployments_machine_id;
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

/// A fresh deployment identity. v7 is time-ordered, so uuids made on one
/// device sort in the order they were made.
fn new_uuid() -> String {
    uuid::Uuid::now_v7().hyphenated().to_string()
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
    /// Internal key; users see `number`.
    pub id: i64,
    /// Position among the deployments of its machine, counted from 1.
    pub number: i64,
    pub machine: String,
    /// In the local time zone.
    pub created_at: String,
    pub toplevel: String,
    /// Sum of the NAR sizes of the closure, in bytes.
    pub size: i64,
}

pub fn machines(conn: &Connection) -> Result<Vec<Machine>> {
    let mut stmt = conn.prepare(
        "SELECT m.identifier, COUNT(d.id), datetime(MAX(d.created_at), 'localtime')
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
    let machine_id = require_machine(conn, machine)?;
    find_deployments(conn, "d.target_machine_id = ?1 ORDER BY d.number DESC", [machine_id])
}

/// One deployment of `machine`, by number or toplevel; the latest when no
/// reference is given.
pub fn resolve(conn: &Connection, machine: &str, reference: Option<&str>) -> Result<Deployment> {
    let machine_id = require_machine(conn, machine)?;
    match reference {
        Some(reference) => resolve_in(conn, machine_id, machine, reference),
        None => require_latest(conn, machine_id, machine),
    }
}

pub fn closure_len(conn: &Connection, deployment_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM closures WHERE deployment_id = ?1",
        params![deployment_id],
        |r| r.get(0),
    )?)
}

/// The latest deployment of `machine`, if it is already `toplevel`.
pub fn unchanged(conn: &Connection, machine: &str, toplevel: &str) -> Result<Option<Deployment>> {
    let Some(machine_id) = find_machine(conn, machine)? else {
        return Ok(None);
    };
    Ok(latest(conn, machine_id)?.filter(|d| d.toplevel == toplevel))
}

/// Store a new deployment of `machine` and its closure, creating the machine
/// on first sight. Refuses a repeat of the machine's latest deployment;
/// callers check `unchanged` first, so this only fires if another process
/// recorded the same system in between.
pub fn record(
    conn: &Connection,
    machine: &str,
    toplevel: &str,
    snapshot: &StoreSnapshot,
) -> Result<Deployment> {
    let tx = conn.unchecked_transaction()?;
    if let Some(existing) = unchanged(&tx, machine, toplevel)? {
        bail!("already recorded as deployment {} for {machine}", existing.number);
    }
    let machine_id = upsert_machine(&tx, machine)?;
    let uuid = new_uuid();
    let id = insert_deployment(&tx, &Origin::fresh(&uuid), machine_id, toplevel, snapshot)?;
    tx.commit()?;
    find_deployments(conn, "d.id = ?1", [id])?.pop().context("the new deployment is missing")
}

/// The two sides of a diff, both from `machine`. With no reference its last
/// two deployments are compared; with only `old`, that one against the
/// latest.
pub fn resolve_diff_pair(
    conn: &Connection,
    machine: &str,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<(Deployment, Deployment)> {
    let machine_id = require_machine(conn, machine)?;
    let Some(old) = old else {
        let mut rows = find_deployments(
            conn,
            "d.target_machine_id = ?1 ORDER BY d.number DESC LIMIT 2",
            [machine_id],
        )?;
        // Newest first, so popping yields old then new.
        return match (rows.pop(), rows.pop()) {
            (Some(old), Some(new)) => Ok((old, new)),
            (first, _) => {
                first.with_context(|| format!("machine {machine} has no deployments"))?;
                bail!("machine {machine} has only one deployment, so there is nothing to diff")
            }
        };
    };

    let old = resolve_in(conn, machine_id, machine, old)?;
    let new = if let Some(new) = new {
        resolve_in(conn, machine_id, machine, new)?
    } else {
        let newest = require_latest(conn, machine_id, machine)?;
        if newest.id == old.id {
            bail!(
                "deployment {} is the latest of {machine}, so there is nothing newer to diff it against",
                old.number
            );
        }
        newest
    };
    Ok((old, new))
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

/// A deployment as it travels between devices: named by its uuid, with no
/// local id or number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portable {
    pub uuid: String,
    pub machine: String,
    /// UTC, in `SQLite`'s `datetime('now')` format.
    pub created_at: String,
    pub toplevel: String,
    pub snapshot: StoreSnapshot,
}

/// Deployments with an id above `after_id`, oldest first, each with its id.
pub fn portable_after(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> Result<Vec<(i64, Portable)>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    find_portables(conn, "d.id > ?1 ORDER BY d.id LIMIT ?2", params![after_id, limit])
}

/// The deployments named by `uuids` that exist here, in no particular order.
pub fn portables_by_uuid(conn: &Connection, uuids: &[String]) -> Result<Vec<Portable>> {
    let mut found = Vec::with_capacity(uuids.len());
    for uuid in uuids {
        found.extend(find_portables(conn, "d.uuid = ?1", [uuid])?.into_iter().map(|(_, d)| d));
    }
    Ok(found)
}

/// The uuids of deployments with an id above `after_id`, oldest first, each
/// with its id: a cheap listing for clients to pick from.
pub fn uuids_after(conn: &Connection, after_id: i64, limit: usize) -> Result<Vec<(i64, String)>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    Ok(conn
        .prepare_cached(
            "SELECT id, uuid FROM deployments WHERE id > ?1 AND uuid IS NOT NULL
             ORDER BY id LIMIT ?2",
        )?
        .query_map(params![after_id, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Which of `uuids` this database has never seen, in the given order.
pub fn unknown_uuids(conn: &Connection, uuids: &[String]) -> Result<Vec<String>> {
    let mut known = conn.prepare_cached("SELECT 1 FROM deployments WHERE uuid = ?1")?;
    let mut unknown = Vec::new();
    for uuid in uuids {
        if !known.exists([uuid])? {
            unknown.push(uuid.clone());
        }
    }
    Ok(unknown)
}

/// Deployments matching `filter`, as in [`find_deployments`], with their ids.
fn find_portables(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(i64, Portable)>> {
    let rows: Vec<(i64, String, String, String, String)> = conn
        .prepare_cached(&format!(
            "SELECT d.id, d.uuid, m.identifier, d.created_at, d.toplevel
             FROM deployments d
             JOIN machines m ON m.id = d.target_machine_id
             WHERE {filter}"
        ))?
        .query_map(params, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(id, uuid, machine, created_at, toplevel)| {
            let snapshot = load_snapshot(conn, id)?;
            Ok((id, Portable { uuid, machine, created_at, toplevel, snapshot }))
        })
        .collect()
}

/// Store the deployments whose uuid is new here, keeping their creation
/// time, and return how many that was. The caller provides the transaction.
///
/// Each machine that gained a deployment is renumbered by creation time, so
/// one pulled from another device slots in where it happened rather than
/// becoming the latest.
pub fn import(conn: &Connection, deployments: &[Portable]) -> Result<usize> {
    let mut known = conn.prepare_cached("SELECT 1 FROM deployments WHERE uuid = ?1")?;
    let mut touched = HashSet::new();
    let mut inserted = 0;
    for d in deployments {
        if known.exists(params![d.uuid])? {
            continue;
        }
        let machine_id = upsert_machine(conn, &d.machine)?;
        let origin = Origin { uuid: &d.uuid, created_at: Some(&d.created_at) };
        insert_deployment(conn, &origin, machine_id, &d.toplevel, &d.snapshot)
            .with_context(|| format!("importing deployment {}", d.uuid))?;
        touched.insert(machine_id);
        inserted += 1;
    }
    for machine_id in touched {
        renumber(conn, machine_id)?;
    }
    Ok(inserted)
}

/// Where syncing with one server left off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursors {
    /// The highest server sequence pulled.
    pub pulled: i64,
    /// The highest local deployment id pushed or known to be on the server.
    pub pushed: i64,
}

pub fn cursors(conn: &Connection, remote: &str) -> Result<Cursors> {
    Ok(conn
        .query_row(
            "SELECT last_pulled_seq, last_pushed_id FROM sync_state WHERE remote = ?1",
            params![remote],
            |r| Ok(Cursors { pulled: r.get(0)?, pushed: r.get(1)? }),
        )
        .optional()?
        .unwrap_or_default())
}

pub fn save_cursors(conn: &Connection, remote: &str, cursors: Cursors) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (remote, last_pulled_seq, last_pushed_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(remote) DO UPDATE
         SET last_pulled_seq = excluded.last_pulled_seq, last_pushed_id = excluded.last_pushed_id",
        params![remote, cursors.pulled, cursors.pushed],
    )?;
    Ok(())
}

/// Store a page pulled from `remote`, whose last row has sequence `seq`, and
/// advance the cursors with it in one transaction. Returns how many rows
/// were new here.
pub fn store_pulled(
    conn: &Connection,
    remote: &str,
    deployments: &[Portable],
    seq: i64,
    cursors: &mut Cursors,
) -> Result<usize> {
    // Immediate, so no local `record` can slip in between reading the newest
    // id and writing the cursors back.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let newest = |tx: &Connection| {
        tx.query_row("SELECT COALESCE(MAX(id), 0) FROM deployments", [], |r| r.get::<_, i64>(0))
    };
    let nothing_unpushed = cursors.pushed == newest(&tx)?;
    let inserted = import(&tx, deployments)?;
    cursors.pulled = seq;
    // Rows just pulled came from the server, so skip past them, but only
    // when nothing local is still waiting to be pushed beneath them.
    if nothing_unpushed {
        cursors.pushed = newest(&tx)?;
    }
    save_cursors(&tx, remote, *cursors)?;
    tx.commit()?;
    Ok(inserted)
}

/// Number a machine's deployments by creation time, so every device holding
/// the same deployments numbers them alike. Ties fall to the uuid, which is
/// the same everywhere, where the local id is not.
fn renumber(conn: &Connection, machine_id: i64) -> Result<()> {
    // Only rows whose number moves are written, which for a deployment that is
    // simply the newest is none. `number` is unique per machine and checked
    // row by row, so those rows park on the negated target first, where they
    // cannot collide with each other or with rows that keep their number.
    conn.prepare_cached(
        "UPDATE deployments SET number = -ranked.number
         FROM (
             SELECT id, ROW_NUMBER() OVER (ORDER BY created_at, uuid) AS number
             FROM deployments WHERE target_machine_id = ?1
         ) AS ranked
         WHERE deployments.id = ranked.id AND deployments.number != ranked.number",
    )?
    .execute(params![machine_id])?;
    conn.prepare_cached(
        "UPDATE deployments SET number = -number WHERE target_machine_id = ?1 AND number < 0",
    )?
    .execute(params![machine_id])?;
    Ok(())
}

fn find_machine(conn: &Connection, identifier: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT id FROM machines WHERE identifier = ?1")?
        .query_row(params![identifier], |r| r.get(0))
        .optional()?)
}

fn require_machine(conn: &Connection, machine: &str) -> Result<i64> {
    if let Some(id) = find_machine(conn, machine)? {
        return Ok(id);
    }
    let known: Vec<String> = machines(conn)?.into_iter().map(|m| m.identifier).collect();
    if known.is_empty() {
        bail!("unknown machine: {machine} (nothing has been recorded yet)");
    }
    bail!("unknown machine: {machine} (known: {})", known.join(", "));
}

/// Deployments matching `filter`: the body of a `WHERE` clause over
/// `deployments d` and `machines m`, plus any `ORDER BY`/`LIMIT`.
fn find_deployments(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Deployment>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT d.id, d.number, m.identifier, datetime(d.created_at, 'localtime'),
                d.toplevel, d.size
         FROM deployments d
         JOIN machines m ON m.id = d.target_machine_id
         WHERE {filter}"
    ))?;
    let rows = stmt
        .query_map(params, |r| {
            Ok(Deployment {
                id: r.get(0)?,
                number: r.get(1)?,
                machine: r.get(2)?,
                created_at: r.get(3)?,
                toplevel: r.get(4)?,
                size: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn latest(conn: &Connection, machine_id: i64) -> Result<Option<Deployment>> {
    Ok(find_deployments(
        conn,
        "d.target_machine_id = ?1 ORDER BY d.number DESC LIMIT 1",
        [machine_id],
    )?
    .pop())
}

fn require_latest(conn: &Connection, machine_id: i64, machine: &str) -> Result<Deployment> {
    latest(conn, machine_id)?.with_context(|| format!("machine {machine} has no deployments"))
}

/// A deployment of one machine by number or toplevel.
fn resolve_in(
    conn: &Connection,
    machine_id: i64,
    machine: &str,
    reference: &str,
) -> Result<Deployment> {
    let mut rows = if let Ok(number) = reference.parse::<i64>() {
        find_deployments(
            conn,
            "d.target_machine_id = ?1 AND d.number = ?2",
            params![machine_id, number],
        )?
    } else {
        find_deployments(
            conn,
            "d.target_machine_id = ?1 AND d.toplevel = ?2 ORDER BY d.number",
            params![machine_id, reference],
        )?
    };
    if rows.len() > 1 {
        let numbers: Vec<String> = rows.iter().map(|d| d.number.to_string()).collect();
        bail!(
            "toplevel {reference} matches deployments {} of {machine}; pass a number to pick one",
            numbers.join(", "),
        );
    }
    if let Some(deployment) = rows.pop() {
        return Ok(deployment);
    }
    let newest = require_latest(conn, machine_id, machine)?;
    bail!("machine {machine} has no deployment {reference} (the latest is {})", newest.number)
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

/// Where a deployment comes from: made here just now, or carried over from
/// another device with the identity and time it was made with.
struct Origin<'a> {
    uuid: &'a str,
    /// `None` for now.
    created_at: Option<&'a str>,
}

impl Origin<'_> {
    fn fresh(uuid: &str) -> Origin<'_> {
        Origin { uuid, created_at: None }
    }
}

/// The caller provides the transaction.
fn insert_deployment(
    conn: &Connection,
    origin: &Origin,
    machine_id: i64,
    toplevel: &str,
    snapshot: &StoreSnapshot,
) -> Result<i64> {
    // An aggregate without GROUP BY always yields one row, so this inserts
    // number 1 for a machine with no deployments yet.
    conn.execute(
        "INSERT INTO deployments (target_machine_id, number, toplevel, uuid, created_at)
         SELECT ?1, COALESCE(MAX(number), 0) + 1, ?2, ?3, COALESCE(?4, datetime('now'))
         FROM deployments WHERE target_machine_id = ?1",
        params![machine_id, toplevel, origin.uuid, origin.created_at],
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
    fn record_numbers_each_machine_separately_and_refuses_a_repeat_of_the_latest() {
        let conn = open_in_memory();
        let first = test_path(0, "system");
        let second = test_path(1, "system");
        let dep = test_path(2, "dependency");
        let snapshot =
            |top: &StorePath| test_snapshot(&[(top, 100), (&dep, 200), (&dep, 200)], &[&dep]);

        assert!(unchanged(&conn, "host-a", first.as_str()).unwrap().is_none());
        let recorded = record(&conn, "host-a", first.as_str(), &snapshot(&first)).unwrap();
        assert_eq!((recorded.machine.as_str(), recorded.number), ("host-a", 1));
        assert_eq!(recorded.size, 300, "a path listed twice is stored and counted once");
        assert_eq!(closure_len(&conn, recorded.id).unwrap(), 2);

        assert_eq!(unchanged(&conn, "host-a", first.as_str()).unwrap().unwrap().number, 1);
        let err = record(&conn, "host-a", first.as_str(), &snapshot(&first)).unwrap_err();
        assert_eq!(err.to_string(), "already recorded as deployment 1 for host-a");
        assert_eq!(deployments(&conn, "host-a").unwrap().len(), 1, "a refusal stores nothing");

        // Another machine may record the same system and starts at 1 again;
        // going back to an earlier system after a different one is a new
        // deployment.
        let other = record(&conn, "host-b", first.as_str(), &snapshot(&first)).unwrap();
        assert_eq!(other.number, 1);
        record(&conn, "host-a", second.as_str(), &snapshot(&second)).unwrap();
        assert!(unchanged(&conn, "host-a", first.as_str()).unwrap().is_none());
        record(&conn, "host-a", first.as_str(), &snapshot(&first)).unwrap();
        let numbers: Vec<i64> =
            deployments(&conn, "host-a").unwrap().iter().map(|d| d.number).collect();
        assert_eq!(numbers, [3, 2, 1], "newest first");

        let listed = machines(&conn).unwrap();
        assert_eq!(
            listed.iter().map(|m| (m.identifier.as_str(), m.deployments)).collect::<Vec<_>>(),
            [("host-a", 3), ("host-b", 1)]
        );
        let err = deployments(&conn, "host-c").unwrap_err().to_string();
        assert_eq!(err, "unknown machine: host-c (known: host-a, host-b)");
        let err = deployments(&open_in_memory(), "host-c").unwrap_err().to_string();
        assert_eq!(err, "unknown machine: host-c (nothing has been recorded yet)");
    }

    #[test]
    fn upgrading_numbers_existing_deployments_per_machine() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE machines (id INTEGER PRIMARY KEY, identifier TEXT NOT NULL UNIQUE);
             CREATE TABLE deployments (
                 id INTEGER PRIMARY KEY,
                 target_machine_id INTEGER NOT NULL REFERENCES machines(id),
                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 toplevel TEXT NOT NULL,
                 size INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX idx_deployments_machine_id ON deployments(target_machine_id, id DESC);
             INSERT INTO machines (identifier) VALUES ('host-a'), ('host-b');
             INSERT INTO deployments (target_machine_id, toplevel) VALUES
                 (1, '/nix/store/a1'), (2, '/nix/store/b1'), (1, '/nix/store/a2'),
                 (2, '/nix/store/b2'), (1, '/nix/store/a3');
             PRAGMA user_version = 1;",
        )
        .unwrap();

        ensure_schema(&conn).unwrap();

        let numbered = |machine: &str| -> Vec<(i64, String)> {
            let mut rows = deployments(&conn, machine).unwrap();
            rows.reverse();
            rows.into_iter().map(|d| (d.number, d.toplevel)).collect()
        };
        assert_eq!(
            numbered("host-a"),
            [(1, "/nix/store/a1".into()), (2, "/nix/store/a2".into()), (3, "/nix/store/a3".into())]
        );
        assert_eq!(numbered("host-b"), [(1, "/nix/store/b1".into()), (2, "/nix/store/b2".into())]);
        let uuids: Vec<String> = conn
            .prepare("SELECT uuid FROM deployments ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(uuids.len(), 5);
        assert!(uuids.is_sorted(), "backfilled in id order: {uuids:?}");
        let next = insert(&conn, 2, "/nix/store/b3", &test_snapshot(&[], &[]));
        assert_eq!(resolve(&conn, "host-b", None).unwrap().id, next);
        assert_eq!(resolve(&conn, "host-b", None).unwrap().number, 3);
    }

    fn insert(conn: &Connection, machine_id: i64, toplevel: &str, snapshot: &StoreSnapshot) -> i64 {
        insert_deployment(conn, &Origin::fresh(&new_uuid()), machine_id, toplevel, snapshot).unwrap()
    }

    #[test]
    fn upgrading_rekeys_closures_by_deployment() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE machines (id INTEGER PRIMARY KEY, identifier TEXT NOT NULL UNIQUE);
             CREATE TABLE store_paths (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, nar_size INTEGER NOT NULL);
             CREATE TABLE deployments (
                 id INTEGER PRIMARY KEY,
                 target_machine_id INTEGER NOT NULL REFERENCES machines(id),
                 number INTEGER NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 toplevel TEXT NOT NULL,
                 size INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE closures (
                 store_path_id INTEGER NOT NULL REFERENCES store_paths(id),
                 deployment_id INTEGER NOT NULL REFERENCES deployments(id),
                 selected INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (store_path_id, deployment_id)
             );
             CREATE INDEX idx_closures_deployment ON closures(deployment_id, store_path_id, selected);
             INSERT INTO machines (identifier) VALUES ('host-a');
             INSERT INTO store_paths (path, nar_size) VALUES
                 ('/nix/store/00000000000000000000000000000000-foo', 100),
                 ('/nix/store/00000000000000000000000000000001-bar', 200);
             INSERT INTO deployments (target_machine_id, number, toplevel) VALUES (1, 1, '/nix/store/a');
             INSERT INTO closures VALUES (1, 1, 1), (2, 1, 0);
             PRAGMA user_version = 2;",
        )
        .unwrap();

        ensure_schema(&conn).unwrap();

        let wr: bool = conn
            .query_row("SELECT wr FROM pragma_table_list WHERE name = 'closures'", [], |r| r.get(0))
            .unwrap();
        assert!(wr);
        let snapshot = load_snapshot(&conn, 1).unwrap();
        assert_eq!(snapshot.closure.len(), 2);
        assert_eq!(snapshot.selected.len(), 1);
        let leftover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('idx_closures_deployment', 'closures_by_deployment')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0);
    }

    fn add_machine(conn: &Connection, identifier: &str) -> i64 {
        upsert_machine(conn, identifier).unwrap()
    }

    fn add_deployment(conn: &Connection, machine_id: i64, toplevel: &str, size: i64) -> i64 {
        conn.execute(
            "INSERT INTO deployments (target_machine_id, number, toplevel, size)
             SELECT ?1, COALESCE(MAX(number), 0) + 1, ?2, ?3
             FROM deployments WHERE target_machine_id = ?1",
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
        let first = insert(&conn, machine, changed.as_str(), &snapshot(100));
        conn.execute_batch(
            "CREATE TEMP TABLE size_updates (path TEXT);
             CREATE TEMP TRIGGER count_size_updates AFTER UPDATE ON store_paths
             BEGIN INSERT INTO size_updates VALUES (NEW.path); END;",
        )
        .unwrap();

        let second = insert(&conn, machine, changed.as_str(), &snapshot(150));

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
        let deployment = insert(&conn, machine, path.as_str(), &snapshot);
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
            let id = insert(&conn, machine, paths[0].as_str(), &snapshot);
            let loaded = load_snapshot(&conn, id).unwrap();
            assert_eq!(loaded.closure.len(), paths.len());
            assert_eq!(
                loaded.selected.iter().collect::<HashSet<_>>(),
                snapshot.selected.iter().collect()
            );
        }
    }

    /// `host-a` with three deployments and `host-b` with one.
    fn two_machines() -> Connection {
        let conn = open_in_memory();
        let a = add_machine(&conn, "host-a");
        let b = add_machine(&conn, "host-b");
        add_deployment(&conn, a, "/nix/store/aaa-system", 100);
        add_deployment(&conn, b, "/nix/store/zzz-system", 100);
        add_deployment(&conn, a, "/nix/store/bbb-system", 200);
        add_deployment(&conn, a, "/nix/store/ccc-system", 300);
        conn
    }

    #[test]
    fn resolve_accepts_a_number_or_a_toplevel_and_defaults_to_the_latest() {
        let conn = two_machines();
        let toplevel = |reference| resolve(&conn, "host-a", reference).unwrap().toplevel;

        assert_eq!(toplevel(Some("2")), "/nix/store/bbb-system");
        assert_eq!(toplevel(Some("/nix/store/aaa-system")), "/nix/store/aaa-system");
        assert_eq!(toplevel(None), "/nix/store/ccc-system");
        assert_eq!(resolve(&conn, "host-b", Some("1")).unwrap().toplevel, "/nix/store/zzz-system");
    }

    #[test]
    fn resolve_stays_inside_the_machine() {
        let conn = two_machines();

        let err = resolve(&conn, "host-b", Some("2")).unwrap_err().to_string();
        assert_eq!(err, "machine host-b has no deployment 2 (the latest is 1)");
        let err = resolve(&conn, "host-b", Some("/nix/store/aaa-system")).unwrap_err().to_string();
        assert!(err.starts_with("machine host-b has no deployment /nix/store/aaa-system"), "{err}");
        let err = resolve(&conn, "host-c", None).unwrap_err().to_string();
        assert_eq!(err, "unknown machine: host-c (known: host-a, host-b)");
    }

    #[test]
    fn resolve_asks_for_a_number_when_a_toplevel_repeats() {
        let conn = two_machines();
        let a = find_machine(&conn, "host-a").unwrap().unwrap();
        add_deployment(&conn, a, "/nix/store/aaa-system", 100);

        let err = resolve(&conn, "host-a", Some("/nix/store/aaa-system")).unwrap_err().to_string();
        assert_eq!(
            err,
            "toplevel /nix/store/aaa-system matches deployments 1, 4 of host-a; \
             pass a number to pick one"
        );
    }

    #[test]
    fn diff_pair_defaults_to_the_last_two_then_to_the_latest() {
        let conn = two_machines();
        let numbers = |old, new| {
            let (old, new) = resolve_diff_pair(&conn, "host-a", old, new).unwrap();
            (old.number, new.number)
        };

        assert_eq!(numbers(None, None), (2, 3));
        assert_eq!(numbers(Some("1"), None), (1, 3));
        assert_eq!(numbers(Some("1"), Some("/nix/store/bbb-system")), (1, 2));
        assert_eq!(numbers(Some("3"), Some("1")), (3, 1));
    }

    #[test]
    fn diff_pair_explains_when_there_is_nothing_to_compare() {
        let conn = two_machines();
        let error = |machine, old, new| {
            resolve_diff_pair(&conn, machine, old, new).unwrap_err().to_string()
        };

        assert_eq!(
            error("host-b", None, None),
            "machine host-b has only one deployment, so there is nothing to diff"
        );
        assert_eq!(
            error("host-a", Some("3"), None),
            "deployment 3 is the latest of host-a, so there is nothing newer to diff it against"
        );
        assert_eq!(
            error("host-b", Some("1"), Some("3")),
            "machine host-b has no deployment 3 (the latest is 1)"
        );
        assert_eq!(error("host-c", None, None), "unknown machine: host-c (known: host-a, host-b)");
    }

    fn portable(machine: &str, toplevel: &StorePath, created_at: &str) -> Portable {
        let dep = test_path(9, "dependency");
        Portable {
            uuid: new_uuid(),
            machine: machine.into(),
            created_at: created_at.into(),
            toplevel: toplevel.as_str().into(),
            snapshot: test_snapshot(&[(toplevel, 100), (&dep, 200)], &[&dep]),
        }
    }

    #[test]
    fn import_keeps_identity_and_time_and_skips_known_uuids() {
        let conn = open_in_memory();
        let d = portable("host-a", &test_path(0, "system"), "2020-01-02 03:04:05");

        assert_eq!(import(&conn, std::slice::from_ref(&d)).unwrap(), 1);
        assert_eq!(import(&conn, std::slice::from_ref(&d)).unwrap(), 0);

        let exported = portable_after(&conn, 0, 10).unwrap();
        assert_eq!(exported.len(), 1);
        let (_, mut back) = exported.into_iter().next().unwrap();
        back.snapshot.closure.sort_by(|a, b| a.path.cmp(&b.path));
        let mut want = d;
        want.snapshot.closure.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(back, want);
        assert_eq!(resolve(&conn, "host-a", None).unwrap().size, 300);
    }

    #[test]
    fn import_renumbers_by_creation_time() {
        let conn = open_in_memory();
        let (a, b, c) = (test_path(0, "system"), test_path(1, "system"), test_path(2, "system"));
        let snapshot = |top: &StorePath| test_snapshot(&[(top, 100)], &[]);
        record(&conn, "host-a", a.as_str(), &snapshot(&a)).unwrap();
        conn.execute("UPDATE deployments SET created_at = '2020-01-01 00:00:00'", []).unwrap();
        record(&conn, "host-a", c.as_str(), &snapshot(&c)).unwrap();

        // Made on another device between the two local ones.
        import(&conn, &[portable("host-a", &b, "2021-01-01 00:00:00")]).unwrap();

        let numbered: Vec<(i64, String)> = deployments(&conn, "host-a")
            .unwrap()
            .into_iter()
            .map(|d| (d.number, d.toplevel))
            .collect();
        assert_eq!(
            numbered,
            [(3, c.as_str().into()), (2, b.as_str().into()), (1, a.as_str().into())]
        );
        assert!(unchanged(&conn, "host-a", c.as_str()).unwrap().is_some(), "the latest stays");
    }

    #[test]
    fn portable_after_pages_in_id_order() {
        let conn = open_in_memory();
        let machine = add_machine(&conn, "host-a");
        for i in 0..3 {
            let top = test_path(i, "system");
            insert(&conn, machine, top.as_str(), &test_snapshot(&[(&top, 1)], &[]));
        }
        let ids = |after, limit| -> Vec<i64> {
            portable_after(&conn, after, limit).unwrap().into_iter().map(|(id, _)| id).collect()
        };
        assert_eq!(ids(0, 2), [1, 2]);
        assert_eq!(ids(2, 2), [3]);
    }
}

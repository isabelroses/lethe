use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use dix::store::StorePathInfo;
use dix::{StorePath, StoreSnapshot};
use rusqlite::{params, Connection, OptionalExtension};
use size::Size;
use tabled::settings::object::Columns;
use tabled::settings::{Alignment, Modify, Style};
use tabled::{Table, Tabled};
use tracing::info;
use yansi::Paint as _;

use crate::nix::{self, Target};

pub fn record(
    conn: &Connection,
    name_override: Option<&str>,
    target: &Target,
    system_link: &str,
) -> Result<()> {
    let machine = match name_override {
        Some(n) => n.to_string(),
        None => nix::fetch_hostname(target)?,
    };
    info!("recording {} ({})", machine, target_label(target));

    info!("resolving {system_link}");
    let toplevel = nix::resolve_toplevel(target, system_link)?;
    info!("toplevel: {toplevel}");

    info!("fetching closure");
    let snapshot = nix::fetch_snapshot(target, &toplevel)?;
    info!("{} store paths, {} selected", snapshot.closure.len(), snapshot.selected.len());

    let toplevel_size: i64 = snapshot.closure.iter().map(|p| p.nar_size().bytes()).sum();

    let machine_id = upsert_machine(conn, &machine)?;

    if let Some(previous) = latest_deployment_for_machine(conn, machine_id)?
        && previous.toplevel == toplevel
    {
        anyhow::bail!("machine {machine} already has this system as the most recent deployment");
    }

    insert_deployment(conn, machine_id, &toplevel, toplevel_size, &snapshot)?;
    println!(
        "recorded deployment for {machine} (toplevel {toplevel}, closure {})",
        Size::from_bytes(toplevel_size)
    );
    Ok(())
}

pub fn machines(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT m.identifier,
                COUNT(d.id) AS n,
                MAX(d.created_at) AS last
         FROM machines m
         LEFT JOIN deployments d ON d.target_machine_id = m.id
         GROUP BY m.id
         ORDER BY m.identifier",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MachineRow {
                machine: row.get(0)?,
                deploys: row.get(1)?,
                last: row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "-".to_string()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut table = Table::new(rows);
    table
        .with(Style::empty())
        .with(Modify::new(Columns::one(1)).with(Alignment::right()));
    println!("{table}");
    Ok(())
}

#[derive(Tabled)]
struct MachineRow {
    #[tabled(rename = "MACHINE")]
    machine: String,
    #[tabled(rename = "DEPLOYS")]
    deploys: i64,
    #[tabled(rename = "LAST")]
    last: String,
}

pub fn deployments(conn: &Connection, machine: &str) -> Result<()> {
    let machine_id: i64 = conn
        .query_row(
            "SELECT id FROM machines WHERE identifier = ?1",
            params![machine],
            |r| r.get(0),
        )
        .optional()?
        .with_context(|| format!("unknown machine: {machine}"))?;

    let mut stmt = conn.prepare(
        "SELECT id, created_at, size, toplevel
         FROM deployments
         WHERE target_machine_id = ?1
         ORDER BY id DESC",
    )?;
    let rows = stmt
        .query_map(params![machine_id], |row| {
            Ok(DeploymentListRow {
                id: row.get(0)?,
                created: row.get(1)?,
                size: Size::from_bytes(row.get::<_, i64>(2)?).to_string(),
                toplevel: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut table = Table::new(rows);
    table
        .with(Style::empty())
        .with(Modify::new(Columns::one(0)).with(Alignment::right()))
        .with(Modify::new(Columns::one(2)).with(Alignment::right()));
    println!("{table}");
    Ok(())
}

#[derive(Tabled)]
struct DeploymentListRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "CREATED")]
    created: String,
    #[tabled(rename = "SIZE")]
    size: String,
    #[tabled(rename = "TOPLEVEL")]
    toplevel: String,
}

pub fn show(conn: &Connection, id: i64) -> Result<()> {
    let row = conn
        .query_row(
            "SELECT d.id, d.created_at, m.identifier, d.toplevel, d.size
             FROM deployments d
             JOIN machines m ON m.id = d.target_machine_id
             WHERE d.id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?
        .with_context(|| format!("unknown deployment: {id}"))?;

    let (id, created, machine, top, size) = row;
    println!("Deployment {id}");
    println!("  machine:   {machine}");
    println!("  created:   {created}");
    println!("  toplevel:  {top}");
    println!("  size:      {} ({size} bytes)", Size::from_bytes(size));

    let n_paths: i64 = conn.query_row(
        "SELECT COUNT(*) FROM closures WHERE deployment_id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    println!("  closure:   {n_paths} store paths");
    Ok(())
}

pub fn diff(conn: &Connection, old: &str, new: Option<&str>) -> Result<()> {
    let (old_dep, new_dep) = resolve_diff_pair(conn, old, new)?;

    let old_snapshot = load_snapshot(conn, old_dep.id)?;
    let new_snapshot = load_snapshot(conn, new_dep.id)?;

    let report = dix::diff_store_snapshots(&old_snapshot, &new_snapshot);

    println!("{}", format!("<<< {}", old_dep.toplevel).bold().dim());
    println!("{}", format!(">>> {}", new_dep.toplevel).bold().dim());

    let mut rendered = String::new();
    dix::write_diff_report(&mut rendered, &report)?;
    print!("{rendered}");
    Ok(())
}

#[cfg_attr(test, derive(Debug))]
struct DeploymentRow {
    id: i64,
    target_machine_id: i64,
    toplevel: String,
}

/// Column order here is what [`row_to_deployment`] indexes into, so the two
/// are written next to each other and every query reuses this prefix.
const SELECT_DEPLOYMENT: &str = "SELECT id, target_machine_id, toplevel FROM deployments";

fn row_to_deployment(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeploymentRow> {
    Ok(DeploymentRow {
        id: r.get(0)?,
        target_machine_id: r.get(1)?,
        toplevel: r.get(2)?,
    })
}

fn try_resolve_deployment(conn: &Connection, reference: &str) -> Result<Option<DeploymentRow>> {
    if let Ok(id) = reference.parse::<i64>() {
        return Ok(conn
            .query_row(
                &format!("{SELECT_DEPLOYMENT} WHERE id = ?1"),
                params![id],
                row_to_deployment,
            )
            .optional()?);
    }

    let mut stmt =
        conn.prepare(&format!("{SELECT_DEPLOYMENT} WHERE toplevel = ?1 ORDER BY id DESC"))?;
    let mut rows = stmt
        .query_map(params![reference], row_to_deployment)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if rows.len() > 1 {
        let ids: Vec<String> = rows.iter().map(|r| r.id.to_string()).collect();
        anyhow::bail!(
            "toplevel {reference} matches {} deployments (ids: {}); pass an id to disambiguate",
            rows.len(),
            ids.join(", "),
        );
    }
    Ok(rows.pop())
}

fn resolve_deployment(conn: &Connection, reference: &str) -> Result<DeploymentRow> {
    try_resolve_deployment(conn, reference)?
        .with_context(|| format!("no deployment found with id or toplevel: {reference}"))
}

fn resolve_diff_pair(
    conn: &Connection,
    old: &str,
    new: Option<&str>,
) -> Result<(DeploymentRow, DeploymentRow)> {
    if let Some(new) = new {
        return Ok((resolve_deployment(conn, old)?, resolve_deployment(conn, new)?));
    }

    if let Some(old_dep) = try_resolve_deployment(conn, old)? {
        let new_dep = latest_deployment_for_machine(conn, old_dep.target_machine_id)?
            .with_context(|| format!("no deployments for machine id {}", old_dep.target_machine_id))?;
        return Ok((old_dep, new_dep));
    }

    let machine_id: i64 = conn
        .query_row(
            "SELECT id FROM machines WHERE identifier = ?1",
            params![old],
            |r| r.get(0),
        )
        .optional()?
        .with_context(|| {
            format!("no deployment, toplevel, or machine matches: {old}")
        })?;

    let mut stmt = conn.prepare(&format!(
        "{SELECT_DEPLOYMENT} WHERE target_machine_id = ?1 ORDER BY id DESC LIMIT 2"
    ))?;
    let mut rows = stmt
        .query_map(params![machine_id], row_to_deployment)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if rows.len() < 2 {
        anyhow::bail!(
            "machine {old} has fewer than two deployments ({})",
            rows.len()
        );
    }
    // Newest first, so popping yields old then new.
    let old_dep = rows.pop().expect("length checked above");
    let new_dep = rows.pop().expect("length checked above");
    Ok((old_dep, new_dep))
}

fn latest_deployment_for_machine(
    conn: &Connection,
    machine_id: i64,
) -> Result<Option<DeploymentRow>> {
    Ok(conn
        .query_row(
            &format!("{SELECT_DEPLOYMENT} WHERE target_machine_id = ?1 ORDER BY id DESC LIMIT 1"),
            params![machine_id],
            row_to_deployment,
        )
        .optional()?)
}

/// Load a recorded closure back into the same shape `dix` produces when
/// querying a live store.
fn load_snapshot(conn: &Connection, deployment_id: i64) -> Result<StoreSnapshot> {
    let mut stmt = conn.prepare(
        "SELECT sp.path, sp.nar_size, c.selected
         FROM closures c
         JOIN store_paths sp ON sp.id = c.store_path_id
         WHERE c.deployment_id = ?1",
    )?;
    let rows = stmt.query_map(params![deployment_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, bool>(2)?))
    })?;

    let mut closure = Vec::new();
    let mut selected = Vec::new();
    for row in rows {
        let (path, nar_size, is_selected) = row?;
        // `PathBuf::from(String)` reuses the buffer; passing a reference copies.
        let store_path = StorePath::try_from(PathBuf::from(path))
            .map_err(|e| anyhow!("invalid store path: {e}"))?;
        if is_selected {
            selected.push(store_path.clone());
        }
        closure.push(StorePathInfo::new(store_path, Size::from_bytes(nar_size)));
    }

    Ok(StoreSnapshot { closure, selected })
}

fn upsert_machine(conn: &Connection, identifier: &str) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO machines (identifier) VALUES (?1)",
        params![identifier],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM machines WHERE identifier = ?1",
        params![identifier],
        |r| r.get(0),
    )?)
}

fn insert_deployment(
    conn: &Connection,
    machine_id: i64,
    toplevel: &str,
    size: i64,
    snapshot: &StoreSnapshot,
) -> Result<i64> {
    let selected: HashSet<&Path> = snapshot.selected.iter().map(|p| p.as_path()).collect();

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO deployments (target_machine_id, toplevel, size) VALUES (?1, ?2, ?3)",
        params![machine_id, toplevel, size],
    )?;
    let deployment_id = tx.last_insert_rowid();

    {
        // `DO UPDATE` rather than `DO NOTHING` so `RETURNING` still yields the
        // id when the path is already known; the value written is unchanged.
        let mut upsert = tx.prepare(
            "INSERT INTO store_paths (path, nar_size) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET nar_size = excluded.nar_size
             RETURNING id",
        )?;
        let mut link = tx.prepare(
            "INSERT OR IGNORE INTO closures (store_path_id, deployment_id, selected)
             VALUES (?1, ?2, ?3)",
        )?;

        for sp in &snapshot.closure {
            let path = sp.path().to_str().context("non-UTF8 store path")?;
            let store_path_id: i64 =
                upsert.query_row(params![path, sp.nar_size().bytes()], |r| r.get(0))?;
            let is_selected = selected.contains(sp.path().as_path());
            link.execute(params![store_path_id, deployment_id, i64::from(is_selected)])?;
        }
    }

    tx.commit()?;
    Ok(deployment_id)
}

fn target_label(target: &Target) -> String {
    match target {
        Target::Local => "local system".to_string(),
        Target::Ssh(t) => t.store_url(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(db::SCHEMA).unwrap();
        conn
    }

    fn add_machine(conn: &Connection, identifier: &str) -> i64 {
        conn.execute(
            "INSERT INTO machines (identifier) VALUES (?1)",
            params![identifier],
        )
        .unwrap();
        conn.last_insert_rowid()
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
        let conn = fresh_conn();
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
        let conn = fresh_conn();
        let m = add_machine(&conn, "host-a");
        let d = add_deployment(&conn, m, "/nix/store/aaa-system", 100);
        add_closure_path(&conn, d, "/etc/not-a-store-path", 100, false);

        let err = load_snapshot(&conn, d).unwrap_err();
        assert!(err.to_string().contains("invalid store path"));
    }

    #[test]
    fn resolve_by_id() {
        let conn = fresh_conn();
        let m = add_machine(&conn, "host-a");
        let d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);

        let dep = resolve_deployment(&conn, &d1.to_string()).unwrap();
        assert_eq!(dep.id, d1);
        assert_eq!(dep.toplevel, "/nix/store/aaa-system");
    }

    #[test]
    fn resolve_by_toplevel() {
        let conn = fresh_conn();
        let m = add_machine(&conn, "host-a");
        let d1 = add_deployment(&conn, m, "/nix/store/aaa-system", 100);

        let dep = resolve_deployment(&conn, "/nix/store/aaa-system").unwrap();
        assert_eq!(dep.id, d1);
    }

    #[test]
    fn resolve_unknown_ref_errors() {
        let conn = fresh_conn();
        add_machine(&conn, "host-a");

        let err = resolve_deployment(&conn, "/nix/store/nope-system").unwrap_err();
        assert!(err.to_string().contains("no deployment"));
    }

    #[test]
    fn resolve_ambiguous_toplevel_errors() {
        let conn = fresh_conn();
        let m = add_machine(&conn, "host-a");
        add_deployment(&conn, m, "/nix/store/dup-system", 100);
        add_deployment(&conn, m, "/nix/store/dup-system", 200);

        let err = resolve_deployment(&conn, "/nix/store/dup-system").unwrap_err();
        assert!(err.to_string().contains("matches 2 deployments"));
    }

    #[test]
    fn diff_pair_two_args_resolves_both() {
        let conn = fresh_conn();
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
        let conn = fresh_conn();
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
        let conn = fresh_conn();
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
        let conn = fresh_conn();
        let m = add_machine(&conn, "host-a");
        add_deployment(&conn, m, "/nix/store/aaa-system", 100);

        let err = resolve_diff_pair(&conn, "host-a", None).unwrap_err();
        assert!(err.to_string().contains("fewer than two deployments"));
    }

    #[test]
    fn diff_pair_unknown_single_ref_errors() {
        let conn = fresh_conn();
        add_machine(&conn, "host-a");

        let err = resolve_diff_pair(&conn, "host-b", None).unwrap_err();
        assert!(err.to_string().contains("no deployment, toplevel, or machine"));
    }

    #[test]
    fn diff_pair_id_lookup_does_not_match_unrelated_machine() {
        let conn = fresh_conn();
        let m_a = add_machine(&conn, "host-a");
        let m_b = add_machine(&conn, "host-b");
        let d_a = add_deployment(&conn, m_a, "/nix/store/aaa-system", 100);
        let _d_b1 = add_deployment(&conn, m_b, "/nix/store/bbb-system", 200);
        let _d_b2 = add_deployment(&conn, m_b, "/nix/store/ccc-system", 300);

        let (old, new) = resolve_diff_pair(&conn, &d_a.to_string(), None).unwrap();
        assert_eq!(old.id, d_a);
        assert_eq!(new.id, d_a);
        assert_eq!(new.target_machine_id, m_a);
    }
}

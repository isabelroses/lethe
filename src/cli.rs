use anyhow::{Context, Result};
use lix_diff::{PackageListDiff, color};
use rusqlite::{params, Connection, OptionalExtension};
use tabled::settings::object::Columns;
use tabled::settings::{Alignment, Modify, Style};
use tabled::{Table, Tabled};
use tracing::info;

use crate::diff::{self, ClosurePathRef};
use crate::nix::{self, StorePathInfo, Target};

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
    let closure = nix::fetch_closure(target, system_link)?;
    info!("{} store paths", closure.len());

    let toplevel_size = closure
        .iter()
        .find(|p| p.path == toplevel)
        .map(|p| p.closure_size)
        .with_context(|| format!("toplevel {toplevel} not found in closure output"))?;

    let machine_id = upsert_machine(conn, &machine)?;

    let last = conn
        .query_row(
            "SELECT id, toplevel FROM deployments
             WHERE target_machine_id = ?1
             ORDER BY id DESC LIMIT 1",
            params![machine_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;

    if let Some((_id, prev_toplevel)) = &last
        && prev_toplevel == &toplevel {
            anyhow::bail!(
                "machine {machine} already has this system as the most recent deployment"
            );
        }

    insert_deployment(conn, machine_id, &toplevel, toplevel_size, &closure)?;
    println!(
        "recorded deployment for {machine} (toplevel {toplevel}, closure {})",
        format_size(toplevel_size)
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
                size: format_size(row.get(2)?),
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
    println!("  size:      {} ({} bytes)", format_size(size), size);

    let n_paths: i64 = conn.query_row(
        "SELECT COUNT(*) FROM closures WHERE deployment_id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    println!("  closure:   {n_paths} store paths");
    Ok(())
}

pub fn diff(conn: &Connection, old_id: i64, new_id: Option<i64>) -> Result<()> {
    let old_dep = load_deployment(conn, old_id)?;
    let new_dep = match new_id {
        Some(new_id) => load_deployment(conn, new_id)?,
        None => conn.query_row(
            "SELECT id, target_machine_id, toplevel, size, created_at
             FROM deployments
             WHERE target_machine_id = ?1
             ORDER BY id DESC LIMIT 1",
            params![old_dep.target_machine_id],
            |r| {
                Ok(DeploymentRow {
                    id: r.get(0)?,
                    target_machine_id: r.get(1)?,
                    toplevel: r.get(2)?,
                    size: r.get(3)?,
                    created_at: r.get(4)?,
                })
            },
        )?,
    };

    let new_paths = load_closure_paths(conn, new_dep.id)?;
    let old_paths = load_closure_paths(conn, old_dep.id)?;

    let new_refs: Vec<_> = new_paths
        .iter()
        .map(|(p, n)| ClosurePathRef { path: p, nar_size: *n })
        .collect();
    let old_refs: Vec<_> = old_paths
        .iter()
        .map(|(p, n)| ClosurePathRef { path: p, nar_size: *n })
        .collect();

    let diff_root = diff::build_diff_root(&new_refs, &old_refs);

    color::init(false);
    let mut packages = PackageListDiff::new();
    packages.show_size_delta = false;
    packages.from_diff_root(diff_root);

    println!("Deployment {} vs {}", old_dep.id, new_dep.id);
    println!();
    print!("{packages}");
    println!(
        "size: {} -> {} ({})",
        format_size(old_dep.size),
        format_size(new_dep.size),
        packages.size_delta(),
    );
    Ok(())
}

struct DeploymentRow {
    id: i64,
    target_machine_id: i64,
    #[allow(dead_code)]
    toplevel: String,
    size: i64,
    #[allow(dead_code)]
    created_at: String,
}

fn load_deployment(conn: &Connection, id: i64) -> Result<DeploymentRow> {
    conn.query_row(
        "SELECT id, target_machine_id, toplevel, size, created_at
         FROM deployments WHERE id = ?1",
        params![id],
        |r| {
            Ok(DeploymentRow {
                id: r.get(0)?,
                target_machine_id: r.get(1)?,
                toplevel: r.get(2)?,
                size: r.get(3)?,
                created_at: r.get(4)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("unknown deployment: {id}"))
}

fn load_closure_paths(conn: &Connection, deployment_id: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT sp.path, sp.nar_size
         FROM closures c
         JOIN store_paths sp ON sp.id = c.store_path_id
         WHERE c.deployment_id = ?1",
    )?;
    let rows = stmt.query_map(params![deployment_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
    closure: &[StorePathInfo],
) -> Result<i64> {
    let tx = conn.unchecked_transaction()?;

    // Insert all store paths.
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO store_paths
                (path, closure_size, nar_size, deriver, nar_hash, valid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for sp in closure {
            stmt.execute(params![
                sp.path,
                sp.closure_size,
                sp.nar_size,
                sp.deriver,
                sp.nar_hash,
                i64::from(sp.valid)
            ])?;
        }
    }

    // Resolve store path IDs.
    let mut id_for = std::collections::HashMap::<String, i64>::new();
    {
        let mut stmt = tx.prepare("SELECT id FROM store_paths WHERE path = ?1")?;
        for sp in closure {
            let id: i64 = stmt.query_row(params![sp.path], |r| r.get(0))?;
            id_for.insert(sp.path.clone(), id);
        }
    }

    let deployment_id = {
        tx.execute(
            "INSERT INTO deployments (target_machine_id, toplevel, size)
             VALUES (?1, ?2, ?3)",
            params![machine_id, toplevel, size],
        )?;
        tx.last_insert_rowid()
    };

    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO closures (store_path_id, deployment_id) VALUES (?1, ?2)",
        )?;
        for sp in closure {
            stmt.execute(params![id_for[&sp.path], deployment_id])?;
        }
    }

    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO refs (referrer_id, referenced_id) VALUES (?1, ?2)",
        )?;
        for sp in closure {
            let referrer = id_for[&sp.path];
            for r in &sp.references {
                if let Some(&referenced) = id_for.get(r) {
                    stmt.execute(params![referrer, referenced])?;
                }
            }
        }
    }

    tx.commit()?;
    Ok(deployment_id)
}

fn target_label(target: &Target) -> String {
    match target {
        Target::Local => "local system".to_string(),
        Target::Ssh(t) => match t.port {
            Some(p) => format!("ssh://{}:{}", t.host, p),
            None => format!("ssh://{}", t.host),
        },
    }
}

fn format_size(bytes: i64) -> String {
    let s = humansize::format_size(bytes.unsigned_abs(), humansize::BINARY);
    if bytes < 0 {
        format!("-{s}")
    } else {
        s
    }
}

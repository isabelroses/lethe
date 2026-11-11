use std::io::Write;

use anyhow::Result;
use dix::Size;
use rusqlite::Connection;
use tabled::builder::Builder;
use tabled::settings::object::Columns;
use tabled::settings::{Alignment, Modify, Style};
use tracing::info;
use yansi::Paint as _;

use crate::db;
use crate::nix::{self, Target};

pub fn record(
    conn: &Connection,
    out: &mut impl Write,
    name_override: Option<&str>,
    target: &Target,
    system_link: &str,
) -> Result<()> {
    info!("resolving {system_link} on {}", target.label());
    let system = nix::identify(target, name_override, system_link)?;
    info!("recording {}: {}", system.machine, system.toplevel);

    // An early exit before the closure is fetched, which is the slow part.
    // `db::record` enforces the same rule when it stores the deployment.
    db::ensure_new(conn, &system.machine, &system.toplevel)?;

    info!("fetching closure");
    let snapshot = nix::fetch_snapshot(target, &system)?;
    info!("{} store paths, {} selected", snapshot.closure.len(), snapshot.selected.len());

    let deployment = db::record(conn, &system.machine, &system.toplevel, &snapshot)?;
    writeln!(
        out,
        "recorded deployment for {} (toplevel {}, closure {})",
        deployment.machine,
        deployment.toplevel,
        Size::from_bytes(deployment.size)
    )?;
    Ok(())
}

pub fn machines(conn: &Connection, out: &mut impl Write) -> Result<()> {
    let rows = db::machines(conn)?.into_iter().map(|m| {
        [m.identifier, m.deployments.to_string(), m.last_deployed.unwrap_or_else(|| "-".into())]
    });
    writeln!(out, "{}", table(["MACHINE", "DEPLOYS", "LAST"], rows, &[1]))?;
    Ok(())
}

pub fn deployments(conn: &Connection, out: &mut impl Write, machine: &str) -> Result<()> {
    let rows = db::deployments(conn, machine)?.into_iter().map(|d| {
        [d.id.to_string(), d.created_at, Size::from_bytes(d.size).to_string(), d.toplevel]
    });
    writeln!(out, "{}", table(["ID", "CREATED", "SIZE", "TOPLEVEL"], rows, &[0, 2]))?;
    Ok(())
}

pub fn show(conn: &Connection, out: &mut impl Write, id: i64) -> Result<()> {
    let d = db::deployment(conn, id)?;
    writeln!(out, "Deployment {}", d.id)?;
    writeln!(out, "  machine:   {}", d.machine)?;
    writeln!(out, "  created:   {}", d.created_at)?;
    writeln!(out, "  toplevel:  {}", d.toplevel)?;
    writeln!(out, "  size:      {} ({} bytes)", Size::from_bytes(d.size), d.size)?;
    writeln!(out, "  closure:   {} store paths", db::closure_len(conn, id)?)?;
    Ok(())
}

pub fn diff(conn: &Connection, out: &mut impl Write, old: &str, new: Option<&str>) -> Result<()> {
    let (old_dep, new_dep) = db::resolve_diff_pair(conn, old, new)?;
    let report = dix::diff_store_snapshots(
        &db::load_snapshot(conn, old_dep.id)?,
        &db::load_snapshot(conn, new_dep.id)?,
    );

    let mut rendered = String::with_capacity(4096);
    let _hidden = dix::write_diff_report(&mut rendered, &report, dix::RenderOptions::default())?;
    writeln!(out, "{}", format!("<<< {}", old_dep.toplevel).bold().dim())?;
    writeln!(out, "{}", format!(">>> {}", new_dep.toplevel).bold().dim())?;
    write!(out, "{rendered}")?;
    Ok(())
}

fn table<const N: usize>(
    header: [&str; N],
    rows: impl IntoIterator<Item = [String; N]>,
    right_aligned: &[usize],
) -> String {
    let mut builder = Builder::default();
    builder.push_record(header);
    for row in rows {
        builder.push_record(row);
    }
    let mut table = builder.build();
    table.with(Style::empty());
    for &column in right_aligned {
        table.with(Modify::new(Columns::one(column)).with(Alignment::right()));
    }
    table.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testing::{open_in_memory, path, snapshot};

    /// Two deployments of `host-a`: `foo` is upgraded and `bar` is added.
    fn recorded() -> (Connection, String, String) {
        let conn = open_in_memory();
        let (old, new) = (path(0, "system-1"), path(1, "system-2"));
        let (foo1, foo2, bar) = (path(2, "foo-1.0"), path(3, "foo-2.0"), path(4, "bar-1.0"));
        db::record(&conn, "host-a", old.as_str(), &snapshot(&[(&old, 1024), (&foo1, 1024)], &[&foo1]))
            .unwrap();
        db::record(
            &conn,
            "host-a",
            new.as_str(),
            &snapshot(&[(&new, 1024), (&foo2, 2048), (&bar, 1024)], &[&foo2, &bar]),
        )
        .unwrap();
        conn.execute("UPDATE deployments SET created_at = '2026-01-0' || id || ' 12:00:00'", [])
            .unwrap();
        (conn, old.into(), new.into())
    }

    fn rendered(command: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut out = Vec::new();
        command(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn trimmed_lines(text: &str) -> Vec<&str> {
        text.lines().map(str::trim).collect()
    }

    #[test]
    fn machines_lists_deploy_counts_and_last_deployment() {
        let (conn, ..) = recorded();
        conn.execute("INSERT INTO machines (identifier) VALUES ('host-b')", []).unwrap();
        let out = rendered(|out| machines(&conn, out));
        assert_eq!(
            trimmed_lines(&out),
            [
                "MACHINE  DEPLOYS  LAST",
                "host-a         2  2026-01-02 12:00:00",
                "host-b         0  -",
            ]
        );
    }

    #[test]
    fn deployments_lists_newest_first_with_sizes() {
        let (conn, old, new) = recorded();
        let out = rendered(|out| deployments(&conn, out, "host-a"));
        assert_eq!(
            trimmed_lines(&out),
            [
                "ID  CREATED                  SIZE  TOPLEVEL",
                &format!("2  2026-01-02 12:00:00  4.00 KiB  {new}"),
                &format!("1  2026-01-01 12:00:00  2.00 KiB  {old}"),
            ]
        );
        assert!(deployments(&conn, &mut Vec::new(), "host-z").is_err());
    }

    #[test]
    fn show_prints_one_deployment() {
        let (conn, _, new) = recorded();
        let out = rendered(|out| show(&conn, out, 2));
        assert_eq!(
            out,
            format!(
                "Deployment 2\n  machine:   host-a\n  created:   2026-01-02 12:00:00\n  \
                 toplevel:  {new}\n  size:      4.00 KiB (4096 bytes)\n  closure:   3 store paths\n"
            )
        );
        let err = show(&conn, &mut Vec::new(), 9).unwrap_err();
        assert!(err.to_string().contains("unknown deployment: 9"));
    }

    #[test]
    fn diff_names_both_systems_and_the_changed_packages() {
        let (conn, old, new) = recorded();
        let out = rendered(|out| diff(&conn, out, "host-a", None));
        let old_at = out.find(&format!("<<< {old}")).expect("old toplevel");
        let new_at = out.find(&format!(">>> {new}")).expect("new toplevel");
        assert!(old_at < new_at);
        // Versions are coloured digit by digit, so only names are matched.
        for needle in ["CHANGED", "foo", "ADDED", "bar"] {
            assert!(out.contains(needle), "missing {needle} in:\n{out}");
        }
    }
}

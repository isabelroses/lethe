//! Sharing deployments between devices.
//!
//! A deployment travels as a self-contained [`Deployment`] record keyed by
//! its uuid. Local ids and numbers never leave a database: the server's own
//! id doubles as the sequence number clients pull after.

pub mod client;
pub mod server;

use std::collections::HashSet;
use std::io::Read as _;

use anyhow::{Result, anyhow, bail};
use dix::{Size, StorePath, StorePathInfo, StoreSnapshot};
use rusqlite::Connection;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::db;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub uuid: String,
    pub machine: String,
    /// `SQLite`'s `datetime('now')` format, UTC. Carried verbatim.
    pub created_at: String,
    pub toplevel: String,
    pub closure: Vec<ClosurePath>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClosurePath {
    pub path: String,
    pub nar_size: i64,
    pub selected: bool,
}

/// A local deployment with its id, which is the push cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequenced {
    pub seq: i64,
    pub deployment: Deployment,
}

/// `GET /v1/index`: which deployments the server holds past a cursor, without
/// their closures, so a client downloads only those it lacks.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct IndexPage {
    pub entries: Vec<IndexEntry>,
    pub next: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub seq: i64,
    pub uuid: String,
}

/// `POST /v1/fetch`.
#[derive(Serialize, Deserialize, Debug)]
pub struct FetchBody {
    pub uuids: Vec<String>,
}

/// `POST /v1/deployments`, and the reply to `POST /v1/fetch`.
#[derive(Serialize, Deserialize, Debug)]
pub struct PushBody {
    pub deployments: Vec<Deployment>,
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
pub struct PushResult {
    pub inserted: usize,
    pub skipped: usize,
}

/// Bodies are JSON, compressed with zstd when the other side accepts it.
/// Consecutive deployments share nearly every store path, so a page shrinks by
/// one to two orders of magnitude, which is most of the time spent on a network.
pub const ZSTD: &str = "zstd";

/// zstd's default level: much better than gzip, and far faster than the
/// network it saves time on.
const ZSTD_LEVEL: i32 = 3;

/// Largest body either side reads, before or after decompression.
pub const MAX_BODY: usize = 64 * 1024 * 1024;

pub fn encode(value: &impl Serialize, compress: bool) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(value)?;
    if !compress {
        return Ok(json);
    }
    zstd_compress(&json)
}

pub fn zstd_compress(body: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::bulk::compress(body, ZSTD_LEVEL)?)
}

/// Parse a body read off the wire. Decompression stops at [`MAX_BODY`], so a
/// small body cannot inflate without bound.
pub fn decode<T: DeserializeOwned>(body: &[u8], compressed: bool) -> Result<T> {
    if !compressed {
        return Ok(serde_json::from_slice(body)?);
    }
    let mut json = Vec::with_capacity(body.len() * 8);
    zstd::stream::read::Decoder::new(body)?
        .take(u64::try_from(MAX_BODY).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut json)?;
    if json.len() > MAX_BODY {
        bail!("body inflates past {MAX_BODY} bytes");
    }
    Ok(serde_json::from_slice(&json)?)
}

impl From<db::Portable> for Deployment {
    fn from(d: db::Portable) -> Self {
        let selected: HashSet<_> = d.snapshot.selected.iter().collect();
        let closure = d
            .snapshot
            .closure
            .iter()
            .map(|p| ClosurePath {
                path: p.path.as_str().into(),
                nar_size: p.nar_size.bytes(),
                selected: selected.contains(&p.path),
            })
            .collect();
        Deployment {
            uuid: d.uuid,
            machine: d.machine,
            created_at: d.created_at,
            toplevel: d.toplevel,
            closure,
        }
    }
}

impl TryFrom<Deployment> for db::Portable {
    type Error = anyhow::Error;

    /// Checks every path, since the record may come from another device.
    fn try_from(d: Deployment) -> Result<Self> {
        let mut snapshot = StoreSnapshot { closure: Vec::with_capacity(d.closure.len()), selected: Vec::new() };
        for c in d.closure {
            let path = StorePath::try_from(c.path)
                .map_err(|e| anyhow!("deployment {}: invalid store path: {e}", d.uuid))?;
            if c.selected {
                snapshot.selected.push(path.clone());
            }
            snapshot.closure.push(StorePathInfo { path, nar_size: Size::from_bytes(c.nar_size) });
        }
        Ok(db::Portable {
            uuid: d.uuid,
            machine: d.machine,
            created_at: d.created_at,
            toplevel: d.toplevel,
            snapshot,
        })
    }
}

/// Deployments with a local id above `after_id`, oldest first.
pub fn export_after(conn: &Connection, after_id: i64, limit: usize) -> Result<Vec<Sequenced>> {
    Ok(db::portable_after(conn, after_id, limit)?
        .into_iter()
        .map(|(seq, d)| Sequenced { seq, deployment: d.into() })
        .collect())
}

/// The uuids past `after_id`, oldest first. Empty `next` means the last page.
pub fn index_after(conn: &Connection, after_id: i64, limit: usize) -> Result<IndexPage> {
    let entries: Vec<IndexEntry> = db::uuids_after(conn, after_id, limit)?
        .into_iter()
        .map(|(seq, uuid)| IndexEntry { seq, uuid })
        .collect();
    let next = (entries.len() == limit).then(|| entries.last().map(|e| e.seq)).flatten();
    Ok(IndexPage { entries, next })
}

/// The deployments named by `uuids`; unknown ones are left out.
pub fn fetch(conn: &Connection, uuids: &[String]) -> Result<Vec<Deployment>> {
    Ok(db::portables_by_uuid(conn, uuids)?.into_iter().map(Deployment::from).collect())
}

/// Insert every deployment not already known. The caller provides the
/// transaction, so the client can advance its cursor along with the rows.
pub fn import(conn: &Connection, deployments: Vec<Deployment>) -> Result<PushResult> {
    let total = deployments.len();
    let portable = deployments
        .into_iter()
        .map(db::Portable::try_from)
        .collect::<Result<Vec<_>>>()?;
    let inserted = db::import(conn, &portable)?;
    Ok(PushResult { inserted, skipped: total - inserted })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::testing::{open_in_memory, path, snapshot};

    /// Record a deployment the way `lethe record` would, returning its uuid.
    pub(crate) fn record(conn: &Connection, machine: &str, toplevel: &str) -> String {
        let top = StorePath::try_from(toplevel.to_string()).unwrap();
        let dep = path(9, "dependency");
        db::record(conn, machine, toplevel, &snapshot(&[(&top, 100), (&dep, 200)], &[&dep]))
            .unwrap();
        conn.query_row("SELECT uuid FROM deployments ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
            .unwrap()
    }

    pub(crate) fn system(index: usize) -> String {
        path(index, "system").as_str().into()
    }

    fn sorted(mut d: Deployment) -> Deployment {
        d.closure.sort_by(|a, b| a.path.cmp(&b.path));
        d
    }

    fn only(conn: &Connection) -> Deployment {
        let mut all = export_after(conn, 0, 10).unwrap();
        assert_eq!(all.len(), 1);
        all.pop().unwrap().deployment
    }

    #[test]
    fn export_import_round_trips() {
        let src = open_in_memory();
        record(&src, "host-a", &system(0));
        let d = only(&src);
        assert_eq!(d.closure.len(), 2);
        assert_eq!(d.closure.iter().filter(|c| c.selected).count(), 1);

        let dst = open_in_memory();
        assert_eq!(import(&dst, vec![d.clone()]).unwrap(), PushResult { inserted: 1, skipped: 0 });
        assert_eq!(import(&dst, vec![d.clone()]).unwrap(), PushResult { inserted: 0, skipped: 1 });
        assert_eq!(sorted(only(&dst)), sorted(d));
    }

    #[test]
    fn import_rejects_paths_outside_the_store() {
        let src = open_in_memory();
        record(&src, "host-a", &system(0));
        let mut d = only(&src);
        d.closure[0].path = "/etc/passwd".into();

        let err = import(&open_in_memory(), vec![d]).unwrap_err();
        assert!(format!("{err:#}").contains("invalid store path"), "{err:#}");
    }

    #[test]
    fn json_shape_is_stable() {
        let conn = open_in_memory();
        record(&conn, "host-a", &system(0));
        let value = serde_json::to_value(only(&conn)).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["closure", "created_at", "machine", "toplevel", "uuid"]);
    }

    #[test]
    fn index_pages_and_fetch_skips_unknown_uuids() {
        let conn = open_in_memory();
        let uuids: Vec<String> = (0..3).map(|i| record(&conn, "host-a", &system(i))).collect();

        let first = index_after(&conn, 0, 2).unwrap();
        assert_eq!(first.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(first.next, Some(2));
        let rest = index_after(&conn, 2, 2).unwrap();
        assert_eq!(rest.entries[0].uuid, uuids[2]);
        assert_eq!(rest.next, None);

        let fetched = fetch(&conn, &[uuids[1].clone(), "nope".into()]).unwrap();
        assert_eq!(fetched.iter().map(|d| &d.uuid).collect::<Vec<_>>(), [&uuids[1]]);
    }

    #[test]
    fn encode_round_trips_either_way_and_bounds_inflation() {
        let conn = open_in_memory();
        record(&conn, "host-a", &system(0));
        let body = PushBody { deployments: vec![only(&conn)] };
        for compress in [false, true] {
            let back: PushBody = decode(&encode(&body, compress).unwrap(), compress).unwrap();
            assert_eq!(back.deployments, body.deployments);
        }

        let bomb = zstd::bulk::compress(&vec![b' '; MAX_BODY + 1], 1).unwrap();
        let err = decode::<PushBody>(&bomb, true).unwrap_err();
        assert!(err.to_string().contains("inflates"), "{err:#}");
    }
}

//! `lethe sync`: push what this device recorded, pull what others did.

use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::server::MAX_FETCH;
use super::{FetchBody, IndexPage, MAX_BODY, PushBody, PushResult, ZSTD, decode, encode, export_after};
use crate::db;

/// Whole deployments per request in both directions. A NixOS closure is a few
/// hundred KB of JSON, so this keeps each body well under [`MAX_BODY`] before
/// compression.
const BATCH: usize = 20;
const _: () = assert!(BATCH <= MAX_FETCH);

/// Index entries per request. Each is a uuid and a number.
const INDEX_PAGE: usize = 1000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
}

pub fn sync(conn: &Connection, server: &str, token: &str) -> Result<SyncReport> {
    let remote = server.trim_end_matches('/');
    let client = Client {
        agent: ureq::Agent::config_builder().http_status_as_error(false).build().into(),
        remote,
        auth: format!("Bearer {}", token.trim()),
    };
    let mut cursors = db::cursors(conn, remote)?;
    let mut report = SyncReport::default();

    // Push first, so the pull below can tell rows it fetched apart from rows
    // this device still owes the server.
    loop {
        let batch = export_after(conn, cursors.pushed, BATCH)?;
        let Some(high) = batch.last().map(|s| s.seq) else { break };
        let body = PushBody { deployments: batch.into_iter().map(|s| s.deployment).collect() };
        let result: PushResult = client.post("/v1/deployments", &body)?;
        report.pushed += result.inserted;
        cursors.pushed = high;
        db::save_cursors(conn, remote, cursors)?;
    }

    // Pull by index, downloading only deployments this device lacks: what it
    // just pushed comes back in the index, but never over the wire again.
    loop {
        let page: IndexPage =
            client.get(&format!("/v1/index?after={}&limit={INDEX_PAGE}", cursors.pulled))?;
        let Some(high) = page.entries.last().map(|e| e.seq) else { break };
        let uuids: Vec<String> = page.entries.into_iter().map(|e| e.uuid).collect();
        let missing = db::unknown_uuids(conn, &uuids)?;

        // The cursor moves to the end of the page only with its last batch,
        // so an interrupted sync fetches the rest next time.
        let mut batches = missing.chunks(BATCH).peekable();
        if batches.peek().is_none() {
            db::store_pulled(conn, remote, &[], high, &mut cursors)?;
        }
        while let Some(batch) = batches.next() {
            let fetched: PushBody = client.post("/v1/fetch", &FetchBody { uuids: batch.to_vec() })?;
            let deployments = fetched
                .deployments
                .into_iter()
                .map(db::Portable::try_from)
                .collect::<Result<Vec<_>>>()?;
            let seq = if batches.peek().is_none() { high } else { cursors.pulled };
            report.pulled += db::store_pulled(conn, remote, &deployments, seq, &mut cursors)?;
        }

        if page.next.is_none() {
            break;
        }
    }

    Ok(report)
}

struct Client<'a> {
    agent: ureq::Agent,
    remote: &'a str,
    auth: String,
}

impl Client<'_> {
    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .agent
            .get(format!("{}{path}", self.remote))
            .header("Authorization", &self.auth)
            .header("Accept-Encoding", ZSTD)
            .call()
            .with_context(|| format!("contacting sync server {}", self.remote))?;
        Self::decode(response)
    }

    fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let response = self
            .agent
            .post(format!("{}{path}", self.remote))
            .header("Authorization", &self.auth)
            .header("Accept-Encoding", ZSTD)
            .header("Content-Encoding", ZSTD)
            .content_type("application/json")
            .send(encode(body, true)?)
            .with_context(|| format!("contacting sync server {}", self.remote))?;
        Self::decode(response)
    }

    fn decode<T: DeserializeOwned>(mut response: ureq::http::Response<ureq::Body>) -> Result<T> {
        let status = response.status().as_u16();
        if status == 401 {
            anyhow::bail!("sync server rejected the token (401)");
        }
        let compressed = response
            .headers()
            .get("Content-Encoding")
            .is_some_and(|v| v.as_bytes() == ZSTD.as_bytes());
        // Read whole, then parse: serde over a reader goes byte by byte and
        // was most of the time spent on a page.
        let body = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(MAX_BODY).unwrap_or(u64::MAX))
            .read_to_vec()
            .context("reading sync server response")?;
        if !(200..300).contains(&status) {
            let message = decode::<serde_json::Value>(&body, compressed)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).trim().to_owned());
            return Err(anyhow!("sync server returned {status}: {message}"));
        }
        decode(&body, compressed).context("decoding sync server response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testing::open_in_memory as fresh;
    use crate::sync::server::tests::{TOKEN, spawn};
    use crate::sync::tests::{record, system};

    fn uuids(conn: &Connection) -> Vec<(String, String)> {
        conn.prepare("SELECT uuid, created_at FROM deployments ORDER BY uuid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn sync_surfaces_401() {
        let url = spawn(fresh());
        let err = sync(&fresh(), &url, "wrong").unwrap_err();
        assert!(format!("{err:#}").contains("rejected the token"), "{err:#}");
    }

    #[test]
    fn push_only_then_pull_only() {
        let url = spawn(fresh());
        let a = fresh();
        record(&a, "host-a", &system(0));
        assert_eq!(sync(&a, &url, TOKEN).unwrap(), SyncReport { pushed: 1, pulled: 0 });

        let b = fresh();
        assert_eq!(sync(&b, &url, TOKEN).unwrap(), SyncReport { pushed: 0, pulled: 1 });
        assert_eq!(uuids(&a), uuids(&b));
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM deployments", [], |r| r.get(0)).unwrap()
    }

    /// Three databases: a server and two orchestrators that have each recorded
    /// one deployment and synced until they agree.
    fn converged() -> (String, Connection, Connection) {
        let url = spawn(fresh());
        let a = fresh();
        let b = fresh();
        record(&a, "host-a", &system(0));
        record(&b, "host-b", &system(1));
        sync(&a, &url, TOKEN).unwrap();
        sync(&b, &url, TOKEN).unwrap();
        sync(&a, &url, TOKEN).unwrap();
        (url, a, b)
    }

    #[test]
    fn two_clients_converge() {
        let (_url, a, b) = converged();
        assert_eq!(count(&a), 2);
        assert_eq!(uuids(&a), uuids(&b));
    }

    #[test]
    fn second_sync_is_a_noop() {
        let (url, a, b) = converged();
        assert_eq!(sync(&a, &url, TOKEN).unwrap(), SyncReport::default());
        assert_eq!(sync(&b, &url, TOKEN).unwrap(), SyncReport::default());
    }

    #[test]
    fn both_devices_number_a_shared_machine_alike() {
        let url = spawn(fresh());
        let a = fresh();
        let b = fresh();
        // Both orchestrators deploy host-x, interleaved in time.
        record(&a, "host-x", &system(0));
        record(&b, "host-x", &system(1));
        record(&a, "host-x", &system(2));
        for conn in [&a, &b, &a] {
            sync(conn, &url, TOKEN).unwrap();
        }

        let numbered = |conn: &Connection| -> Vec<(i64, String)> {
            db::deployments(conn, "host-x")
                .unwrap()
                .into_iter()
                .map(|d| (d.number, d.toplevel))
                .collect()
        };
        assert_eq!(numbered(&a).len(), 3);
        assert_eq!(numbered(&a), numbered(&b));
    }

    #[test]
    fn pulled_rows_are_not_pushed_back() {
        let url = spawn(fresh());
        let a = fresh();
        record(&a, "host-a", &system(0));
        sync(&a, &url, TOKEN).unwrap();

        let b = fresh();
        assert_eq!(sync(&b, &url, TOKEN).unwrap().pulled, 1);
        assert_eq!(sync(&b, &url, TOKEN).unwrap().pushed, 0);
        assert_eq!(db::cursors(&b, &url).unwrap().pushed, 1);
    }

    #[test]
    fn resync_after_cursor_reset_dedupes() {
        let (url, a, _b) = converged();
        a.execute("DELETE FROM sync_state", []).unwrap();
        assert_eq!(sync(&a, &url, TOKEN).unwrap(), SyncReport::default());
        assert_eq!(count(&a), 2);
    }

    #[test]
    fn trailing_slash_shares_cursor() {
        let url = spawn(fresh());
        let a = fresh();
        record(&a, "host-a", &system(0));
        sync(&a, &url, TOKEN).unwrap();
        assert_eq!(sync(&a, &format!("{url}/"), TOKEN).unwrap(), SyncReport::default());
    }

    /// Timings for a realistic history. Run with
    /// `cargo test --release bench_sync -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn bench_sync() {
        use dix::{Size, StorePath, StorePathInfo, StoreSnapshot};
        use std::time::Instant;

        const DEPLOYMENTS: usize = 100;
        const PATHS: usize = 4000;
        const CHURN: usize = 120;

        let src = fresh();
        let mut paths: Vec<String> = (0..PATHS)
            .map(|i| format!("/nix/store/{i:032}-package-number-{i}-1.2.3"))
            .collect();
        let t = Instant::now();
        for d in 0..DEPLOYMENTS {
            for c in 0..CHURN {
                let slot = (d * 7919 + c * 104_729) % PATHS;
                paths[slot] = format!("/nix/store/{:032}-package-{slot}-v{d}", PATHS + d * CHURN + c);
            }
            let top = StorePath::try_from(format!("/nix/store/{:032}-system-{d}", 9_000_000 + d))
                .unwrap();
            let mut closure: Vec<StorePathInfo> = paths
                .iter()
                .map(|p| StorePathInfo {
                    path: StorePath::try_from(p.clone()).unwrap(),
                    nar_size: Size::from_bytes(123_456),
                })
                .collect();
            closure.push(StorePathInfo { path: top.clone(), nar_size: Size::from_bytes(1) });
            let selected = closure[..50].iter().map(|p| p.path.clone()).collect();
            let machine = ["host-a", "host-b", "host-c"][d % 3];
            db::record(&src, machine, top.as_str(), &StoreSnapshot { closure, selected }).unwrap();
        }
        eprintln!("seed     {:>8.1?}", t.elapsed());

        let page = export_after(&src, 0, BATCH).unwrap();
        let json = serde_json::to_vec(&PushBody {
            deployments: page.into_iter().map(|s| s.deployment).collect(),
        })
        .unwrap();
        eprintln!("page     {:>8} bytes for {BATCH} deployments", json.len());
        let page: Vec<_> = export_after(&src, 0, BATCH).unwrap().into_iter().map(|s| s.deployment).collect();
        let t = Instant::now();
        let bytes = serde_json::to_vec(&PushBody { deployments: page.clone() }).unwrap();
        eprintln!("encode   {:>8.1?} per page", t.elapsed());
        let t = Instant::now();
        let back: PushBody = serde_json::from_slice(&bytes).unwrap();
        eprintln!("decode   {:>8.1?} per page", t.elapsed());
        let t = Instant::now();
        let portable = back.deployments.into_iter().map(|d| db::Portable::try_from(d).unwrap()).collect::<Vec<_>>();
        eprintln!("validate {:>8.1?} per page", t.elapsed());
        let scratch = fresh();
        let t = Instant::now();
        let tx = scratch.unchecked_transaction().unwrap();
        db::import(&tx, &portable).unwrap();
        tx.commit().unwrap();
        eprintln!("import   {:>8.1?} per page (empty db)", t.elapsed());

        let t = Instant::now();
        for _ in 0..5 {
            export_after(&src, 0, BATCH).unwrap();
        }
        eprintln!("export   {:>8.1?} per page", t.elapsed() / 5);

        let url = spawn(fresh());
        let t = Instant::now();
        sync(&src, &url, TOKEN).unwrap();
        eprintln!("push     {:>8.1?}", t.elapsed());

        let dst = fresh();
        let t = Instant::now();
        sync(&dst, &url, TOKEN).unwrap();
        eprintln!("pull     {:>8.1?}", t.elapsed());

        let t = Instant::now();
        sync(&dst, &url, TOKEN).unwrap();
        eprintln!("noop     {:>8.1?}", t.elapsed());
    }
}

//! `lethe serve`: a small blocking HTTP server holding the shared copy.
//!
//! One request at a time is plenty: clients sync a few times a day, and
//! `SQLite` serialises writes anyway.

use std::io::Read as _;
use std::net::SocketAddr;

use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;
use serde::de::DeserializeOwned;
use tiny_http::{Header, Method, Request, Response};
use tracing::{info, warn};

use super::{
    Deployment, FetchBody, MAX_BODY, PushBody, PushResult, ZSTD, decode, encode, fetch,
    import, index_after, zstd_compress,
};

/// Index entries are a uuid and a number, so a page can be long.
const DEFAULT_INDEX_PAGE: usize = 1000;
const MAX_INDEX_PAGE: usize = 5000;
/// Whole deployments per fetch: a few hundred KB of JSON each.
pub const MAX_FETCH: usize = 100;
/// Replies smaller than this go out as they are; compressing them saves
/// nothing worth the time.
const COMPRESS_OVER: usize = 1024;

pub struct Server {
    http: tiny_http::Server,
    conn: Connection,
    token: String,
}

pub fn bind(addr: &str, conn: Connection, token: &str) -> Result<Server> {
    let token = token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("refusing to serve without a sync token");
    }
    let http = tiny_http::Server::http(addr).map_err(|e| anyhow!("binding {addr}: {e}"))?;
    Ok(Server { http, conn, token })
}

/// A response before it is written: status plus an uncompressed body.
struct Reply {
    status: u16,
    body: Vec<u8>,
    json: bool,
}

impl Reply {
    fn json(status: u16, value: &impl serde::Serialize) -> Self {
        match encode(value, false) {
            Ok(body) => Reply { status, body, json: true },
            Err(e) => Reply::error(500, &format!("encoding response: {e}")),
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Reply {
            status,
            body: serde_json::json!({ "error": message }).to_string().into_bytes(),
            json: true,
        }
    }
}

fn header<'r>(request: &'r Request, name: &'static str) -> Option<&'r str> {
    request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

fn static_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("static header is valid")
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.http
            .server_addr()
            .to_ip()
            .expect("bound with Server::http, so this is a TCP listener")
    }

    pub fn run(self) -> Result<()> {
        info!("listening on http://{}", self.local_addr());
        loop {
            if let Err(e) = self.handle_one() {
                warn!("{e:#}");
            }
        }
    }

    fn handle_one(&self) -> Result<()> {
        let mut request = self.http.recv().context("receiving request")?;
        let reply = self.route(&mut request);
        info!("{} {} -> {}", request.method(), request.url(), reply.status);

        let accepts_zstd = header(&request, "Accept-Encoding")
            .is_some_and(|v| v.split(',').any(|e| e.trim() == ZSTD));
        let compress = reply.json && accepts_zstd && reply.body.len() > COMPRESS_OVER;
        let body = if compress { zstd_compress(&reply.body)? } else { reply.body };

        let mut response = Response::from_data(body).with_status_code(reply.status);
        if reply.json {
            response.add_header(static_header("Content-Type", "application/json"));
        }
        if compress {
            response.add_header(static_header("Content-Encoding", ZSTD));
        }
        request.respond(response).context("writing response")
    }

    fn route(&self, request: &mut Request) -> Reply {
        let (path, query) = request.url().split_once('?').unwrap_or((request.url(), ""));
        let path = path.to_string();
        let query = query.to_string();

        if path == "/v1/health" {
            return Reply { status: 200, body: b"ok".to_vec(), json: false };
        }
        if !self.authorized(request) {
            return Reply::error(401, "unauthorized");
        }

        let result = match (request.method(), path.as_str()) {
            (Method::Get, "/v1/index") => self.index(&query),
            (Method::Post, "/v1/fetch") => self.fetch(request),
            (Method::Post, "/v1/deployments") => self.push(request),
            _ => return Reply::error(404, "not found"),
        };
        result.unwrap_or_else(|reply| reply)
    }

    fn authorized(&self, request: &Request) -> bool {
        header(request, "Authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|given| tokens_match(given.trim().as_bytes(), self.token.as_bytes()))
    }

    fn index(&self, query: &str) -> Result<Reply, Reply> {
        let mut after = 0i64;
        let mut limit = DEFAULT_INDEX_PAGE;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let parsed = match key {
                "after" => value.parse().map(|v| after = v).is_ok(),
                "limit" => {
                    value.parse().map(|v: usize| limit = v.clamp(1, MAX_INDEX_PAGE)).is_ok()
                }
                _ => true,
            };
            if !parsed {
                return Err(Reply::error(400, &format!("invalid {key}: {value}")));
            }
        }
        let page = index_after(&self.conn, after, limit).map_err(|e| database_error(&e))?;
        Ok(Reply::json(200, &page))
    }

    fn fetch(&self, request: &mut Request) -> Result<Reply, Reply> {
        let body: FetchBody = read_body(request)?;
        if body.uuids.len() > MAX_FETCH {
            return Err(Reply::error(400, &format!("at most {MAX_FETCH} uuids per fetch")));
        }
        let deployments = fetch(&self.conn, &body.uuids).map_err(|e| database_error(&e))?;
        Ok(Reply::json(200, &PushBody { deployments }))
    }

    fn push(&self, request: &mut Request) -> Result<Reply, Reply> {
        let body: PushBody = read_body(request)?;
        let result = self.store(body.deployments).map_err(|e| database_error(&e))?;
        Ok(Reply::json(200, &result))
    }

    /// One transaction per push, so a bad record stores none of its batch.
    fn store(&self, deployments: Vec<Deployment>) -> Result<PushResult> {
        let tx = self.conn.unchecked_transaction()?;
        let result = import(&tx, deployments)?;
        tx.commit()?;
        Ok(result)
    }
}

/// Read and parse a request body, compressed or not, within [`MAX_BODY`].
fn read_body<T: DeserializeOwned>(request: &mut Request) -> Result<T, Reply> {
    if request.body_length().is_some_and(|n| n > MAX_BODY) {
        return Err(Reply::error(413, "body too large"));
    }
    let compressed = header(request, "Content-Encoding") == Some(ZSTD);
    let mut body = Vec::with_capacity(request.body_length().unwrap_or(0));
    let limit = u64::try_from(MAX_BODY).unwrap_or(u64::MAX) + 1;
    if let Err(e) = request.as_reader().take(limit).read_to_end(&mut body) {
        return Err(Reply::error(400, &format!("reading body: {e}")));
    }
    if body.len() > MAX_BODY {
        return Err(Reply::error(413, "body too large"));
    }
    decode(&body, compressed).map_err(|e| Reply::error(400, &format!("invalid body: {e:#}")))
}

fn database_error(e: &anyhow::Error) -> Reply {
    warn!("{e:#}");
    Reply::error(500, "database error")
}

/// Compare without exiting early, so response timing doesn't reveal how
/// much of a guessed token was right.
fn tokens_match(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::testing::open_in_memory as fresh;
    use crate::sync::tests::{record, system};
    use crate::sync::{IndexPage, export_after};

    pub(crate) const TOKEN: &str = "s3cret";

    /// Start a server on an ephemeral port and return its base URL.
    pub(crate) fn spawn(conn: Connection) -> String {
        let server = bind("127.0.0.1:0", conn, TOKEN).unwrap();
        let url = format!("http://{}", server.local_addr());
        std::thread::spawn(move || server.run());
        url
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder().http_status_as_error(false).build().into()
    }

    fn sample() -> Deployment {
        let src = fresh();
        record(&src, "host-a", &system(0));
        export_after(&src, 0, 1).unwrap().pop().unwrap().deployment
    }

    fn post<T: DeserializeOwned>(url: &str, path: &str, body: &impl serde::Serialize) -> T {
        agent()
            .post(format!("{url}{path}"))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .send_json(body)
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    }

    fn index(url: &str, query: &str) -> IndexPage {
        agent()
            .get(format!("{url}/v1/index?{query}"))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    }

    #[test]
    fn health_needs_no_auth() {
        let url = spawn(fresh());
        let mut resp = agent().get(format!("{url}/v1/health")).call().unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.body_mut().read_to_string().unwrap(), "ok");
    }

    #[test]
    fn rejects_bad_token() {
        let url = spawn(fresh());
        let resp = agent()
            .post(format!("{url}/v1/deployments"))
            .header("Authorization", "Bearer nope")
            .send_json(PushBody { deployments: vec![sample()] })
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401);
        let resp = agent().get(format!("{url}/v1/index")).call().unwrap();
        assert_eq!(resp.status().as_u16(), 401);

        assert_eq!(index(&url, "after=0").entries, []);
    }

    #[test]
    fn push_index_fetch() {
        let url = spawn(fresh());
        let d = sample();
        let pushed: PushResult = post(&url, "/v1/deployments", &PushBody { deployments: vec![d.clone()] });
        assert_eq!(pushed, PushResult { inserted: 1, skipped: 0 });

        let page = index(&url, "after=0");
        assert_eq!(page.entries.len(), 1);
        assert_eq!((page.entries[0].seq, page.entries[0].uuid.as_str()), (1, d.uuid.as_str()));
        assert_eq!(page.next, None);

        let fetched: PushBody = post(&url, "/v1/fetch", &FetchBody { uuids: vec![d.uuid.clone()] });
        assert_eq!(fetched.deployments, [d]);
    }

    #[test]
    fn index_paginates() {
        let url = spawn(fresh());
        let _: PushResult =
            post(&url, "/v1/deployments", &PushBody { deployments: vec![sample(), sample(), sample()] });

        let first = index(&url, "after=0&limit=2");
        assert_eq!(first.entries.len(), 2);
        assert_eq!(first.next, Some(2));
        let rest = index(&url, "after=2&limit=2");
        assert_eq!(rest.entries.len(), 1);
        assert_eq!(rest.next, None);
    }

    #[test]
    fn compresses_both_ways_when_asked() {
        let url = spawn(fresh());
        let mut d = sample();
        // Past `COMPRESS_OVER`, so the reply is worth compressing.
        d.closure.extend((0..50).map(|i| crate::sync::ClosurePath {
            path: format!("/nix/store/{i:032}-package"),
            nar_size: 1,
            selected: false,
        }));
        let mut resp = agent()
            .post(format!("{url}/v1/deployments"))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Encoding", ZSTD)
            .send(encode(&PushBody { deployments: vec![d.clone()] }, true).unwrap())
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{}", resp.body_mut().read_to_string().unwrap());

        let mut resp = agent()
            .post(format!("{url}/v1/fetch"))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Accept-Encoding", "gzip, zstd")
            .send_json(FetchBody { uuids: vec![d.uuid.clone()] })
            .unwrap();
        assert_eq!(resp.headers().get("Content-Encoding").unwrap(), ZSTD);
        let body = resp.body_mut().read_to_vec().unwrap();
        let fetched: PushBody = decode(&body, true).unwrap();
        assert_eq!(fetched.deployments, [d]);
    }

    #[test]
    fn unknown_route_bad_json_and_oversized_fetch() {
        let url = spawn(fresh());
        let authed = |req: ureq::RequestBuilder<ureq::typestate::WithBody>| {
            req.header("Authorization", format!("Bearer {TOKEN}"))
        };
        let resp = agent()
            .get(format!("{url}/v2/nope"))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .call()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let resp = authed(agent().post(format!("{url}/v1/deployments"))).send("not json").unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        let uuids = vec![String::new(); MAX_FETCH + 1];
        let resp = authed(agent().post(format!("{url}/v1/fetch"))).send_json(FetchBody { uuids }).unwrap();
        assert_eq!(resp.status().as_u16(), 400);
    }

    #[test]
    fn bind_refuses_empty_token() {
        assert!(bind("127.0.0.1:0", fresh(), "  ").is_err());
    }

    #[test]
    fn tokens_match_cases() {
        assert!(tokens_match(b"abc", b"abc"));
        assert!(!tokens_match(b"abc", b"abd"));
        assert!(!tokens_match(b"abc", b"abcd"));
    }
}

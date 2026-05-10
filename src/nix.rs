use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

pub enum Target {
    Local,
    Ssh(SshTarget),
}

pub struct SshTarget {
    pub host: String,
    pub port: Option<u16>,
}

/// Parse a target string of the form `host`, `user@host`, or
/// `ssh://[user@]host[:port][/...]` into an `SshTarget`.
/// <https://serverfault.com/questions/974307/how-can-i-create-an-ssh-protocol-link-from-my-browser-which-will-use-a-jump-host>
pub fn parse_ssh_target(raw: &str) -> SshTarget {
    let Some(stripped) = raw.strip_prefix("ssh://") else {
        return SshTarget { host: raw.to_string(), port: None };
    };
    let stripped = stripped.split('/').next().unwrap_or(stripped);
    if let Some((host, port)) = stripped.rsplit_once(':')
        && let Ok(p) = port.parse::<u16>() {
            return SshTarget { host: host.to_string(), port: Some(p) };
        }
    SshTarget { host: stripped.to_string(), port: None }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorePathInfo {
    #[serde(default)]
    pub path: String,
    pub closure_size: i64,
    pub nar_size: i64,
    pub nar_hash: String,
    // Legacy schema (Nix < 2.19 or lix) emits `valid` explicitly; the modern object
    // schema only lists valid paths (invalid ones map to `null`), so default true.
    #[serde(default = "default_true")]
    pub valid: bool,
    #[serde(default)]
    pub deriver: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
}

fn default_true() -> bool {
    true
}

// `nix path-info --json` switched shape in Nix 2.19: pre-2.19 (or lix which forked from 2.18)
// returns an array with `path` as a field; 2.19+ returns an object keyed by store path with `null`
// values for invalid paths. Accept both via untagged deserialization rather than probing `nix
// --version`.
#[derive(Deserialize)]
#[serde(untagged)]
enum PathInfoJson {
    Legacy(Vec<StorePathInfo>),
    Modern(HashMap<String, Option<StorePathInfo>>),
}

/// Resolve a system link (e.g. /run/current-system) to its store path.
pub fn resolve_toplevel(target: &Target, link: &str) -> Result<String> {
    let out = run(target, &["nix", "path-info", link])
        .with_context(|| format!("resolving {link}"))?;
    let path = out.trim();
    if path.is_empty() {
        anyhow::bail!("nix path-info {link} returned empty output");
    }
    Ok(path.to_string())
}

/// Read the short hostname of the target system.
pub fn fetch_hostname(target: &Target) -> Result<String> {
    let out = run(target, &["hostname", "-s"]).context("fetching hostname")?;
    let h = out.trim();
    if h.is_empty() {
        anyhow::bail!("hostname returned empty");
    }
    Ok(h.to_string())
}

/// Fetch the recursive closure with sizes for a system link.
pub fn fetch_closure(target: &Target, link: &str) -> Result<Vec<StorePathInfo>> {
    let out = run(
        target,
        &["nix", "path-info", "--closure-size", "-rsh", link, "--json"],
    )
    .context("running nix path-info --json")?;
    let parsed: PathInfoJson =
        serde_json::from_str(&out).context("parsing nix path-info JSON")?;
    Ok(match parsed {
        PathInfoJson::Legacy(v) => v,
        PathInfoJson::Modern(m) => m
            .into_iter()
            .filter_map(|(path, info)| {
                let mut info = info?;
                info.path = path;
                Some(info)
            })
            .collect(),
    })
}

fn run(target: &Target, argv: &[&str]) -> Result<String> {
    let mut cmd = match target {
        Target::Local => {
            let mut c = Command::new(argv[0]);
            c.args(&argv[1..]);
            c
        }
        Target::Ssh(t) => {
            let mut c = Command::new("ssh");
            if let Some(p) = t.port {
                c.arg("-p").arg(p.to_string());
            }
            c.arg(&t.host).args(argv);
            c
        }
    };
    let out = cmd
        .output()
        .with_context(|| format!("spawning {argv:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("non-UTF8 output")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<StorePathInfo> {
        let parsed: PathInfoJson = serde_json::from_str(json).unwrap();
        match parsed {
            PathInfoJson::Legacy(v) => v,
            PathInfoJson::Modern(m) => m
                .into_iter()
                .filter_map(|(path, info)| {
                    let mut info = info?;
                    info.path = path;
                    Some(info)
                })
                .collect(),
        }
    }

    #[test]
    fn parses_legacy_array_schema() {
        let json = r#"[{
            "path": "/nix/store/aaa-foo",
            "closureSize": 200,
            "narSize": 100,
            "narHash": "sha256:abc",
            "valid": true,
            "references": ["/nix/store/bbb-bar"]
        }]"#;
        let v = parse(json);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "/nix/store/aaa-foo");
        assert_eq!(v[0].closure_size, 200);
        assert_eq!(v[0].references, vec!["/nix/store/bbb-bar"]);
    }

    #[test]
    fn parses_modern_object_schema() {
        // Mirrors real Nix 2.19+ output: no `valid` field, extra fields like
        // `ca`/`registrationTime`/`ultimate`, and `null` for invalid paths.
        let json = r#"{
            "/nix/store/aaa-foo": {
                "ca": null,
                "closureSize": 200,
                "deriver": "/nix/store/ddd-foo.drv",
                "narHash": "sha256:abc",
                "narSize": 100,
                "references": ["/nix/store/bbb-bar"],
                "registrationTime": 1776867243,
                "signatures": [],
                "ultimate": true
            },
            "/nix/store/zzz-invalid": null
        }"#;
        let v = parse(json);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "/nix/store/aaa-foo");
        assert_eq!(v[0].closure_size, 200);
        assert!(v[0].valid);
        assert_eq!(v[0].references, vec!["/nix/store/bbb-bar"]);
    }
}

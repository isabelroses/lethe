use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use dix::{Store, StorePath, StoreSnapshot};
use tracing::debug;

pub enum Target {
    Local,
    Ssh(SshTarget),
}

pub struct SshTarget {
    pub host: String,
    pub port: Option<u16>,
}

impl SshTarget {
    /// The Nix `ssh://` store URL for this target.
    pub fn store_url(&self) -> String {
        match self.port {
            Some(p) => format!("ssh://{}:{p}", self.host),
            None => format!("ssh://{}", self.host),
        }
    }
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

/// Resolve a system link (e.g. /run/current-system) to its store path.
pub fn resolve_toplevel(target: &Target, link: &str) -> Result<String> {
    let resolved = match target {
        Target::Local => {
            std::fs::canonicalize(link).with_context(|| format!("resolving {link}"))?
        }
        Target::Ssh(_) => {
            let out = run(target, &["readlink", "-f", link])
                .with_context(|| format!("resolving {link}"))?;
            let path = out.trim();
            if path.is_empty() {
                anyhow::bail!("readlink -f {link} returned empty output");
            }
            PathBuf::from(path)
        }
    };

    let resolved = resolved
        .to_str()
        .ok_or_else(|| anyhow!("path {link} contains invalid unicode"))?
        .to_string();
    let store_path = StorePath::try_from(resolved.clone())
        .map_err(|e| anyhow!("{link} did not resolve to a nix store path: {e}"))?;
    Ok(store_path.as_str().to_string())
}

const PROC_HOSTNAME: &str = "/proc/sys/kernel/hostname";

/// Read the short hostname of the target system.
pub fn fetch_hostname(target: &Target) -> Result<String> {
    let raw = match target {
        // Avoids depending on `hostname(1)`, which isn't always installed.
        Target::Local => match std::fs::read_to_string(PROC_HOSTNAME) {
            Ok(raw) => raw,
            Err(e) => {
                debug!("reading {PROC_HOSTNAME} failed ({e}), falling back to hostname(1)");
                run(target, &["hostname", "-s"]).context("fetching hostname")?
            }
        },
        Target::Ssh(_) => run(target, &["hostname", "-s"]).context("fetching hostname")?,
    };

    short_hostname(&raw)
        .map(str::to_string)
        .context("hostname was empty")
}

/// Reduce a hostname to its short form the way `hostname -s` does, so a local
/// `/proc` read and a remote `hostname -s` agree on the stored identifier.
fn short_hostname(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let short = trimmed.split('.').next().unwrap_or(trimmed);
    (!short.is_empty()).then_some(short)
}

/// Fetch the closure and the set of explicitly selected packages for a
/// resolved toplevel store path.
pub fn fetch_snapshot(target: &Target, toplevel: &str) -> Result<StoreSnapshot> {
    let path = Path::new(toplevel);
    match target {
        Target::Local => {
            let store = Store::open();
            let doc = store
                .query_snapshot_document(path)
                .map_err(|e| anyhow!("querying local nix store: {e}"))?;
            Ok(doc.snapshot)
        }
        Target::Ssh(t) => {
            // For remote stores we still need to use nix commands over SSH.
            // query_snapshot_document only works on local paths, so we invoke
            // nix path-info via the SSH target and parse the output into
            // a snapshot ourselves.
            fetch_remote_snapshot(t, toplevel)
        }
    }
}

/// Build a `StoreSnapshot` for a remote machine by running
/// `nix path-info --json --recursive --store <url>` locally (Nix talks to
/// the remote store for us).
fn fetch_remote_snapshot(t: &SshTarget, toplevel: &str) -> Result<StoreSnapshot> {
    let store_url = t.store_url();
    let mut cmd = Command::new("nix");
    cmd.args([
        "path-info",
        "--recursive",
        "--size",
        "--store",
        &store_url,
        toplevel,
    ]);
    let out = cmd.output().context("spawning nix path-info")?;
    if !out.status.success() {
        anyhow::bail!(
            "nix path-info failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let mut closure = Vec::new();
    let mut selected = Vec::new();
    for line in String::from_utf8(out.stdout)
        .context("non-UTF8 nix path-info output")?
        .lines()
    {
        let mut columns = line.split_whitespace();
        let path_str = columns
            .next()
            .ok_or_else(|| anyhow!("missing path in nix path-info output line"))?;
        let nar_size = columns
            .next()
            .ok_or_else(|| anyhow!("missing NAR size in nix path-info output line"))?
            .parse::<i64>()
            .context("failed to parse NAR size from nix path-info output")?;
        let store_path = StorePath::try_from(path_str.to_string())
            .map_err(|e| anyhow!("invalid store path {path_str}: {e}"))?;
        if path_str == toplevel {
            selected.push(store_path.clone());
        }
        closure.push(dix::StorePathInfo {
            path: store_path,
            nar_size: dix::Size::from_bytes(nar_size),
        });
    }

    if closure.is_empty() {
        anyhow::bail!("nix path-info returned no store paths");
    }

    Ok(StoreSnapshot { closure, selected })
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
    let out = cmd.output().with_context(|| format!("spawning {argv:?}"))?;
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

    #[test]
    fn parses_ssh_target_host_only() {
        let t = parse_ssh_target("example.com");
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, None);
    }

    #[test]
    fn parses_ssh_url_with_port() {
        let t = parse_ssh_target("ssh://example.com:2222");
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, Some(2222));
    }

    #[test]
    fn parses_ssh_url_with_user() {
        let t = parse_ssh_target("ssh://user@example.com");
        assert_eq!(t.host, "user@example.com");
        assert_eq!(t.port, None);
    }

    #[test]
    fn short_hostname_strips_trailing_newline() {
        // What a `/proc/sys/kernel/hostname` read looks like.
        assert_eq!(short_hostname("amaterasu\n"), Some("amaterasu"));
    }

    #[test]
    fn short_hostname_drops_the_domain() {
        assert_eq!(short_hostname("amaterasu.example.com\n"), Some("amaterasu"));
    }

    #[test]
    fn short_hostname_rejects_empty() {
        assert_eq!(short_hostname(""), None);
        assert_eq!(short_hostname("  \n"), None);
    }
}

use std::ffi::OsString;
use std::num::NonZeroU16;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use dix::{Store, StorePath, StorePathInfo, StoreSnapshot};
use tracing::debug;

pub enum Target {
    Local,
    Ssh(SshTarget),
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Self::Local => "local system".to_string(),
            Self::Ssh(t) => t.label(),
        }
    }
}

pub struct SshTarget {
    /// `[user@]host`, as ssh(1) expects it.
    destination: String,
    port: Option<NonZeroU16>,
    /// `ssh[-ng]://[user@]host` without a port: Nix and Lix disagree on URL
    /// port syntax, so the port travels through `NIX_SSHOPTS` instead.
    store_url: String,
}

impl SshTarget {
    fn label(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.store_url),
            None => self.store_url.clone(),
        }
    }

    fn ssh(&self, script: &str) -> Command {
        let mut cmd = Command::new("ssh");
        if let Some(port) = self.port {
            cmd.arg("-p").arg(port.to_string());
        }
        cmd.arg(&self.destination).arg(script);
        cmd
    }

    fn path_info(&self, toplevel: &str) -> Command {
        let mut cmd = Command::new("nix");
        cmd.args(["path-info", "--recursive", "--size", "--store", &self.store_url, toplevel]);
        if let Some(port) = self.port {
            let mut options = OsString::from(format!("-p {port}"));
            if let Some(existing) = std::env::var_os("NIX_SSHOPTS") {
                options.push(" ");
                options.push(existing);
            }
            cmd.env("NIX_SSHOPTS", options);
        }
        cmd
    }
}

/// Parse a target of the form `[user@]host[:port]` or
/// `ssh[-ng]://[user@]host[:port][/...]`. Without a scheme, `ssh-ng` is used.
pub fn parse_ssh_target(raw: &str) -> Result<SshTarget> {
    let invalid = || anyhow!("invalid SSH target: {raw}");

    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme @ ("ssh" | "ssh-ng"), rest)) => (scheme, rest),
        Some(_) => bail!("unsupported SSH target scheme: {raw}"),
        None => ("ssh-ng", raw),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() || authority.contains(['?', '#']) {
        return Err(invalid());
    }
    let (user, address) = match authority.rsplit_once('@') {
        Some(("", _)) => return Err(invalid()),
        Some((user, address)) => (format!("{user}@"), address),
        None => (String::new(), authority),
    };
    let (host, port) = if let Some(bracketed) = address.strip_prefix('[') {
        let (host, rest) = bracketed.split_once(']').ok_or_else(invalid)?;
        let port = if rest.is_empty() {
            None
        } else {
            Some(rest.strip_prefix(':').ok_or_else(invalid)?)
        };
        (host, port)
    } else {
        match address.split_once(':') {
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            // No port, or an unbracketed IPv6 address made of colons.
            _ => (address, None),
        }
    };
    let port = port
        .map(|port| port.parse().with_context(|| format!("invalid SSH port in target: {raw}")))
        .transpose()?;
    if host.is_empty() {
        return Err(invalid());
    }

    let store_url = if host.contains(':') {
        format!("{scheme}://{user}[{host}]")
    } else {
        format!("{scheme}://{user}{host}")
    };
    Ok(SshTarget { destination: format!("{user}{host}"), port, store_url })
}

pub struct System {
    pub machine: String,
    pub toplevel: String,
    /// A remote target reports its selected packages while being identified,
    /// to save a connection; the local store is asked in `fetch_snapshot`.
    selected: Vec<StorePath>,
}

/// Work out the machine identifier (`name`, or else the target's short
/// hostname) and the store path that `link` (e.g. /run/current-system)
/// resolves to.
pub fn identify(target: &Target, name: Option<&str>, link: &str) -> Result<System> {
    let (selected, resolved, hostname) = match target {
        Target::Local => {
            let resolved = std::fs::canonicalize(link)
                .with_context(|| format!("resolving {link}"))?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow!("path {link} contains invalid unicode"))?;
            let hostname = if name.is_none() { local_hostname()? } else { String::new() };
            (Vec::new(), resolved, hostname)
        }
        Target::Ssh(t) => {
            // Everything shares one connection: the SSH handshake costs far
            // more than the commands. A flat `||`/`&&` chain reads left to
            // right in fish as well as POSIX shells. The selected packages
            // are the references of `<link>/sw`, as dix reads them locally;
            // a path without `sw` is not a system and selects nothing.
            let mut script = format!(
                "test ! -e {link}/sw || nix-store --query --references {link}/sw && readlink -f {link}"
            );
            if name.is_none() {
                script.push_str(" && hostname -s");
            }
            let reply = output_of(&mut t.ssh(&script), "remote system lookup")?;
            parse_identity(&reply, name.is_none())
        }
    };

    let machine = name
        .or_else(|| short_hostname(&hostname))
        .context("hostname was empty")?
        .to_string();
    let toplevel = StorePath::try_from(resolved)
        .map(String::from)
        .map_err(|e| anyhow!("{link} did not resolve to a nix store path: {e}"))?;
    let selected = selected
        .into_iter()
        .map(|path| StorePath::try_from(path).map_err(|e| anyhow!("invalid selected path: {e}")))
        .collect::<Result<_>>()?;
    Ok(System { machine, toplevel, selected })
}

/// Split the reply to the remote lookup into the selected paths, the resolved
/// link, and the hostname (empty unless it was asked for).
fn parse_identity(reply: &str, with_hostname: bool) -> (Vec<String>, String, String) {
    let mut lines: Vec<String> = reply
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let hostname = if with_hostname { lines.pop() } else { None };
    let resolved = lines.pop();
    (lines, resolved.unwrap_or_default(), hostname.unwrap_or_default())
}

const PROC_HOSTNAME: &str = "/proc/sys/kernel/hostname";

fn local_hostname() -> Result<String> {
    // Avoids depending on `hostname(1)`, which isn't always installed.
    match std::fs::read_to_string(PROC_HOSTNAME) {
        Ok(raw) => Ok(raw),
        Err(e) => {
            debug!("reading {PROC_HOSTNAME} failed ({e}), falling back to hostname(1)");
            output_of(Command::new("hostname").arg("-s"), "hostname -s")
        }
    }
}

/// Reduce a hostname to its short form the way `hostname -s` does, so a local
/// `/proc` read and a remote `hostname -s` agree on the stored identifier.
fn short_hostname(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let short = trimmed.split('.').next().unwrap_or(trimmed);
    (!short.is_empty()).then_some(short)
}

pub fn fetch_snapshot(target: &Target, system: &System) -> Result<StoreSnapshot> {
    match target {
        Target::Local => Store::open()
            .query_snapshot_document(Path::new(&system.toplevel))
            .map(|doc| doc.snapshot)
            .map_err(|e| anyhow!("querying local nix store: {e}")),
        Target::Ssh(t) => {
            let out = output_of(&mut t.path_info(&system.toplevel), "nix path-info")?;
            Ok(StoreSnapshot { closure: parse_path_info(&out)?, selected: system.selected.clone() })
        }
    }
}

/// Parse `nix path-info --size` output: one `<store path> <NAR size>` per line.
fn parse_path_info(stdout: &str) -> Result<Vec<StorePathInfo>> {
    let mut closure = Vec::with_capacity(stdout.lines().count());
    for line in stdout.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (raw_path, size) = line
            .split_once(char::is_whitespace)
            .with_context(|| format!("missing NAR size in nix path-info output line: {line}"))?;
        let nar_size = size
            .trim_start()
            .parse::<i64>()
            .context("failed to parse NAR size from nix path-info output")?;
        let path = StorePath::try_from(raw_path.to_string())
            .map_err(|e| anyhow!("invalid store path {raw_path}: {e}"))?;
        closure.push(StorePathInfo { path, nar_size: dix::Size::from_bytes(nar_size) });
    }

    if closure.is_empty() {
        bail!("nix path-info returned no store paths");
    }
    Ok(closure)
}

fn output_of(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("could not run {}", cmd.get_program().to_string_lossy()))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    String::from_utf8(out.stdout).with_context(|| format!("non-UTF8 output from {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_targets_and_selects_store_scheme() {
        for (input, host, port, store, label) in [
            ("example.com", "example.com", None, "ssh-ng://example.com", "ssh-ng://example.com"),
            ("user@example.com", "user@example.com", None, "ssh-ng://user@example.com", "ssh-ng://user@example.com"),
            ("ssh://user@example.com", "user@example.com", None, "ssh://user@example.com", "ssh://user@example.com"),
            ("ssh-ng://user@example.com", "user@example.com", None, "ssh-ng://user@example.com", "ssh-ng://user@example.com"),
            ("example.com:2222", "example.com", Some(2222), "ssh-ng://example.com", "ssh-ng://example.com:2222"),
            ("ssh://user@example.com:2222", "user@example.com", Some(2222), "ssh://user@example.com", "ssh://user@example.com:2222"),
            ("ssh-ng://user@example.com:2222/path", "user@example.com", Some(2222), "ssh-ng://user@example.com", "ssh-ng://user@example.com:2222"),
            ("ssh-ng://user@[::1]:2222", "user@::1", Some(2222), "ssh-ng://user@[::1]", "ssh-ng://user@[::1]:2222"),
            ("::1", "::1", None, "ssh-ng://[::1]", "ssh-ng://[::1]"),
        ] {
            let target = parse_ssh_target(input).unwrap();
            assert_eq!(target.destination, host, "{input}");
            assert_eq!(target.port.map(NonZeroU16::get), port, "{input}");
            assert_eq!(target.store_url, store, "{input}");
            assert_eq!(target.label(), label, "{input}");
        }
    }

    #[test]
    fn rejects_invalid_ssh_targets() {
        for input in [
            "", "https://example.com", "ssh://", "ssh-ng://user@",
            "ssh://host:0", "ssh-ng://host:invalid", "ssh-ng://host:65536",
            "ssh-ng://[::1", "ssh-ng://host?port=2222",
        ] {
            assert!(parse_ssh_target(input).is_err(), "{input}");
        }
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

    #[test]
    fn identity_reply_lists_selected_paths_then_toplevel_then_hostname() {
        let reply = "/nix/store/a-pkg\n/nix/store/b-pkg\n/nix/store/c-system\nhost.example.com\n";
        assert_eq!(
            parse_identity(reply, true),
            (
                vec!["/nix/store/a-pkg".to_string(), "/nix/store/b-pkg".to_string()],
                "/nix/store/c-system".to_string(),
                "host.example.com".to_string()
            )
        );
        // With a name override no hostname is asked for; without `sw`
        // nothing is selected.
        assert_eq!(
            parse_identity("/nix/store/c-system\n", false),
            (vec![], "/nix/store/c-system".to_string(), String::new())
        );
        assert_eq!(parse_identity("", true), (vec![], String::new(), String::new()));
    }

    #[test]
    fn path_info_parses_paths_and_sizes() {
        let system = "/nix/store/00000000000000000000000000000000-system";
        let dependency = "/nix/store/00000000000000000000000000000001-dependency";
        let stdout = format!("  {system}\t100  \n\n{dependency}  200\n");

        let closure = parse_path_info(&stdout).unwrap();
        assert_eq!(closure.len(), 2);
        assert_eq!(closure[0].path.as_str(), system);
        assert_eq!(closure[0].nar_size.bytes(), 100);
        assert_eq!(closure[1].path.as_str(), dependency);
        assert_eq!(closure[1].nar_size.bytes(), 200);
    }

    #[test]
    fn path_info_rejects_invalid_output() {
        for (output, expected) in [
            (" \n", "no store paths"),
            ("/nix/store/00000000000000000000000000000000-system", "missing NAR size"),
            ("/invalid 100", "invalid store path"),
            ("/invalid not-a-size", "failed to parse NAR size"),
        ] {
            let err = parse_path_info(output).unwrap_err();
            assert!(err.to_string().contains(expected), "{err:#}");
        }
    }
}

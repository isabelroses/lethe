#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Logs its arguments and answers the remote lookup: selected paths, then
/// the resolved link, then the hostname.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$SSH_LOG"
printf '/nix/store/00000000000000000000000000000001-package\n'
printf '/nix/store/00000000000000000000000000000000-system\ntest-host.example.com\n'
"#;

/// Logs its arguments and `NIX_SSHOPTS`, and prints a two-path closure.
const FAKE_NIX: &str = r#"#!/bin/sh
printf '%s\n' "$*" > "$NIX_LOG"
printf '%s\n' "$NIX_SSHOPTS" > "$OPTIONS_LOG"
printf '/nix/store/00000000000000000000000000000000-system 100\n'
printf '/nix/store/00000000000000000000000000000001-package 200\n'
"#;

fn selected_paths(db: &Path) -> Vec<String> {
    rusqlite::Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT sp.path FROM closures c JOIN store_paths sp ON sp.id = c.store_path_id
             WHERE c.selected ORDER BY sp.path",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn records_remote_targets_with_the_requested_store_and_port() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("lethe-remote-{}-{nonce}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let bin = dir.join("bin");
    fs::create_dir(&bin).unwrap();

    for (name, script) in [("ssh", FAKE_SSH), ("nix", FAKE_NIX)] {
        let path = bin.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    for (index, target, expected_store, expected_ssh, expected_options) in [
        (
            0,
            "example.com",
            "ssh-ng://example.com",
            "example.com",
            "-o BatchMode=yes",
        ),
        (
            1,
            "ssh-ng://user@example.com:2222",
            "ssh-ng://user@example.com",
            "-p 2222 user@example.com",
            "-p 2222 -o BatchMode=yes",
        ),
        (
            2,
            "ssh://user@example.com:2222",
            "ssh://user@example.com",
            "-p 2222 user@example.com",
            "-p 2222 -o BatchMode=yes",
        ),
    ] {
        let ssh_log = dir.join(format!("ssh-{index}"));
        let nix_log = dir.join(format!("nix-{index}"));
        let options_log = dir.join(format!("options-{index}"));
        let result = Command::new(env!("CARGO_BIN_EXE_lethe"))
            .args([
                "--db",
                dir.join(format!("db-{index}")).to_str().unwrap(),
                "record",
                target,
            ])
            .env("PATH", &path)
            .env("SSH_LOG", &ssh_log)
            .env("NIX_LOG", &nix_log)
            .env("OPTIONS_LOG", &options_log)
            .env("NIX_SSHOPTS", "-o BatchMode=yes")
            .env("RUST_LOG", "off")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::read_to_string(ssh_log).unwrap(),
            format!(
                "{expected_ssh} test ! -e /run/current-system/sw || nix-store --query --references \
                 /run/current-system/sw && readlink -f /run/current-system && hostname -s\n"
            )
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).starts_with("recorded deployment for test-host "),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(
            fs::read_to_string(nix_log).unwrap(),
            format!(
                "path-info --recursive --size --store {expected_store} /nix/store/00000000000000000000000000000000-system\n"
            )
        );
        assert_eq!(
            fs::read_to_string(options_log).unwrap().trim(),
            expected_options
        );

        // The selected set is what the target reported, not the toplevel.
        let selected = selected_paths(&dir.join(format!("db-{index}")));
        assert_eq!(selected, ["/nix/store/00000000000000000000000000000001-package"]);
    }

    fs::remove_dir_all(dir).unwrap();
}

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use lix_diff::{DiffPackage, DiffRoot};
use regex::Regex;

pub struct ClosurePathRef<'a> {
    pub path: &'a str,
    pub nar_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageGroup {
    versions: Vec<String>,
    nar_size: i64,
}

/// Build a [`lix_diff::DiffRoot`] by grouping store paths into packages and
/// computing per-package version/size deltas between two closures.
pub fn build_diff_root(
    new_paths: &[ClosurePathRef<'_>],
    old_paths: &[ClosurePathRef<'_>],
) -> DiffRoot {
    let new_groups = group_packages(new_paths);
    let mut old_groups = group_packages(old_paths);
    let mut packages: BTreeMap<String, DiffPackage> = BTreeMap::new();

    for (name, new_group) in new_groups {
        match old_groups.remove(&name) {
            Some(old_group) if old_group != new_group => {
                packages.insert(
                    name,
                    DiffPackage {
                        size_delta: new_group.nar_size - old_group.nar_size,
                        versions_before: old_group.versions,
                        versions_after: new_group.versions,
                    },
                );
            }
            Some(_) => {}
            None => {
                packages.insert(
                    name,
                    DiffPackage {
                        size_delta: new_group.nar_size,
                        versions_before: Vec::new(),
                        versions_after: new_group.versions,
                    },
                );
            }
        }
    }

    for (name, old_group) in old_groups {
        packages.insert(
            name,
            DiffPackage {
                size_delta: -old_group.nar_size,
                versions_before: old_group.versions,
                versions_after: Vec::new(),
            },
        );
    }

    DiffRoot {
        packages,
        schema: String::new(),
    }
}

fn parse_pname_version(path: &str) -> (String, Option<String>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^/nix/store/[a-z0-9]+-(.+?)(-([0-9].*?))?(\.drv)?$").unwrap()
    });

    let base: String = {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 4 {
            parts[..4].join("/")
        } else {
            path.to_string()
        }
    };

    match re.captures(&base) {
        Some(caps) => {
            let pname = caps.get(1).map_or_else(|| base.clone(), |m| m.as_str().to_string());
            let version = caps.get(3).map(|m| m.as_str().to_string());
            (pname, version)
        }
        None => (base, None),
    }
}

fn group_packages(paths: &[ClosurePathRef<'_>]) -> HashMap<String, PackageGroup> {
    let mut result: HashMap<String, PackageGroup> = HashMap::new();
    for p in paths {
        let (pname, version) = parse_pname_version(p.path);
        let entry = result.entry(pname).or_insert_with(|| PackageGroup {
            versions: Vec::new(),
            nar_size: 0,
        });
        entry.versions.push(version.unwrap_or_else(|| "<none>".to_string()));
        entry.nar_size += p.nar_size;
    }
    for group in result.values_mut() {
        group.versions.sort();
    }
    result
}

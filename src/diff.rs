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
                let (before, after) =
                    collapse_changed(&old_group.versions, &new_group.versions);
                packages.insert(
                    name,
                    DiffPackage {
                        size_delta: new_group.nar_size - old_group.nar_size,
                        versions_before: before,
                        versions_after: after,
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
                        versions_after: collapse_runs(&new_group.versions),
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
                versions_before: collapse_runs(&old_group.versions),
                versions_after: Vec::new(),
            },
        );
    }

    DiffRoot {
        packages,
        schema: String::new(),
    }
}

/// Run-length encode consecutive identical versions, collapsing a run of `n`
/// identical entries into a single `"<value> xN"` entry. Callers pass sorted
/// lists, so identical versions are always adjacent. This keeps long stretches
/// like `<none>, <none>, ...` from drowning out the diff.
fn collapse_runs(versions: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < versions.len() {
        let mut j = i + 1;
        while j < versions.len() && versions[j] == versions[i] {
            j += 1;
        }
        out.push(label(&versions[i], j - i));
        i = j;
    }
    out
}

/// Like [`collapse_runs`] but for a changed package, where each `before`/`after`
/// pair is rendered together. Runs of identical pairs are collapsed in lockstep
/// so the two lists stay aligned for the downstream `before -> after` render.
fn collapse_changed(before: &[String], after: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out_before = Vec::new();
    let mut out_after = Vec::new();
    let paired = before.len().min(after.len());
    let mut i = 0;
    while i < paired {
        let mut j = i + 1;
        while j < paired && before[j] == before[i] && after[j] == after[i] {
            j += 1;
        }
        out_before.push(label(&before[i], j - i));
        out_after.push(label(&after[i], j - i));
        i = j;
    }
    // Preserve any unpaired tail verbatim (matches the prior behavior).
    out_before.extend(before[paired..].iter().cloned());
    out_after.extend(after[paired..].iter().cloned());
    (out_before, out_after)
}

fn label(version: &str, count: usize) -> String {
    if count > 1 {
        format!("{version} x{count}")
    } else {
        version.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn collapses_identical_runs() {
        assert_eq!(collapse_runs(&s(&["<none>"; 13])), s(&["<none> x13"]));
        assert_eq!(
            collapse_runs(&s(&["1.5.1", "1.5.1", "1.5.1"])),
            s(&["1.5.1 x3"])
        );
    }

    #[test]
    fn keeps_singletons_uncollapsed() {
        assert_eq!(
            collapse_runs(&s(&["1.5.0", "1.5.1", "1.6.0"])),
            s(&["1.5.0", "1.5.1", "1.6.0"])
        );
    }

    #[test]
    fn mixed_runs() {
        assert_eq!(
            collapse_runs(&s(&["1.5.0", "1.5.0", "1.5.1"])),
            s(&["1.5.0 x2", "1.5.1"])
        );
    }

    #[test]
    fn changed_collapses_aligned_pairs() {
        let before = s(&["<none>"; 13]);
        let after = s(&["<none>"; 13]);
        assert_eq!(
            collapse_changed(&before, &after),
            (s(&["<none> x13"]), s(&["<none> x13"]))
        );
    }

    #[test]
    fn changed_keeps_distinct_pairs_separate() {
        let before = s(&["1.0", "1.0"]);
        let after = s(&["2.0", "3.0"]);
        assert_eq!(
            collapse_changed(&before, &after),
            (s(&["1.0", "1.0"]), s(&["2.0", "3.0"]))
        );
    }
}

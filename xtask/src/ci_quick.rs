//! `cargo xtask ci --quick`: the checks a branch's own changes can break, for a pre-push check.
//!
//! The full `ci` builds the workspace four ways (clippy, heif, tests, wasm) plus the CLI for the
//! parity check, ~23 minutes cold. `--quick` looks at what changed since the merge base with
//! `origin/main` (or `--base REF`), committed or not, and runs:
//!
//! - fmt, layers, assets: always (seconds);
//! - clippy on the whole workspace (check builds only; catches callers broken by an API change);
//! - the tests of the changed packages only (not their dependents);
//! - heif when `lightcraft-codecs` / `lightcraft-heif` changed;
//! - parity when the tracker docs, a command/control crate, or a file's path changed (renames and
//!   deletions: the tracker cites paths);
//! - the wasm check of the changed packages that must build for wasm32.
//!
//! A change to a workspace-wide file (root `Cargo.toml`, `Cargo.lock`, lint or toolchain config)
//! tests everything. The full `ci` (GitHub's CI on every PR) stays the merge gate.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Root-level files whose change can affect every package.
const WORKSPACE_WIDE: &[&str] =
    &["Cargo.toml", "Cargo.lock", "clippy.toml", "rustfmt.toml", ".rustfmt.toml", "rust-toolchain", "rust-toolchain.toml"];
const WORKSPACE_WIDE_DIRS: &[&str] = &[".cargo/"];
/// Packages whose ids or paths the parity tracker cites (commands, develop controls, UI menus).
const PARITY_PACKAGES: &[&str] = &["lightcraft-engine", "lightcraft-develop", "lightcraft-ui-egui", "lightcraft-cli"];
const PARITY_DOCS: &[&str] = &["docs/parity-checklist.md", "docs/gaps.md"];
const HEIF_PACKAGES: &[&str] = &["lightcraft-codecs", "lightcraft-heif"];

/// A changed path, relative to the workspace root; `gone` when deleted or renamed away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub path: String,
    pub gone: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Workspace packages with a changed file.
    pub packages: BTreeSet<String>,
    /// A workspace-wide file changed: test everything.
    pub workspace_wide: bool,
    pub heif: bool,
    pub parity: bool,
    /// What the changes were measured against, for the log and the report.
    pub base: String,
    pub files: usize,
}

impl Plan {
    /// Whether anything compiled changed (otherwise clippy and the tests are skipped).
    pub fn rust(&self) -> bool {
        self.workspace_wide || !self.packages.is_empty()
    }
}

/// Decide what to run. `packages` are (name, directory relative to the root, `/`-separated).
pub fn plan(changed: &[Changed], packages: &[(String, String)], base: String) -> Plan {
    let mut p = Plan { base, files: changed.len(), ..Plan::default() };
    for c in changed {
        let path = c.path.as_str();
        if WORKSPACE_WIDE.contains(&path) || WORKSPACE_WIDE_DIRS.iter().any(|d| path.starts_with(d)) {
            p.workspace_wide = true;
        }
        if PARITY_DOCS.contains(&path) || c.gone {
            p.parity = true;
        }
        if let Some(name) = owner(path, packages) {
            p.packages.insert(name.to_owned());
        }
    }
    p.heif = p.workspace_wide || HEIF_PACKAGES.iter().any(|h| p.packages.contains(*h));
    p.parity |= p.workspace_wide || PARITY_PACKAGES.iter().any(|h| p.packages.contains(*h));
    p
}

/// The package whose directory (the innermost one) contains `path`.
pub fn owner<'a>(path: &str, packages: &'a [(String, String)]) -> Option<&'a str> {
    packages
        .iter()
        .filter(|(_, dir)| !dir.is_empty() && path.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/')))
        .max_by_key(|(_, dir)| dir.len())
        .map(|(name, _)| name.as_str())
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").args(args).current_dir(root).output().map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `git diff --name-status` output.
fn parse_name_status(out: &str) -> Vec<Changed> {
    let mut v = Vec::new();
    for line in out.lines() {
        let mut cols = line.split('\t');
        let (Some(status), Some(first)) = (cols.next(), cols.next()) else { continue };
        match (status.chars().next(), cols.next()) {
            // renamed / copied: old path, new path
            (Some(k @ ('R' | 'C')), Some(second)) => {
                v.push(Changed { path: first.to_owned(), gone: k == 'R' });
                v.push(Changed { path: second.to_owned(), gone: false });
            }
            (Some('D'), _) => v.push(Changed { path: first.to_owned(), gone: true }),
            _ => v.push(Changed { path: first.to_owned(), gone: false }),
        }
    }
    v
}

/// The files changed since the merge base with `base` (default `origin/main`, else `main`):
/// committed, staged, unstaged and untracked. Returns the base description and the files.
pub fn changed_files(root: &Path, base: Option<&str>) -> Result<(String, Vec<Changed>), String> {
    let base = match base {
        Some(b) => b.to_owned(),
        None => ["origin/main", "main"]
            .into_iter()
            .find(|b| git(root, &["rev-parse", "--verify", "--quiet", &format!("{b}^{{commit}}")]).is_ok())
            .ok_or("no origin/main or main to compare with; pass --base REF")?
            .to_owned(),
    };
    let merge_base = git(root, &["merge-base", "HEAD", &base])?.trim().to_owned();
    let mut files = parse_name_status(&git(root, &["diff", "--name-status", "-M", &merge_base])?);
    for path in git(root, &["ls-files", "--others", "--exclude-standard"])?.lines() {
        files.push(Changed { path: path.to_owned(), gone: false });
    }
    let short: String = merge_base.chars().take(10).collect();
    Ok((format!("{base} (merge base {short})"), files))
}

/// (name, root-relative directory) of each workspace package, from `cargo metadata`.
pub fn packages(root: &Path, metadata: &serde_json::Value) -> Vec<(String, String)> {
    let roots: Vec<std::path::PathBuf> = [Some(root.to_path_buf()), root.canonicalize().ok()].into_iter().flatten().collect();
    metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p["name"].as_str()?;
            let dir = Path::new(p["manifest_path"].as_str()?).parent()?;
            let rel = roots.iter().find_map(|r| dir.strip_prefix(r).ok())?;
            let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            Some((name.to_owned(), rel))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(path: &str) -> Changed {
        Changed { path: path.into(), gone: false }
    }

    fn pkgs() -> Vec<(String, String)> {
        [
            ("lightcraft-raw", "crates/raw"),
            ("lightcraft-codecs", "crates/codecs"),
            ("lightcraft-engine", "crates/engine"),
            ("lightcraft-cli", "apps/lightcraft-cli"),
            ("xtask", "xtask"),
            ("lightcraft-raw-nested", "crates/raw/nested"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect()
    }

    #[test]
    fn changed_files_pick_their_innermost_package() {
        let p = plan(&[ch("crates/raw/src/lib.rs"), ch("crates/raw/nested/src/x.rs"), ch("crates/rawx/y.rs")], &pkgs(), "b".into());
        assert_eq!(p.packages.iter().map(String::as_str).collect::<Vec<_>>(), ["lightcraft-raw", "lightcraft-raw-nested"]);
        assert!(p.rust() && !p.workspace_wide && !p.heif && !p.parity);
    }

    #[test]
    fn docs_only_skip_the_builds_but_tracker_docs_run_parity() {
        let p = plan(&[ch("README.md"), ch("docs/raw-parity.md")], &pkgs(), "b".into());
        assert!(!p.rust() && !p.parity);
        let p = plan(&[ch("docs/gaps.md")], &pkgs(), "b".into());
        assert!(!p.rust() && p.parity);
    }

    #[test]
    fn workspace_files_heif_and_parity_triggers() {
        let p = plan(&[ch("Cargo.lock")], &pkgs(), "b".into());
        assert!(p.workspace_wide && p.rust() && p.heif && p.parity);
        let p = plan(&[ch("crates/codecs/src/heif.rs")], &pkgs(), "b".into());
        assert!(p.heif && !p.parity);
        let p = plan(&[ch("crates/engine/src/commands.rs")], &pkgs(), "b".into());
        assert!(p.parity && !p.heif);
        let p = plan(&[Changed { path: "crates/raw/src/old.rs".into(), gone: true }], &pkgs(), "b".into());
        assert!(p.parity, "a removed path may be cited by the tracker");
    }

    #[test]
    fn name_status_parses_renames_and_deletions() {
        let v = parse_name_status("M\ta.rs\nR087\told.rs\tnew.rs\nD\tgone.rs\nA\tadded.rs\n\n");
        assert_eq!(
            v,
            [ch("a.rs"), Changed { path: "old.rs".into(), gone: true }, ch("new.rs"), Changed { path: "gone.rs".into(), gone: true }, ch("added.rs"),]
        );
    }
}

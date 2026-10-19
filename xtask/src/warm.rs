//! `cargo xtask warm`: start a new worktree's `target/` from a copy-on-write clone of a seed
//! checkout's, so its first build recompiles only the packages that differ.
//!
//! sccache can't share the workspace's own crates between worktrees (the absolute
//! `CARGO_MANIFEST_DIR` is part of every key; Rust support for `SCCACHE_BASEDIRS` is unreleased),
//! so every new worktree used to rebuild all of LightCraft. Cargo's own artifacts do carry over:
//! its hashes use paths relative to the workspace, and freshness is by modification time. So
//! `warm`:
//!
//! 1. clones the seed's `target/` (APFS `clonefile` on macOS, reflinks on Linux: seconds, and no
//!    disk until files are rewritten);
//! 2. gives every file whose bytes equal the seed's the seed's modification time, so cargo judges
//!    it exactly as it would in the seed;
//! 3. stamps every file of a package with any difference (added, removed, changed — fixtures
//!    too) with the current time, so that package and its dependents rebuild (an earlier warm may
//!    have given them old times).
//!
//! Reused crates keep the seed's `CARGO_MANIFEST_DIR` (`env!` uses, debuginfo paths): a reused
//! test reads its fixtures from the seed checkout, whose files equal this one's at warm time.
//! Keep the seed on a clean main and refresh it with `cargo xtask warm --seed`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const USAGE: &str = "usage: cargo xtask warm [--from SEED_CHECKOUT] [--force] | cargo xtask warm --seed";

pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let force = args.contains(&"--force");
    let seed_mode = args.contains(&"--seed");
    let from_at = args.iter().position(|a| *a == "--from");
    let from = from_at.map(|i| args.get(i + 1).copied().ok_or("warm: --from needs a checkout directory")).transpose()?;
    let unknown = args.iter().enumerate().find(|(i, a)| !matches!(**a, "--force" | "--seed" | "--from") && from_at.is_none_or(|f| *i != f + 1));
    if let Some((_, other)) = unknown {
        return Err(format!("warm: unknown option `{other}`\n{USAGE}"));
    }
    if seed_mode {
        return build_seed(root);
    }
    let seed = match from {
        Some(f) => PathBuf::from(f),
        None => main_worktree(root)?,
    };
    let same = |a: &Path, b: &Path| a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b);
    if same(&seed, root) {
        return Err(format!("warm: {} is the seed checkout itself; refresh its builds with `cargo xtask warm --seed`", root.display()));
    }
    let seed_target = seed.join("target");
    if !has_workspace_builds(&seed_target) {
        return Err(format!("warm: {} has no workspace builds; run `cargo xtask warm --seed` in {}", seed_target.display(), seed.display()));
    }
    let dest = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(d) if Path::new(&d).is_absolute() => PathBuf::from(d),
        Some(d) => root.join(d),
        None => root.join("target"),
    };
    if has_workspace_builds(&dest) && !force {
        return Err(format!("warm: {} already has workspace builds; --force replaces them", dest.display()));
    }

    let t = Instant::now();
    clone_dir(&seed_target, &dest)?;
    eprintln!("cloned {} → {} in {:.1} s", seed_target.display(), dest.display(), t.elapsed().as_secs_f64());

    let t = Instant::now();
    let packages = crate::ci_quick::packages(root, &crate::metadata()?);
    let ours = listed_files(root)?;
    let theirs = listed_files(&seed)?;
    let mut differ: BTreeSet<&str> = ours.symmetric_difference(&theirs).map(String::as_str).collect();
    for path in ours.intersection(&theirs) {
        if !same_bytes(&root.join(path), &seed.join(path)) {
            differ.insert(path);
        }
    }
    let rebuilt: BTreeSet<&str> = differ.iter().filter_map(|p| crate::ci_quick::owner(p, &packages)).collect();
    let now = std::time::SystemTime::now();
    let mut synced = 0usize;
    for path in &ours {
        let dirty = differ.contains(path.as_str()) || crate::ci_quick::owner(path, &packages).is_some_and(|o| rebuilt.contains(o));
        let set = if dirty { set_mtime(&root.join(path), now) } else { copy_mtime(&seed.join(path), &root.join(path)) };
        if set.is_ok() && !dirty {
            synced += 1;
        }
    }
    eprintln!("{synced} unchanged files take the seed's timestamps ({} differ; {:.1} s)", differ.len(), t.elapsed().as_secs_f64());
    if rebuilt.is_empty() {
        eprintln!("no package differs from the seed: builds the seed has done are fresh here");
    } else {
        eprintln!("differ from the seed, rebuilt with their dependents on the next build: {}", rebuilt.into_iter().collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

/// `--seed`: the builds `cargo xtask ci` does first (clippy, the workspace tests), with its
/// settings, into this checkout's `target/`.
fn build_seed(root: &Path) -> Result<(), String> {
    let _slot = crate::ci_lock::acquire(root, "cargo xtask warm --seed")?;
    for args in [&["clippy", "--workspace", "--all-targets"][..], &["test", "--workspace", "--no-run"][..]] {
        let mut c = crate::cargo();
        c.args(args).env_remove("CARGO_TARGET_DIR");
        crate::ci_env(&mut c);
        crate::run(c, &format!("cargo {}", args.join(" ")))?;
    }
    eprintln!("seed ready: `cargo xtask warm` in a new worktree now starts from {}", root.join("target").display());
    Ok(())
}

/// The main checkout of the repository: the first entry of `git worktree list`.
fn main_worktree(root: &Path) -> Result<PathBuf, String> {
    let out =
        Command::new("git").args(["worktree", "list", "--porcelain"]).current_dir(root).output().map_err(|e| format!("git worktree list: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| "warm: no main worktree found; pass --from SEED_CHECKOUT".to_owned())
}

/// Whether `target` holds builds of workspace packages (not only xtask's own).
fn has_workspace_builds(target: &Path) -> bool {
    std::fs::read_dir(target.join("debug").join(".fingerprint"))
        .map(|dir| dir.flatten().any(|e| e.file_name().to_string_lossy().starts_with("lightcraft-")))
        .unwrap_or(false)
}

/// Clone `from` to `to` copy-on-write, replacing `to`. The old directory is renamed aside first:
/// the running xtask binary may live in it (fine to unlink on Unix).
fn clone_dir(from: &Path, to: &Path) -> Result<(), String> {
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "target".into());
    let tmp = to.with_file_name(format!("{name}.warm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut c = if cfg!(target_os = "macos") {
        let mut c = Command::new("cp");
        c.arg("-c");
        c
    } else if cfg!(target_os = "linux") {
        let mut c = Command::new("cp");
        c.arg("--reflink=always");
        c
    } else {
        return Err("warm: needs copy-on-write clones (macOS APFS, or reflinks on Linux btrfs / XFS)".into());
    };
    c.arg("-R").arg(from).arg(&tmp);
    let out = c.output().map_err(|e| format!("cp: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "warm: copy-on-write clone failed (does this file system support it?): {}",
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("")
        ));
    }
    let old = to.with_file_name(format!("{name}.old-{}", std::process::id()));
    if to.exists() {
        std::fs::rename(to, &old).map_err(|e| format!("warm: move {} aside: {e}", to.display()))?;
    }
    std::fs::rename(&tmp, to).map_err(|e| format!("warm: {}: {e}", to.display()))?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Tracked and untracked (not ignored) files of a checkout, relative paths.
fn listed_files(checkout: &Path) -> Result<BTreeSet<String>, String> {
    let out = Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(checkout)
        .output()
        .map_err(|e| format!("git ls-files in {}: {e}", checkout.display()))?;
    if !out.status.success() {
        return Err(format!("git ls-files in {}: {}", checkout.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    // deleted but still in the index: not a file here
    Ok(String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty() && checkout.join(p).is_file()).map(str::to_owned).collect())
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else { return false };
    ma.len() == mb.len() && matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

fn copy_mtime(from: &Path, to: &Path) -> std::io::Result<()> {
    set_mtime(to, std::fs::metadata(from)?.modified()?)
}

fn set_mtime(path: &Path, time: std::time::SystemTime) -> std::io::Result<()> {
    std::fs::File::options().write(true).open(path)?.set_modified(time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_builds_are_told_from_xtask_only() {
        let dir = std::env::temp_dir().join(format!("xtask-warm-{}", std::process::id()));
        let fp = dir.join("debug").join(".fingerprint");
        std::fs::create_dir_all(fp.join("xtask-0123")).unwrap();
        assert!(!has_workspace_builds(&dir));
        std::fs::create_dir_all(fp.join("lightcraft-geom-4567")).unwrap();
        assert!(has_workspace_builds(&dir));
        assert!(!has_workspace_builds(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_and_bytes_helpers() {
        let dir = std::env::temp_dir().join(format!("xtask-warm-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        std::fs::write(&a, "same").unwrap();
        std::fs::write(&b, "same").unwrap();
        std::fs::write(&c, "diff").unwrap();
        assert!(same_bytes(&a, &b) && !same_bytes(&a, &c) && !same_bytes(&a, &dir.join("none")));
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800);
        std::fs::File::options().write(true).open(&a).unwrap().set_modified(old).unwrap();
        copy_mtime(&a, &b).unwrap();
        assert_eq!(std::fs::metadata(&b).unwrap().modified().unwrap(), old);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

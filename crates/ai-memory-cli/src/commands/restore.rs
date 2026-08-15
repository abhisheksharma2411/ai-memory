//! `ai-memory restore --from <tarball>` — restore a backup tarball.
//!
//! Refuses to overwrite a non-empty data dir unless `--force` is given.
//! Refuses while another `ai-memory` process is alive. After extraction,
//! re-opens the store so any pending migrations run (and a corrupt
//! snapshot fails loudly).
//!
//! # Exception to invariant §16
//!
//! `restore` is one of the documented exceptions to the rule that the CLI
//! is always a thin HTTP client. Restoration is a lifecycle operation that
//! fundamentally requires the server to be stopped: extracting a tarball
//! over a live SQLite WAL writer would corrupt the database. The sysinfo
//! guard at the top of `run` enforces this precondition by refusing to
//! proceed when any sibling `ai-memory` process is detected.

use ai_memory_store::Store;
use ai_memory_store::session_purge::SessionTombstone;
use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::path::{Component, Path};
use tracing::info;

use crate::cli::RestoreArgs;
use crate::config::Config;
use crate::process_guard::{busy_message, sibling_processes};

/// Run the `restore` subcommand.
///
/// # Errors
/// Returns an error if another `ai-memory` process is running, the
/// data dir is non-empty without `--force`, the tarball cannot be
/// extracted, or the restored store fails to open.
pub fn run(config: &Config, args: RestoreArgs) -> Result<()> {
    let siblings = sibling_processes();
    if !siblings.is_empty() {
        bail!(busy_message("restore", &siblings));
    }

    if !args.from.is_file() {
        bail!("source tarball {} not found", args.from.display());
    }

    // Read the ledger BEFORE the overwrite destroys the database holding it.
    let ledger = load_ledger(config, &args)?;

    let wiki = config.data_dir.join("wiki");
    let db = config.data_dir.join("db").join("memory.sqlite");
    if (wiki.is_dir() && std::fs::read_dir(&wiki)?.next().is_some()) || db.is_file() {
        if !args.force {
            bail!(
                "refusing to restore: data dir at {} is non-empty (pass --force to overwrite)",
                config.data_dir.display(),
            );
        }
        // Force path: drop the existing wiki + db so the tarball can
        // populate them cleanly. Keep config.toml, logs/, models/.
        for sub in ["wiki", "db"] {
            let path = config.data_dir.join(sub);
            if path.exists() {
                std::fs::remove_dir_all(&path)?;
            }
        }
    }
    std::fs::create_dir_all(&config.data_dir)?;

    let file = std::fs::File::open(&args.from)
        .with_context(|| format!("opening {}", args.from.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    unpack_checked_archive(&mut archive, &config.data_dir)
        .with_context(|| format!("extracting into {}", config.data_dir.display()))?;
    info!(from = %args.from.display(), into = %config.data_dir.display(), "tarball extracted");

    // Open the store so refinery applies any pending migrations and the SQLite
    // file is validated.
    let store = Store::open(&config.data_dir).context("opening restored store")?;

    // Reapply the tombstone ledger BEFORE the restored state is available
    // (#387). A snapshot taken before a purge contains everything that purge
    // removed; restoring it without this silently resurrects deleted content,
    // and the operator has no way to notice.
    drop(store);
    let reapplied = reapply_tombstones(config, &ledger)?;

    info!("restore complete");
    println!(
        "restored {} -> {}",
        args.from.display(),
        config.data_dir.display()
    );
    if reapplied > 0 {
        println!("re-purged {reapplied} session(s) the tombstone ledger says were deleted");
    }
    Ok(())
}

/// Read the tombstone ledger that must survive this restore.
///
/// Prefers `--tombstones-from`, which is what makes restoring into a FRESH
/// directory safe: there is no current database to carry a ledger over from, so
/// without an explicit one a pre-purge snapshot cannot be declared a safe
/// restoration.
fn load_ledger(config: &Config, args: &RestoreArgs) -> Result<Vec<SessionTombstone>> {
    let source = args
        .tombstones_from
        .clone()
        .unwrap_or_else(|| config.data_dir.clone());
    let db = source.join("db").join("memory.sqlite");
    if args.tombstones_from.is_some() && !db.is_file() {
        bail!(
            "--tombstones-from {} has no db/memory.sqlite to read a tombstone ledger from",
            source.display()
        );
    }
    ai_memory_store::session_purge::read_ledger_from_db(&db)
        .with_context(|| format!("reading the tombstone ledger from {}", db.display()))
}

/// Re-purge every session the ledger says was deleted but the snapshot brought
/// back, then fail closed if any survives.
fn reapply_tombstones(config: &Config, ledger: &[SessionTombstone]) -> Result<usize> {
    if ledger.is_empty() {
        return Ok(0);
    }
    let db = config.data_dir.join("db").join("memory.sqlite");
    let repurged = ai_memory_store::session_purge::reapply_ledger(&db, ledger)
        .context("reapplying the tombstone ledger to the restored database")?;

    // The wiki files and git history came back with the snapshot too, so the
    // filesystem layers must be reapplied, not just the rows.
    let wiki_root = config.data_dir.join("wiki");
    let mut repo_paths = Vec::new();
    for session in &repurged {
        for path in &session.repo_paths {
            let _ = std::fs::remove_file(wiki_root.join(path));
            repo_paths.push(path.clone());
        }
        info!(session = %session.session_id, pages = session.repo_paths.len(), "re-purged a resurrected session");
    }
    if !repo_paths.is_empty() {
        ai_memory_wiki::git_purge::purge_paths_from_history(&wiki_root, &repo_paths)
            .context("removing resurrected pages from restored wiki history")?;
    }

    // Fail closed: a restore that cannot prove the ledger holds is not a safe
    // restoration, and reporting success would be worse than refusing.
    let still_present = ai_memory_store::session_purge::count_resurrected(&db)
        .context("verifying the restored state against the tombstone ledger")?;
    if still_present > 0 {
        bail!(
            "restore aborted: {still_present} purged session(s) survive in the restored state. \
             The data directory is NOT safe to serve."
        );
    }
    info!(repurged = repurged.len(), "tombstone ledger reapplied");
    Ok(repurged.len())
}

fn unpack_checked_archive<R: std::io::Read>(
    archive: &mut tar::Archive<R>,
    data_dir: &Path,
) -> Result<()> {
    archive.set_preserve_permissions(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let entry_type = entry.header().entry_type();
        validate_restore_entry(&path, entry_type)?;
        entry
            .unpack_in(data_dir)
            .with_context(|| format!("extracting {}", path.display()))?;
    }
    Ok(())
}

fn validate_restore_entry(path: &Path, entry_type: tar::EntryType) -> Result<()> {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("backup contains unsafe path: {}", path.display());
    }
    if entry_type.is_symlink() || entry_type.is_hard_link() {
        bail!("backup contains unsupported link entry: {}", path.display());
    }
    if !(entry_type.is_file() || entry_type.is_dir()) {
        bail!("backup contains unsupported entry type: {}", path.display());
    }
    let path_str = path.to_string_lossy();
    let allowed = if entry_type.is_dir() {
        path_str == "wiki" || path_str.starts_with("wiki/") || path_str == "db"
    } else {
        path_str == "config.toml" || path_str == "db/memory.sqlite" || path_str.starts_with("wiki/")
    };
    if !allowed {
        bail!("backup contains unexpected path: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_with_entry(path: &str, entry_type: tar::EntryType) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_entry_type(entry_type);
            if entry_type.is_symlink() || entry_type.is_hard_link() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            let body: &[u8] = if entry_type.is_file() { b"body" } else { b"" };
            header.set_size(body.len() as u64);
            header.set_cksum();
            builder.append(&header, body).unwrap();
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn restore_accepts_expected_backup_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.append_dir("wiki", tmp.path()).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("wiki/default/project/notes/x.md").unwrap();
            header.set_size(4);
            header.set_cksum();
            builder.append(&header, &b"body"[..]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("db/memory.sqlite").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("config.toml").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
            builder.finish().unwrap();
        }

        let restore_dir = tempfile::TempDir::new().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        unpack_checked_archive(&mut archive, restore_dir.path()).unwrap();
        assert!(
            restore_dir
                .path()
                .join("wiki/default/project/notes/x.md")
                .is_file()
        );
        assert!(restore_dir.path().join("db/memory.sqlite").is_file());
        assert!(restore_dir.path().join("config.toml").is_file());
    }

    #[test]
    fn restore_rejects_link_entries() {
        for entry_type in [tar::EntryType::symlink(), tar::EntryType::hard_link()] {
            let bytes = archive_with_entry("wiki/link.md", entry_type);
            let restore_dir = tempfile::TempDir::new().unwrap();
            let mut archive = tar::Archive::new(bytes.as_slice());
            let err = unpack_checked_archive(&mut archive, restore_dir.path()).unwrap_err();
            assert!(err.to_string().contains("unsupported link entry"));
        }
    }

    #[test]
    fn restore_rejects_unexpected_or_unsafe_paths() {
        for path in ["../config.toml", "/tmp/x"] {
            let err = validate_restore_entry(Path::new(path), tar::EntryType::file()).unwrap_err();
            assert!(
                err.to_string().contains("unsafe path"),
                "unexpected error for {path}: {err}"
            );
        }

        let path = "db/extra.sqlite";
        let bytes = archive_with_entry(path, tar::EntryType::file());
        let restore_dir = tempfile::TempDir::new().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let err = unpack_checked_archive(&mut archive, restore_dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("unexpected path"),
            "unexpected error for {path}: {err}"
        );
    }
}

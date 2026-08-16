//! Physical removal of paths from the managed wiki repository's history
//! (#387, stage C).
//!
//! Deleting a file and committing leaves every prior version reachable in git
//! history — and `restore-page` will happily hand one back. Strong deletion
//! means the content must be gone from the object database itself, not merely
//! unreferenced: working tree, index, refs, reflogs, loose objects, packs, and
//! dangling objects alike.
//!
//! ## Why a rebuild rather than an in-place rewrite
//!
//! git never mutates objects; it only stops referencing them. Rewriting history
//! in place therefore leaves the old commits, trees, and blobs in the ODB,
//! reachable through reflogs and recoverable by `git fsck --unreachable` long
//! after the rewrite "succeeded". Building a NEW repository and keeping only
//! what survives is the only construction where the absence is a property of
//! the result rather than of a cleanup pass that might not have run.
//!
//! ## Crash safety
//!
//! The swap is two renames (Rust offers no portable atomic directory exchange,
//! and `renameat2(RENAME_EXCHANGE)` would require `unsafe`, which this
//! workspace forbids). A durable journal records the purge across them, so an
//! interrupted swap converges on the purged state when
//! [`recover_interrupted_purge`] runs at startup rather than silently leaving
//! the old history in place.
//!
//! Everything here runs on fixture repositories in tests; the caller is
//! expected to hold the wiki mutation lock for the whole operation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use git2::{Oid, Repository, TreeWalkMode, TreeWalkResult};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{WikiError, WikiResult};

/// Filename of the durable purge journal, written in the wiki root.
const JOURNAL_NAME: &str = ".ai-memory-purge-journal.json";

/// Prefix for both transient directories, so recovery can recognize them and
/// no other code mistakes them for wiki content.
const SCRATCH_PREFIX: &str = ".ai-memory-purge-";

/// Durable record of an in-flight history purge.
///
/// Written before the first rename and removed only after the last one, so its
/// presence at startup always means "a purge was interrupted mid-swap".
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PurgeJournal {
    /// Unique id of this purge, and the suffix of its scratch directories.
    purge_id: String,
    /// Repo-relative paths being removed from every commit.
    paths: Vec<String>,
    /// How far the swap got.
    state: JournalState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    /// The replacement repository is built and verified but not yet installed.
    /// The live `.git` is still the old one.
    Prepared,
    /// The old `.git` has been moved aside; the replacement may or may not be
    /// installed yet. Recovery finishes the install.
    Swapping,
}

/// What a history purge must remove, and how.
///
/// Three distinct kinds of residue, because the naive "delete these paths" only
/// covers the first — and a live end-to-end run is what surfaced the other two:
///
/// 1. **Whole paths**, e.g. the session's own pages: removed from every commit.
/// 2. **Shared files**, e.g. the monthly log: the file belongs to every session
///    that wrote to it, so removing the path would destroy other sessions'
///    history. Instead each historical version is rewritten with the purged
///    session's marked lines removed.
/// 3. **Commit messages**, which embed the session's own prompt text. The
///    rebuilt commit carries the message verbatim unless it is redacted.
#[derive(Debug, Clone, Default)]
pub struct HistoryPurgeSpec {
    /// Repo-relative paths removed from every commit outright.
    pub doomed_paths: Vec<String>,
    /// Repo-relative paths kept, with `line_marker` lines scrubbed from every
    /// historical version.
    pub scrub_paths: Vec<String>,
    /// Marker identifying the purged session's lines inside a scrubbed file.
    /// Empty disables line scrubbing.
    pub line_marker: String,
    /// Commit-message prefix identifying commits this session produced, e.g.
    /// `session 11111111`. Matched as an identity handle, never as free text.
    /// Empty disables message redaction.
    pub commit_prefix: String,
}

impl HistoryPurgeSpec {
    /// Whether this spec would change anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.doomed_paths.is_empty() && self.scrub_paths.is_empty()
    }
}

/// What a history purge removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitPurgeReport {
    /// Commits carried over into the rebuilt repository.
    pub commits_rewritten: usize,
    /// Objects in the old database, counted before the rebuild.
    pub objects_before: usize,
    /// Objects in the rebuilt database.
    pub objects_after: usize,
    /// Blob objects that existed only through the purged paths and are absent
    /// from the rebuilt database. This is the number that makes the deletion
    /// real rather than merely unreferenced.
    pub blobs_destroyed: usize,
    /// Repositories rebuilt (0 when there was no history to rewrite, else 1).
    pub repositories_rebuilt: usize,
    /// Historical versions of shared files rewritten to drop the purged
    /// session's lines.
    pub blobs_scrubbed: usize,
    /// Commit messages redacted because they carried the session's own text.
    pub messages_redacted: usize,
}

fn git_err(e: git2::Error) -> WikiError {
    WikiError::Io(std::io::Error::other(format!("git purge: {e}")))
}

fn io_err(context: &str, e: std::io::Error) -> WikiError {
    WikiError::Io(std::io::Error::other(format!("git purge: {context}: {e}")))
}

fn purge_err(msg: impl Into<String>) -> WikiError {
    WikiError::Io(std::io::Error::other(msg.into()))
}

/// Remove `paths` from every commit in the wiki repository at `root`.
///
/// Returns a report whose `blobs_destroyed` count is the load-bearing one: it
/// is the number of blob objects that existed only through the purged paths and
/// are provably absent from the rebuilt object database.
///
/// # Errors
/// Returns [`WikiError`] if the repository cannot be read, the rebuild fails
/// verification, or the swap cannot be completed. On a verification failure the
/// live repository is left untouched.
pub fn purge_paths_from_history(
    root: &Path,
    spec: &HistoryPurgeSpec,
) -> WikiResult<GitPurgeReport> {
    if spec.is_empty() {
        return Ok(GitPurgeReport::default());
    }
    let paths = &spec.doomed_paths;
    let doomed: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let repo = Repository::open(root).map_err(git_err)?;

    let objects_before = count_objects(&repo)?;
    let doomed_blobs = blobs_reachable_only_through(&repo, &doomed)?;

    // Distinct per attempt so a crashed run's scratch directories can never be
    // mistaken for this one's; the journal records which id is live.
    let purge_id = Oid::hash_object(
        git2::ObjectType::Blob,
        format!("{}-{}", root.display(), objects_before).as_bytes(),
    )
    .map_err(git_err)?
    .to_string()[..16]
        .to_string();
    let scratch = root.join(format!("{SCRATCH_PREFIX}new-{purge_id}"));
    let retired = root.join(format!("{SCRATCH_PREFIX}old-{purge_id}"));
    // A previous crashed attempt must not be mistaken for this one's output.
    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_dir_all(&retired);

    let report = build_replacement(
        &repo,
        &scratch,
        &doomed,
        spec,
        objects_before,
        &doomed_blobs,
    );
    let mut report = match report {
        Ok(report) => report,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };

    // Verify BEFORE anything destructive: a failure here must leave the live
    // repository exactly as it was.
    if let Err(e) = verify_replacement(&scratch, &doomed, spec, &doomed_blobs) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }

    write_journal(
        root,
        &PurgeJournal {
            purge_id: purge_id.clone(),
            paths: paths.to_vec(),
            state: JournalState::Prepared,
        },
    )?;

    install_replacement(root, &scratch, &retired, &purge_id, paths)?;

    report.blobs_destroyed = doomed_blobs.len();
    report.repositories_rebuilt = 1;
    info!(
        commits = report.commits_rewritten,
        objects_before = report.objects_before,
        objects_after = report.objects_after,
        blobs_destroyed = report.blobs_destroyed,
        "wiki history rebuilt without purged paths"
    );
    Ok(report)
}

/// Move the rebuilt `.git` into place and destroy the old one.
///
/// Ordered so that every interruption is recoverable: the journal advances to
/// `Swapping` before the live `.git` moves, and is only removed once the old
/// directory is physically gone.
fn install_replacement(
    root: &Path,
    scratch: &Path,
    retired: &Path,
    purge_id: &str,
    paths: &[String],
) -> WikiResult<()> {
    let live_git = root.join(".git");
    let new_git = scratch.join(".git");

    write_journal(
        root,
        &PurgeJournal {
            purge_id: purge_id.to_string(),
            paths: paths.to_vec(),
            state: JournalState::Swapping,
        },
    )?;

    std::fs::rename(&live_git, retired).map_err(|e| io_err("retiring old .git", e))?;
    if let Err(e) = std::fs::rename(&new_git, &live_git) {
        // Put the original back rather than leaving the wiki without a repo.
        let _ = std::fs::rename(retired, &live_git);
        let _ = std::fs::remove_dir_all(scratch);
        let _ = remove_journal(root);
        return Err(io_err("installing rebuilt .git", e));
    }

    // Physically remove the old object database BEFORE reporting success: an
    // old pack left on disk is exactly the residue this whole module exists to
    // prevent.
    std::fs::remove_dir_all(retired).map_err(|e| io_err("removing retired .git", e))?;
    let _ = std::fs::remove_dir_all(scratch);
    remove_journal(root)?;
    Ok(())
}

/// Finish or roll back a purge interrupted mid-swap.
///
/// Called at startup. Returns `Ok(true)` when a journal was found and resolved.
/// Reads and ingest must stay blocked until this returns, or a reader could
/// observe the pre-purge history that the journal exists to finish removing.
///
/// # Errors
/// Returns [`WikiError`] when the journal exists but cannot be resolved, which
/// deliberately keeps the caller from opening the wiki for business.
pub fn recover_interrupted_purge(root: &Path) -> WikiResult<bool> {
    let journal_path = root.join(JOURNAL_NAME);
    let raw = match std::fs::read_to_string(&journal_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err("reading purge journal", e)),
    };
    let journal: PurgeJournal = serde_json::from_str(&raw)
        .map_err(|e| purge_err(format!("git purge: unreadable journal: {e}")))?;
    warn!(
        purge_id = %journal.purge_id,
        state = ?journal.state,
        "resuming an interrupted wiki history purge"
    );

    let live_git = root.join(".git");
    let scratch = root.join(format!("{SCRATCH_PREFIX}new-{}", journal.purge_id));
    let retired = root.join(format!("{SCRATCH_PREFIX}old-{}", journal.purge_id));
    let new_git = scratch.join(".git");

    match journal.state {
        // Nothing destructive happened yet: discard the replacement and let the
        // operator re-run the purge. Rolling forward would install a repository
        // that was never verified against THIS process's inputs.
        JournalState::Prepared => {
            let _ = std::fs::remove_dir_all(&scratch);
            let _ = std::fs::remove_dir_all(&retired);
        }
        JournalState::Swapping => {
            if !live_git.exists() && new_git.exists() {
                std::fs::rename(&new_git, &live_git)
                    .map_err(|e| io_err("installing rebuilt .git during recovery", e))?;
            } else if !live_git.exists() && retired.exists() {
                // The rebuilt repo is gone but the old one survives: restore it
                // so the wiki has a repository, and report the purge unfinished.
                std::fs::rename(&retired, &live_git)
                    .map_err(|e| io_err("restoring old .git during recovery", e))?;
                remove_journal(root)?;
                return Err(purge_err(
                    "git purge: interrupted before the rebuilt repository was installed; \
                     the purge must be re-run",
                ));
            }
            if retired.exists() {
                std::fs::remove_dir_all(&retired)
                    .map_err(|e| io_err("removing retired .git during recovery", e))?;
            }
            let _ = std::fs::remove_dir_all(&scratch);
        }
    }
    remove_journal(root)?;
    Ok(true)
}

fn write_journal(root: &Path, journal: &PurgeJournal) -> WikiResult<()> {
    let body = serde_json::to_vec_pretty(journal)
        .map_err(|e| purge_err(format!("git purge: encoding journal: {e}")))?;
    let path = root.join(JOURNAL_NAME);
    let tmp = root.join(format!("{JOURNAL_NAME}.tmp"));
    std::fs::write(&tmp, &body).map_err(|e| io_err("writing purge journal", e))?;
    // fsync the journal itself, then rename: a journal that is not durable is
    // not a journal.
    let file = std::fs::File::open(&tmp).map_err(|e| io_err("opening purge journal", e))?;
    file.sync_all()
        .map_err(|e| io_err("syncing purge journal", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io_err("installing purge journal", e))?;
    Ok(())
}

fn remove_journal(root: &Path) -> WikiResult<()> {
    match std::fs::remove_file(root.join(JOURNAL_NAME)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("removing purge journal", e)),
    }
}

fn count_objects(repo: &Repository) -> WikiResult<usize> {
    let odb = repo.odb().map_err(git_err)?;
    let mut count = 0usize;
    // `foreach` visits EVERY object — loose and packed, reachable or not. That
    // breadth is the point: a reachability-based count would report success
    // while dangling copies of the purged content sat in the same database.
    odb.foreach(|_| {
        count += 1;
        true
    })
    .map_err(git_err)?;
    Ok(count)
}

/// Blob oids that are reachable ONLY through the doomed paths.
///
/// Content-identical blobs shared with a surviving path are deliberately
/// excluded: git stores one object for identical content, so destroying it
/// would corrupt the page that legitimately keeps it.
fn blobs_reachable_only_through(
    repo: &Repository,
    doomed: &HashSet<&str>,
) -> WikiResult<HashSet<Oid>> {
    let mut through_doomed = HashSet::new();
    let mut through_survivors = HashSet::new();

    let mut walk = repo.revwalk().map_err(git_err)?;
    walk.push_glob("refs/*").map_err(git_err)?;
    if repo.head().is_ok() {
        let _ = walk.push_head();
    }
    for oid in walk {
        let oid = oid.map_err(git_err)?;
        let commit = repo.find_commit(oid).map_err(git_err)?;
        let tree = commit.tree().map_err(git_err)?;
        tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return TreeWalkResult::Ok;
            }
            let Ok(name) = entry.name() else {
                return TreeWalkResult::Ok;
            };
            let full = format!("{dir}{name}");
            if doomed.contains(full.as_str()) {
                through_doomed.insert(entry.id());
            } else {
                through_survivors.insert(entry.id());
            }
            TreeWalkResult::Ok
        })
        .map_err(git_err)?;
    }
    Ok(through_doomed
        .difference(&through_survivors)
        .copied()
        .collect())
}

/// Build a replacement repository containing the same history minus `doomed`.
fn build_replacement(
    repo: &Repository,
    scratch: &Path,
    doomed: &HashSet<&str>,
    spec: &HistoryPurgeSpec,
    objects_before: usize,
    doomed_blobs: &HashSet<Oid>,
) -> WikiResult<GitPurgeReport> {
    std::fs::create_dir_all(scratch).map_err(|e| io_err("creating scratch directory", e))?;
    let new_repo = Repository::init(scratch).map_err(git_err)?;

    let mut walk = repo.revwalk().map_err(git_err)?;
    walk.push_glob("refs/*").map_err(git_err)?;
    if repo.head().is_ok() {
        let _ = walk.push_head();
    }
    // Oldest first: a commit can only be rebuilt once its parents have new ids.
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .map_err(git_err)?;

    let scrub: HashSet<&str> = spec.scrub_paths.iter().map(String::as_str).collect();
    let mut remapped: HashMap<Oid, Oid> = HashMap::new();
    let mut commits_rewritten = 0usize;
    let mut blobs_scrubbed = 0usize;
    let mut messages_redacted = 0usize;
    let mut last_new = None;

    for oid in walk {
        let oid = oid.map_err(git_err)?;
        let commit = repo.find_commit(oid).map_err(git_err)?;
        let tree = commit.tree().map_err(git_err)?;
        let new_tree_oid = copy_tree_without(
            repo,
            &new_repo,
            &tree,
            "",
            doomed,
            &scrub,
            &spec.line_marker,
            &mut blobs_scrubbed,
        )?;
        let new_tree = new_repo.find_tree(new_tree_oid).map_err(git_err)?;

        let parents: Vec<_> = commit
            .parent_ids()
            .filter_map(|p| remapped.get(&p).copied())
            .collect();
        let parent_commits: Vec<_> = parents
            .iter()
            .map(|p| new_repo.find_commit(*p))
            .collect::<Result<_, _>>()
            .map_err(git_err)?;
        let parent_refs: Vec<&git2::Commit<'_>> = parent_commits.iter().collect();

        // A commit message embeds the session's own prompt text, so a rebuilt
        // commit would carry the purged content in plain sight. Matched by the
        // session's short-id handle rather than by searching the text.
        let original = commit.message().unwrap_or("");
        let message = if !spec.commit_prefix.is_empty()
            && original
                .trim_start()
                .starts_with(spec.commit_prefix.as_str())
        {
            messages_redacted += 1;
            format!("{} (purged)", spec.commit_prefix)
        } else {
            original.to_string()
        };
        let new_oid = new_repo
            .commit(
                None,
                &commit.author(),
                &commit.committer(),
                &message,
                &new_tree,
                &parent_refs,
            )
            .map_err(git_err)?;
        remapped.insert(oid, new_oid);
        last_new = Some(new_oid);
        commits_rewritten += 1;
    }

    // Point the rebuilt repo's HEAD branch at the rewritten tip. Reflogs are
    // not carried over: a fresh repository starts with none, which is precisely
    // the property that makes the old versions unrecoverable.
    if let Some(tip) = last_new {
        let branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().ok().map(str::to_owned))
            .unwrap_or_else(|| "main".to_string());
        new_repo
            .reference(
                &format!("refs/heads/{branch}"),
                tip,
                true,
                "ai-memory history purge",
            )
            .map_err(git_err)?;
        new_repo
            .set_head(&format!("refs/heads/{branch}"))
            .map_err(git_err)?;
    }

    let objects_after = count_objects(&new_repo)?;
    debug!(
        commits_rewritten,
        objects_before,
        objects_after,
        doomed_blobs = doomed_blobs.len(),
        "replacement repository built"
    );
    Ok(GitPurgeReport {
        commits_rewritten,
        objects_before,
        objects_after,
        blobs_destroyed: 0,
        repositories_rebuilt: 0,
        blobs_scrubbed,
        messages_redacted,
    })
}

/// Copy `tree` into `dest`, omitting any entry whose full path is doomed.
#[allow(clippy::too_many_arguments)]
fn copy_tree_without(
    src: &Repository,
    dest: &Repository,
    tree: &git2::Tree<'_>,
    prefix: &str,
    doomed: &HashSet<&str>,
    scrub: &HashSet<&str>,
    line_marker: &str,
    scrubbed: &mut usize,
) -> WikiResult<Oid> {
    let mut builder = dest.treebuilder(None).map_err(git_err)?;
    for entry in tree.iter() {
        let Ok(name) = entry.name() else { continue };
        let full = format!("{prefix}{name}");
        match entry.kind() {
            Some(git2::ObjectType::Tree) => {
                let subtree = src.find_tree(entry.id()).map_err(git_err)?;
                let sub_prefix = format!("{full}/");
                let new_sub = copy_tree_without(
                    src,
                    dest,
                    &subtree,
                    &sub_prefix,
                    doomed,
                    scrub,
                    line_marker,
                    scrubbed,
                )?;
                // Drop directories that became empty, so a purge cannot leave
                // an empty shell where a session's pages used to live.
                let new_tree = dest.find_tree(new_sub).map_err(git_err)?;
                if new_tree.iter().count() > 0 {
                    builder
                        .insert(name, new_sub, entry.filemode())
                        .map_err(git_err)?;
                }
            }
            Some(git2::ObjectType::Blob) => {
                if doomed.contains(full.as_str()) {
                    continue;
                }
                let blob = src.find_blob(entry.id()).map_err(git_err)?;
                // A shared file (the monthly log) belongs to every session that
                // wrote to it, so the path must survive while THIS session's
                // marked lines are removed from every historical version.
                let content = if scrub.contains(full.as_str()) && !line_marker.is_empty() {
                    let text = String::from_utf8_lossy(blob.content());
                    if text.contains(line_marker) {
                        *scrubbed += 1;
                        let kept: String = text
                            .lines()
                            .filter(|line| !line.contains(line_marker))
                            .map(|line| format!("{line}\n"))
                            .collect();
                        kept.into_bytes()
                    } else {
                        blob.content().to_vec()
                    }
                } else {
                    blob.content().to_vec()
                };
                let new_id = dest.blob(&content).map_err(git_err)?;
                builder
                    .insert(name, new_id, entry.filemode())
                    .map_err(git_err)?;
            }
            // Submodules and anything else are carried by id; the wiki never
            // creates them, so this is a pass-through rather than a policy.
            _ => {
                builder
                    .insert(name, entry.id(), entry.filemode())
                    .map_err(git_err)?;
            }
        }
    }
    builder.write().map_err(git_err)
}

/// Fail closed unless the rebuilt repository provably lacks the purged content.
///
/// Two independent checks, because either alone can pass while the deletion is
/// incomplete: a path check would miss a blob still present under a different
/// name, and an object check would miss a path re-added by a later commit.
fn verify_replacement(
    scratch: &Path,
    doomed: &HashSet<&str>,
    spec: &HistoryPurgeSpec,
    doomed_blobs: &HashSet<Oid>,
) -> WikiResult<()> {
    let repo = Repository::open(scratch).map_err(git_err)?;

    // 1. No commit in the rebuilt history may still contain a purged path.
    let mut walk = repo.revwalk().map_err(git_err)?;
    walk.push_glob("refs/*").map_err(git_err)?;
    if repo.head().is_ok() {
        let _ = walk.push_head();
    }
    for oid in walk {
        let oid = oid.map_err(git_err)?;
        let commit = repo.find_commit(oid).map_err(git_err)?;
        let tree = commit.tree().map_err(git_err)?;
        let mut offending = None;
        tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            let Ok(name) = entry.name() else {
                return TreeWalkResult::Ok;
            };
            let full = format!("{dir}{name}");
            if doomed.contains(full.as_str()) {
                offending = Some(full);
                return TreeWalkResult::Abort;
            }
            TreeWalkResult::Ok
        })
        .map_err(git_err)?;
        if let Some(path) = offending {
            return Err(purge_err(format!(
                "git purge: rebuilt history still contains {path} at commit {oid}"
            )));
        }
    }

    // 2. No object that existed only through a purged path may survive —
    //    including as a dangling object no ref points at.
    let odb = repo.odb().map_err(git_err)?;
    let mut survivor = None;
    odb.foreach(|oid| {
        if doomed_blobs.contains(oid) {
            survivor = Some(*oid);
            return false;
        }
        true
    })
    .map_err(git_err)?;
    if let Some(oid) = survivor {
        return Err(purge_err(format!(
            "git purge: rebuilt object database still contains purged blob {oid}"
        )));
    }

    // 3. No object anywhere may still carry the session's line marker, and no
    //    commit message may still be attributed to it. Checked across the whole
    //    database rather than only reachable objects, for the same reason as
    //    above: an unreachable copy is still a copy.
    if !spec.line_marker.is_empty() || !spec.commit_prefix.is_empty() {
        let mut offender = None;
        odb.foreach(|oid| {
            let Ok(obj) = odb.read(*oid) else {
                return true;
            };
            let carries_marker = !spec.line_marker.is_empty()
                && matches!(obj.kind(), git2::ObjectType::Blob)
                && String::from_utf8_lossy(obj.data()).contains(spec.line_marker.as_str());
            let carries_attribution = !spec.commit_prefix.is_empty()
                && matches!(obj.kind(), git2::ObjectType::Commit)
                && String::from_utf8_lossy(obj.data())
                    .contains(&format!("{} ", spec.commit_prefix))
                && !String::from_utf8_lossy(obj.data())
                    .contains(&format!("{} (purged)", spec.commit_prefix));
            if carries_marker || carries_attribution {
                offender = Some(*oid);
                return false;
            }
            true
        })
        .map_err(git_err)?;
        if let Some(oid) = offender {
            return Err(purge_err(format!(
                "git purge: rebuilt object database still attributes content to the purged session ({oid})"
            )));
        }
    }
    Ok(())
}

/// Paths of any purge scratch directory left inside `root`.
///
/// The contract is that none remain after a successful purge, so this exists to
/// let callers and tests assert it rather than assume it.
#[must_use]
pub fn leftover_scratch_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(SCRATCH_PREFIX))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};

    /// Canaries are unique strings, so finding one anywhere in the object
    /// database is unambiguous evidence the deletion failed.
    const DOOMED_CANARY: &str = "doomed-canary-4b7e91";
    const KEPT_CANARY: &str = "kept-canary-2f8c03";

    fn doomed_spec() -> HistoryPurgeSpec {
        HistoryPurgeSpec {
            doomed_paths: vec!["sessions/doomed.md".to_string()],
            ..Default::default()
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    fn commit(repo: &Repository, root: &Path, files: &[(&str, &str)], message: &str) {
        for (path, body) in files {
            let abs = root.join(path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&abs, body).unwrap();
        }
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("t", "t@example.com").unwrap();
        let parents: Vec<git2::Commit<'_>> = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .unwrap();
    }

    /// A repo whose history contains the doomed page in several versions, plus
    /// an unrelated page that must survive untouched.
    fn fixture() -> Fixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repository::init(&root).unwrap();
        commit(
            &repo,
            &root,
            &[
                ("sessions/doomed.md", &format!("v1 {DOOMED_CANARY}")),
                ("notes/keep.md", &format!("v1 {KEPT_CANARY}")),
            ],
            "first",
        );
        commit(
            &repo,
            &root,
            &[("sessions/doomed.md", &format!("v2 {DOOMED_CANARY} more"))],
            "second",
        );
        commit(
            &repo,
            &root,
            &[("notes/keep.md", &format!("v2 {KEPT_CANARY} more"))],
            "third",
        );
        Fixture { _tmp: tmp, root }
    }

    /// Search EVERY object in the database, reachable or not, for a canary.
    /// This is the check that distinguishes real deletion from unreferencing:
    /// a reachability-based search passes while dangling copies survive.
    fn canary_in_any_object(root: &Path, canary: &str) -> bool {
        let repo = Repository::open(root).unwrap();
        let odb = repo.odb().unwrap();
        let mut found = false;
        odb.foreach(|oid| {
            if let Ok(obj) = odb.read(*oid)
                && obj.kind() == git2::ObjectType::Blob
                && String::from_utf8_lossy(obj.data()).contains(canary)
            {
                found = true;
                return false;
            }
            true
        })
        .unwrap();
        found
    }

    #[test]
    fn purge_destroys_every_historical_version_of_the_path() {
        let fx = fixture();
        assert!(canary_in_any_object(&fx.root, DOOMED_CANARY));

        let report = purge_paths_from_history(&fx.root, &doomed_spec()).unwrap();

        assert_eq!(report.repositories_rebuilt, 1);
        assert_eq!(
            report.commits_rewritten, 3,
            "history is preserved, not squashed"
        );
        assert_eq!(
            report.blobs_destroyed, 2,
            "both versions of the doomed page must be destroyed"
        );
        assert!(
            !canary_in_any_object(&fx.root, DOOMED_CANARY),
            "the doomed canary must not survive in ANY object, reachable or not"
        );
        assert!(
            canary_in_any_object(&fx.root, KEPT_CANARY),
            "the unrelated page must survive"
        );
    }

    #[test]
    fn purge_leaves_no_scratch_or_retired_directory_behind() {
        let fx = fixture();
        purge_paths_from_history(&fx.root, &doomed_spec()).unwrap();
        assert_eq!(
            leftover_scratch_dirs(&fx.root),
            Vec::<PathBuf>::new(),
            "no quarantine copy, .bak, or swap directory may remain"
        );
        assert!(
            !fx.root.join(JOURNAL_NAME).exists(),
            "the journal must be removed once the purge is complete"
        );
    }

    #[test]
    fn purge_preserves_history_and_the_surviving_pages_content() {
        let fx = fixture();
        purge_paths_from_history(&fx.root, &doomed_spec()).unwrap();

        let repo = Repository::open(&fx.root).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_head().unwrap();
        let commits: Vec<_> = walk.map(|o| o.unwrap()).collect();
        assert_eq!(commits.len(), 3, "commit history survives the rewrite");

        // The surviving page keeps BOTH of its versions: a purge must not cost
        // unrelated pages their history.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let tree = head.tree().unwrap();
        let entry = tree.get_path(Path::new("notes/keep.md")).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert!(String::from_utf8_lossy(blob.content()).contains("v2"));
        assert!(tree.get_path(Path::new("sessions/doomed.md")).is_err());
    }

    /// A blob shared with a surviving path must NOT be destroyed: git stores
    /// one object for identical content, so removing it would corrupt the page
    /// that legitimately keeps it.
    #[test]
    fn purge_spares_a_blob_shared_with_a_surviving_page() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repository::init(&root).unwrap();
        let shared = "identical body shared by two pages";
        commit(
            &repo,
            &root,
            &[("sessions/doomed.md", shared), ("notes/keep.md", shared)],
            "shared content",
        );

        let report = purge_paths_from_history(&root, &doomed_spec()).unwrap();
        assert_eq!(
            report.blobs_destroyed, 0,
            "a blob the surviving page also uses must be spared"
        );
        let repo = Repository::open(&root).unwrap();
        let tree = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_path(Path::new("sessions/doomed.md")).is_err());
        let kept = tree.get_path(Path::new("notes/keep.md")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(repo.find_blob(kept.id()).unwrap().content()),
            shared,
            "the surviving page must still resolve its content"
        );
    }

    /// Directories emptied by the purge must not survive as shells.
    #[test]
    fn purge_drops_directories_it_empties() {
        let fx = fixture();
        purge_paths_from_history(&fx.root, &doomed_spec()).unwrap();
        let repo = Repository::open(&fx.root).unwrap();
        let tree = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(
            tree.get_path(Path::new("sessions")).is_err(),
            "the emptied directory must be gone, not left as an empty tree"
        );
    }

    /// Regression for two residues that fixtures with generic commit messages
    /// and no shared files could never surface — a live end-to-end run did.
    ///
    /// The monthly log belongs to EVERY session that wrote that month, so
    /// removing its path would destroy the other sessions' history. Its
    /// historical versions must instead be rewritten without the purged
    /// session's marked lines. And a commit message embeds the session's own
    /// prompt text, so the rebuilt commit carries the purged content in plain
    /// sight unless it is redacted.
    #[test]
    fn purge_scrubs_shared_log_history_and_redacts_commit_messages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repository::init(&root).unwrap();

        let marker_a = "<!-- ai-memory:session=11111111 -->";
        let marker_b = "<!-- ai-memory:session=22222222 -->";
        // Version 1 of the log holds A's line...
        commit(
            &repo,
            &root,
            &[(
                "log-2026-08.md",
                &format!("## [t1] user-prompt | {DOOMED_CANARY} {marker_a}\n"),
            )],
            &format!("session 11111111: {DOOMED_CANARY}"),
        );
        // ...and version 2 adds B's, so the file is genuinely shared.
        commit(
            &repo,
            &root,
            &[(
                "log-2026-08.md",
                &format!(
                    "## [t1] user-prompt | {DOOMED_CANARY} {marker_a}\n\
                     ## [t2] user-prompt | {KEPT_CANARY} {marker_b}\n"
                ),
            )],
            &format!("session 22222222: {KEPT_CANARY}"),
        );

        let spec = HistoryPurgeSpec {
            doomed_paths: Vec::new(),
            scrub_paths: vec!["log-2026-08.md".to_string()],
            line_marker: marker_a.to_string(),
            commit_prefix: "session 11111111".to_string(),
        };
        let report = purge_paths_from_history(&root, &spec).unwrap();

        assert!(
            report.blobs_scrubbed >= 2,
            "both log versions must be rewritten"
        );
        assert_eq!(
            report.messages_redacted, 1,
            "A's commit message must be redacted"
        );
        assert!(
            !canary_in_any_object(&root, DOOMED_CANARY),
            "A's text must not survive in ANY object — log blob or commit message"
        );
        assert!(
            canary_in_any_object(&root, KEPT_CANARY),
            "B's log line and commit message must survive"
        );

        // The shared file itself must still exist, with B's line intact.
        let repo = Repository::open(&root).unwrap();
        let tree = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        let entry = tree.get_path(Path::new("log-2026-08.md")).unwrap();
        let body = repo.find_blob(entry.id()).unwrap();
        let body = String::from_utf8_lossy(body.content());
        assert!(
            body.contains(KEPT_CANARY),
            "the shared log must keep B: {body}"
        );
        assert!(!body.contains(marker_a), "and lose A: {body}");
    }

    #[test]
    fn purging_nothing_is_a_no_op() {
        let fx = fixture();
        let report = purge_paths_from_history(&fx.root, &HistoryPurgeSpec::default()).unwrap();
        assert_eq!(report, GitPurgeReport::default());
        assert!(canary_in_any_object(&fx.root, DOOMED_CANARY));
    }

    /// A crash after the old `.git` was moved aside must converge on the
    /// purged state, not leave the wiki without a repository.
    #[test]
    fn recovery_finishes_a_swap_interrupted_after_the_old_repo_moved() {
        let fx = fixture();
        let repo = Repository::open(&fx.root).unwrap();
        let doomed: HashSet<&str> = ["sessions/doomed.md"].into_iter().collect();
        let objects_before = count_objects(&repo).unwrap();
        let doomed_blobs = blobs_reachable_only_through(&repo, &doomed).unwrap();
        let purge_id = "deadbeefdeadbeef".to_string();
        let scratch = fx.root.join(format!("{SCRATCH_PREFIX}new-{purge_id}"));
        let retired = fx.root.join(format!("{SCRATCH_PREFIX}old-{purge_id}"));
        build_replacement(
            &repo,
            &scratch,
            &doomed,
            &doomed_spec(),
            objects_before,
            &doomed_blobs,
        )
        .unwrap();
        drop(repo);

        // Simulate the crash: journal says Swapping, old .git moved aside,
        // replacement not yet installed.
        write_journal(
            &fx.root,
            &PurgeJournal {
                purge_id,
                paths: vec!["sessions/doomed.md".to_string()],
                state: JournalState::Swapping,
            },
        )
        .unwrap();
        std::fs::rename(fx.root.join(".git"), &retired).unwrap();

        assert!(recover_interrupted_purge(&fx.root).unwrap());

        assert!(
            fx.root.join(".git").exists(),
            "the wiki must have a repo again"
        );
        assert!(
            !canary_in_any_object(&fx.root, DOOMED_CANARY),
            "recovery must converge on the PURGED state, not restore the old history"
        );
        assert_eq!(leftover_scratch_dirs(&fx.root), Vec::<PathBuf>::new());
        assert!(!fx.root.join(JOURNAL_NAME).exists());
    }

    /// A crash before anything destructive happened discards the unverified
    /// replacement and leaves the live repository intact.
    #[test]
    fn recovery_rolls_back_a_purge_interrupted_before_the_swap() {
        let fx = fixture();
        let scratch = fx.root.join(format!("{SCRATCH_PREFIX}new-abc123"));
        std::fs::create_dir_all(&scratch).unwrap();
        write_journal(
            &fx.root,
            &PurgeJournal {
                purge_id: "abc123".to_string(),
                paths: vec!["sessions/doomed.md".to_string()],
                state: JournalState::Prepared,
            },
        )
        .unwrap();

        assert!(recover_interrupted_purge(&fx.root).unwrap());
        assert_eq!(leftover_scratch_dirs(&fx.root), Vec::<PathBuf>::new());
        assert!(!fx.root.join(JOURNAL_NAME).exists());
        assert!(
            canary_in_any_object(&fx.root, DOOMED_CANARY),
            "an unverified replacement must not be installed by recovery"
        );
    }

    #[test]
    fn recovery_is_a_no_op_without_a_journal() {
        let fx = fixture();
        assert!(!recover_interrupted_purge(&fx.root).unwrap());
    }
}

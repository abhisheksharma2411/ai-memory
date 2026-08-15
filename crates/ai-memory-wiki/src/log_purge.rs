//! Removal of one session's monthly-log entries (#387).
//!
//! Lives here rather than beside the appender because the monthly log is a file
//! in the wiki tree, and both the hook ingress (which writes entries) and the
//! admin route (which purges them) depend on this crate — putting it here keeps
//! the marker format and the removal that keys on it in one place, and avoids
//! making the transport crate depend on the ingress crate.

use ai_memory_core::{ProjectId, SessionId, WorkspaceId};

use crate::Wiki;

/// Marker that attributes a log line to its session (#387).
///
/// An HTML comment so it is invisible in every markdown renderer while staying
/// exact: a purge removes the lines whose marker matches its session id, and
/// never guesses from the human-readable text. Lines written before this
/// existed carry no marker and are deliberately NOT removable by inference —
/// see [`purge_session_lines`].
const SESSION_MARKER_PREFIX: &str = "<!-- ai-memory:session=";

/// The exact marker string a log entry for `session_id` carries.
#[must_use]
pub fn session_marker(session_id: SessionId) -> String {
    format!("{SESSION_MARKER_PREFIX}{session_id} -->")
}

/// Outcome of removing one session's entries from the monthly logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogPurgeOutcome {
    /// Attributed lines removed because they belong to the purged session.
    pub removed: usize,
    /// Monthly log files that still hold entries written before per-entry
    /// provenance existed. Those lines cannot be attributed to any session, so
    /// they are neither removed nor claimed to be clean.
    ///
    /// Text search is explicitly not a fallback: matching a log line because it
    /// mentions something the session also mentioned would delete other
    /// sessions' entries and still miss this one's.
    pub unattributed_files: Vec<String>,
}

/// Remove every log entry attributed to `session_id` from a project's monthly
/// logs (#387).
///
/// With `force`, a file that still contains unattributed legacy entries is
/// removed whole — the monthly log is the retention unit, so this is the only
/// way to guarantee the session left nothing behind, at the cost of other
/// sessions' entries for that month. Without it, such files are reported in
/// [`LogPurgeOutcome::unattributed_files`] and left intact.
///
/// # Errors
/// Returns any filesystem error other than a missing project directory.
pub fn purge_session_lines(
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    force: bool,
) -> std::io::Result<LogPurgeOutcome> {
    let root = wiki.project_root(workspace_id, project_id);
    let mut outcome = LogPurgeOutcome::default();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(e) => return Err(e),
    };
    let marker = session_marker(session_id);
    for entry in entries.flatten() {
        let path = entry.path();
        let is_log = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("log-") && n.ends_with(".md"));
        if !is_log {
            continue;
        }
        let body = std::fs::read_to_string(&path)?;
        let mut kept = String::with_capacity(body.len());
        let mut removed = 0usize;
        let mut unattributed = false;
        for line in body.lines() {
            if line.contains(&marker) {
                removed += 1;
                continue;
            }
            // Only entry headers carry provenance; continuation lines belong to
            // whichever entry precedes them.
            if line.starts_with("## [") && !line.contains(SESSION_MARKER_PREFIX) {
                unattributed = true;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        if unattributed {
            if force {
                std::fs::remove_file(&path)?;
                outcome.removed += removed;
                continue;
            }
            outcome.unattributed_files.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if removed > 0 {
            std::fs::write(&path, kept)?;
            outcome.removed += removed;
        }
    }
    outcome.unattributed_files.sort_unstable();
    Ok(outcome)
}

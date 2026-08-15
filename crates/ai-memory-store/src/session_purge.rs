//! Strong per-session deletion (#387): the relational half.
//!
//! This module removes a session and everything derived from it inside one
//! transaction, and records a content-free tombstone. It does NOT touch the
//! filesystem — the wiki files, sidecars, spool entries, and git objects are
//! the caller's responsibility, driven by the paths this returns.
//!
//! Two rules shape the whole implementation:
//!
//! 1. **Collect before cutting.** Every link that identifies derived content
//!    (`auto_improve_runs.session_id`, `auto_improve_rejections.source_run_id`,
//!    `pages.supersedes`) is `ON DELETE SET NULL`, so deleting the session
//!    first destroys the very evidence needed to find what it produced. All
//!    targets are resolved up front, into owned id lists.
//! 2. **Identity only, never text.** Targets are chosen by UUID, foreign key,
//!    or the deterministic `sessions/<uuid>.md` path. Searching page bodies for
//!    something that "looks like" the session would both miss content and
//!    delete innocent pages.

use ai_memory_core::{PagePath, ProjectId, SessionId, WorkspaceId};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use tracing::warn;

use crate::error::{StoreError, StoreResult};

/// Contract version stamped on every tombstone this code writes.
///
/// A restore reapplies tombstones written by older builds, so the reader must
/// be able to tell which guarantees a given row was produced under.
pub const SESSION_PURGE_SCHEMA_VERSION: i64 = 1;

/// Whether the purge did the work or found it already done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPurgeStatus {
    /// The session existed and was removed by this call.
    Purged,
    /// A tombstone already covered this session; nothing was left to remove.
    AlreadyPurged,
}

impl SessionPurgeStatus {
    /// Stable wire representation for the CLI/HTTP receipt.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Purged => "purged",
            Self::AlreadyPurged => "already_purged",
        }
    }
}

/// Rows removed, per layer. Counts are for the operator's receipt and the
/// audit trail; they never carry deleted content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionPurgeCounts {
    /// `sessions` rows (0 or 1).
    pub sessions: u64,
    /// `observations` rows; their FTS rows go with them via `observations_fts_ad`.
    pub observations: u64,
    /// Handoffs produced by, or accepted by, this session.
    pub handoffs: u64,
    /// Queued SessionEnd consolidation jobs.
    pub consolidation_jobs: u64,
    /// Auto-improvement scheduler claims.
    pub auto_improve_scheduler_claims: u64,
    /// Auto-improvement runs derived from this session.
    pub auto_improve_runs: u64,
    /// Proposals staged by those runs.
    pub auto_improve_proposals: u64,
    /// Lifecycle events of those proposals.
    pub auto_improve_proposal_events: u64,
    /// Rejections whose reason/summary/evidence came from this session.
    pub auto_improve_rejections: u64,
    /// Page versions removed across every affected path.
    pub page_versions: u64,
}

/// What a purge removed, plus what the filesystem layers still must remove.
#[derive(Clone, Debug)]
pub struct SessionPurgeReceipt {
    /// Whether this call did the work.
    pub status: SessionPurgeStatus,
    /// Stable id of the tombstone covering this session.
    pub tombstone_id: Uuid,
    /// When the session was purged (the first purge, on a repeat call).
    pub purged_at: Timestamp,
    /// Snapshots taken before this instant may still contain purged content.
    pub backup_cutoff: Timestamp,
    /// Rows removed per layer.
    pub counts: SessionPurgeCounts,
    /// Wiki pages whose every version was removed. The caller deletes these
    /// files and rewrites git history; the index is already clean.
    pub removed_pages: Vec<PagePath>,
    /// Proposal ids whose `_pending/auto-improve/<id>.md` sidecars must be
    /// removed from disk. Sidecars are not indexed, so nothing else names them.
    pub removed_proposal_sidecars: Vec<Uuid>,
    /// Non-fatal conditions the operator should see, e.g. shared pages that
    /// could not be rebuilt from surviving sources.
    pub warnings: Vec<String>,
}

/// A tombstone as stored. Content-free by contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTombstone {
    /// The purged session.
    pub session_id: SessionId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Stable receipt id.
    pub tombstone_id: Uuid,
    /// Contract version of the purge that wrote this row.
    pub schema_version: i64,
    /// When the purge completed.
    pub purged_at: Timestamp,
}

/// The deterministic wiki path of a session's heuristic page.
///
/// This is an identity, not a search: the synthesizer writes exactly this path,
/// so a purge can claim every version of it without inspecting any page body.
/// LLM single-page consolidation rewrites the same path, and its
/// `build_frontmatter` omits `session_id`, so the relational
/// `pages.source_session_id` link alone would miss those later versions.
fn session_page_path(session_id: SessionId) -> String {
    format!("sessions/{session_id}.md")
}

/// Read the tombstone covering `session_id`, if any.
pub fn find_session_tombstone(
    conn: &Connection,
    session_id: SessionId,
) -> StoreResult<Option<SessionTombstone>> {
    conn.query_row(
        "SELECT workspace_id, project_id, tombstone_id, schema_version, purged_at \
         FROM session_tombstones WHERE session_id = ?1",
        params![session_id.as_bytes()],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )
    .optional()?
    .map(|(ws, proj, tombstone, schema_version, purged_at)| {
        Ok(SessionTombstone {
            session_id,
            workspace_id: WorkspaceId::from_slice(&ws)?,
            project_id: ProjectId::from_slice(&proj)?,
            tombstone_id: uuid_from_slice(&tombstone)?,
            schema_version,
            purged_at: Timestamp::from_microsecond(purged_at)
                .map_err(|e| StoreError::InvalidState(format!("tombstone timestamp: {e}")))?,
        })
    })
    .transpose()
}

/// Whether ingest for this exact `(workspace, project, session)` must be
/// refused as terminal. Scope is part of the check so a tombstone can never
/// suppress a same-UUID session legitimately living in another project.
pub fn is_session_purged(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
) -> StoreResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM session_tombstones \
             WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            params![
                session_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn uuid_from_slice(raw: &[u8]) -> StoreResult<Uuid> {
    <[u8; 16]>::try_from(raw)
        .map(Uuid::from_bytes)
        .map_err(|_| StoreError::InvalidState("malformed uuid blob".into()))
}

/// SQLite reports affected rows as `usize`; receipts use `u64` so the JSON
/// shape does not vary by platform pointer width.
fn rows(affected: usize) -> StoreResult<u64> {
    u64::try_from(affected)
        .map_err(|_| StoreError::InvalidState("affected row count does not fit u64".into()))
}

/// Whether a live session, queued consolidation, or in-flight auto-improvement
/// run would be cut off by purging now. Callers surface this as `409` unless
/// the operator passed `--force`.
pub fn session_purge_is_busy(
    conn: &Connection,
    session_id: SessionId,
) -> StoreResult<Option<String>> {
    let open: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1 AND ended_at IS NULL",
            params![session_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if open.is_some() {
        return Ok(Some("session is still open".into()));
    }
    let running: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM session_consolidation_jobs \
             WHERE session_id = ?1 AND state IN ('pending', 'running')",
            params![session_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if running.is_some() {
        return Ok(Some("a consolidation job is queued or running".into()));
    }
    let claimed: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM auto_improve_scheduler_claims WHERE session_id = ?1",
            params![session_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if claimed.is_some() {
        return Ok(Some("an auto-improvement claim is outstanding".into()));
    }
    Ok(None)
}

/// Remove a session and everything derived from it.
///
/// `workspace_id` / `project_id` must be the session's own scope: a UUID that
/// exists in a different project is a scope conflict, not a target, so an
/// operator cannot purge someone else's session by naming the wrong project.
///
/// Idempotent. A second call for the same id returns
/// [`SessionPurgeStatus::AlreadyPurged`] with the original tombstone and zero
/// counts, and does not fail on the absence of the rows the first call removed.
pub fn purge_session(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
) -> StoreResult<SessionPurgeReceipt> {
    // Zero freed pages as the delete proceeds. SQLite normally leaves deleted
    // content in place and only marks the pages reusable, so without this the
    // purged bytes stay legible in the database file — a "deletion" anyone with
    // the file could read back. Restored below so ordinary writes keep their
    // default (cheaper) behavior.
    let previous_secure_delete: i32 = conn
        .query_row("PRAGMA secure_delete", [], |row| row.get(0))
        .unwrap_or(0);
    conn.execute_batch("PRAGMA secure_delete = ON;")?;

    let outcome = purge_session_inner(conn, workspace_id, project_id, session_id);

    // Restore the pragma before propagating any error, so one failed purge does
    // not silently change how the rest of the process deletes.
    let _ = conn.execute_batch(&format!("PRAGMA secure_delete = {previous_secure_delete};"));
    let receipt = outcome?;

    reclaim_purged_bytes(conn);
    Ok(receipt)
}

/// Make the deletion true of the FILES, not just of the tables.
///
/// Two residues survive an ordinary `DELETE`, and both were found by searching
/// the data directory for a purged canary rather than by querying for it:
///
/// 1. **The write-ahead log.** Until a checkpoint runs, the deleted rows are
///    still readable in `memory.sqlite-wal` — sitting beside the database and
///    ready to be copied into the next backup.
/// 2. **Freed pages in the database file.** `secure_delete` zeroes content as
///    a delete proceeds, but pages freed by earlier writes (a superseded page
///    version, say) keep their bytes until the file is rebuilt.
/// 3. **FTS5 index segments.** An FTS5 delete is logical — it writes a delete
///    marker and leaves the original terms in the existing segments. The index
///    must be rebuilt from its content table or the purged text stays readable
///    in the shadow tables.
///
/// Neither is fatal to correctness of the index, which is exactly why both are
/// easy to miss: every query says the content is gone while the bytes are still
/// on disk. Failures are logged rather than fatal — the rows ARE deleted, and
/// refusing the whole purge over an incomplete vacuum would leave the operator
/// worse off.
fn reclaim_purged_bytes(conn: &Connection) {
    // Rebuild the FTS indexes from their content tables. FTS5 deletes are
    // LOGICAL: removing a row writes a delete marker and leaves the original
    // terms inside the existing index segments until a merge happens to rewrite
    // them. `MATCH` correctly returns nothing, which is exactly why this is easy
    // to miss — the queries all say the content is gone while the purged text is
    // still legible in `pages_fts_data` / `observations_fts_data`.
    for table in ["pages_fts", "observations_fts"] {
        if let Err(e) =
            conn.execute_batch(&format!("INSERT INTO {table}({table}) VALUES('rebuild');"))
        {
            warn!(error = %e, table, "purge could not rebuild the FTS index; purged terms may remain in its segments");
        }
    }
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        warn!(error = %e, "purge could not truncate the WAL; purged bytes may remain in memory.sqlite-wal until the next checkpoint");
    }
    if let Err(e) = conn.execute_batch("VACUUM;") {
        warn!(error = %e, "purge could not vacuum the database; purged bytes may remain in freed pages of memory.sqlite");
    }
    // VACUUM writes the rebuilt database through the WAL, so checkpoint again
    // or the bytes it just reclaimed reappear in the log it wrote them through.
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        warn!(error = %e, "purge could not truncate the WAL after vacuum");
    }
}

fn purge_session_inner(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
) -> StoreResult<SessionPurgeReceipt> {
    // Immediate: the purge decides what to delete from rows it has read, so a
    // deferred transaction could upgrade mid-way and lose to a writer that
    // added derived rows in between.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    if let Some(existing) = find_session_tombstone(&tx, session_id)? {
        if existing.workspace_id != workspace_id || existing.project_id != project_id {
            return Err(StoreError::InvalidState(
                "session id belongs to a different workspace or project".into(),
            ));
        }
        return Ok(SessionPurgeReceipt {
            status: SessionPurgeStatus::AlreadyPurged,
            tombstone_id: existing.tombstone_id,
            purged_at: existing.purged_at,
            backup_cutoff: existing.purged_at,
            counts: SessionPurgeCounts::default(),
            removed_pages: Vec::new(),
            removed_proposal_sidecars: Vec::new(),
            warnings: Vec::new(),
        });
    }

    let scope: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT workspace_id, project_id FROM sessions WHERE id = ?1",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((session_ws, session_proj)) = scope else {
        return Err(StoreError::NotFound("session".into()));
    };
    if session_ws.as_slice() != workspace_id.as_bytes()
        || session_proj.as_slice() != project_id.as_bytes()
    {
        return Err(StoreError::InvalidState(
            "session id belongs to a different workspace or project".into(),
        ));
    }

    // ── collect ────────────────────────────────────────────────────────────
    // Everything below is read BEFORE any delete: the pointers used here are
    // `ON DELETE SET NULL` and would be erased by the first statement.

    let run_ids = collect_blob_ids(
        &tx,
        "SELECT id FROM auto_improve_runs \
         WHERE source_session_id = ?1 OR session_id = ?1",
        session_id,
    )?;
    let proposal_ids = collect_children(&tx, "auto_improve_proposals", "run_id", &run_ids)?;

    // Page versions derived from this session: the relational link, plus the
    // deterministic session-page path whose later LLM-rewritten versions carry
    // no frontmatter `session_id`.
    let page_paths = collect_page_paths(&tx, workspace_id, project_id, session_id, &proposal_ids)?;

    let sidecars = proposal_ids
        .iter()
        .map(|id| uuid_from_slice(id))
        .collect::<StoreResult<Vec<_>>>()?;

    // ── delete, children first ─────────────────────────────────────────────
    let proposal_events = delete_children(
        &tx,
        "auto_improve_proposal_events",
        "proposal_id",
        &proposal_ids,
    )?;
    let rejections = delete_rejections(&tx, session_id, &run_ids, &proposal_ids)?;
    let proposals = delete_children(&tx, "auto_improve_proposals", "id", &proposal_ids)?;
    let runs = delete_children(&tx, "auto_improve_runs", "id", &run_ids)?;

    let scheduler_claims = rows(tx.execute(
        "DELETE FROM auto_improve_scheduler_claims WHERE session_id = ?1",
        params![session_id.as_bytes()],
    )?)?;
    let consolidation_jobs = rows(tx.execute(
        "DELETE FROM session_consolidation_jobs WHERE session_id = ?1",
        params![session_id.as_bytes()],
    )?)?;
    // Handoffs link the session twice and both links are `SET NULL`, so the
    // rows must be deleted explicitly or the baton survives its session with a
    // dangling pointer and its body intact.
    let handoffs = rows(tx.execute(
        "DELETE FROM handoffs WHERE from_session_id = ?1 OR accepted_by_session = ?1",
        params![session_id.as_bytes()],
    )?)?;

    let mut page_versions = 0u64;
    for path in &page_paths {
        page_versions += rows(tx.execute(
            "DELETE FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
        )?)?;
    }
    if page_versions > 0 {
        // Entities are shared; drop only the ones nothing links to anymore.
        tx.execute(
            "DELETE FROM entities \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_page_links l WHERE l.entity_id = entities.id \
               )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
    }

    // Observations last among the derived rows: `observations_fts_ad` clears
    // the external-content index as they go.
    let observations = rows(tx.execute(
        "DELETE FROM observations WHERE session_id = ?1",
        params![session_id.as_bytes()],
    )?)?;
    let sessions = rows(tx.execute(
        "DELETE FROM sessions WHERE id = ?1",
        params![session_id.as_bytes()],
    )?)?;

    let counts = SessionPurgeCounts {
        sessions,
        observations,
        handoffs,
        consolidation_jobs,
        auto_improve_scheduler_claims: scheduler_claims,
        auto_improve_runs: runs,
        auto_improve_proposals: proposals,
        auto_improve_proposal_events: proposal_events,
        auto_improve_rejections: rejections,
        page_versions,
    };

    // ── tombstone ──────────────────────────────────────────────────────────
    // Truncate to what the column actually stores, so the first receipt and
    // every later read of this tombstone report the identical instant.
    let now = Timestamp::from_microsecond(Timestamp::now().as_microsecond())
        .map_err(|e| StoreError::InvalidState(format!("purge timestamp: {e}")))?;
    let tombstone_id = Uuid::new_v4();
    crate::ops::audit(
        &tx,
        "purge_session",
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        None,
        None,
        now.as_microsecond(),
    )?;
    // The receipt points at the audit row rather than repeating its detail,
    // which is what keeps the tombstone content-free.
    let audit_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO session_tombstones \
         (session_id, workspace_id, project_id, tombstone_id, schema_version, purged_at, audit_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            tombstone_id.as_bytes(),
            SESSION_PURGE_SCHEMA_VERSION,
            now.as_microsecond(),
            audit_id,
        ],
    )?;

    tx.commit()?;

    Ok(SessionPurgeReceipt {
        status: SessionPurgeStatus::Purged,
        tombstone_id,
        purged_at: now,
        backup_cutoff: now,
        counts,
        removed_pages: page_paths,
        removed_proposal_sidecars: sidecars,
        warnings: Vec::new(),
    })
}

/// Every tombstone in this database, newest first.
///
/// A restore carries this ledger across the overwrite: without it, restoring a
/// pre-purge snapshot silently resurrects everything a purge removed, and the
/// operator has no way to know.
///
/// # Errors
/// Returns [`StoreError`] on SQLite failure or a malformed stored row.
pub fn all_tombstones(conn: &Connection) -> StoreResult<Vec<SessionTombstone>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, workspace_id, project_id, tombstone_id, schema_version, purged_at \
         FROM session_tombstones ORDER BY purged_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (session, ws, proj, tombstone, schema_version, purged_at) = row?;
        out.push(SessionTombstone {
            session_id: SessionId::from_slice(&session)?,
            workspace_id: WorkspaceId::from_slice(&ws)?,
            project_id: ProjectId::from_slice(&proj)?,
            tombstone_id: uuid_from_slice(&tombstone)?,
            schema_version,
            purged_at: Timestamp::from_microsecond(purged_at)
                .map_err(|e| StoreError::InvalidState(format!("tombstone timestamp: {e}")))?,
        });
    }
    Ok(out)
}

/// Insert tombstones carried over from another database, keeping any already
/// present.
///
/// Used by restore: the ledger is imported BEFORE the restored state is made
/// available, so a session purged after the snapshot was taken is re-purged
/// rather than served.
///
/// Returns the number of rows newly inserted.
///
/// # Errors
/// Returns [`StoreError`] on SQLite failure.
pub fn import_tombstones(
    conn: &mut Connection,
    tombstones: &[SessionTombstone],
) -> StoreResult<usize> {
    let tx = conn.transaction()?;
    let mut inserted = 0usize;
    for tombstone in tombstones {
        // The scope rows may not exist in the restored database (the project
        // could have been purged too), so the ledger is inserted without
        // requiring them — a tombstone must never be droppable by a cascade.
        let affected = tx.execute(
            "INSERT OR IGNORE INTO session_tombstones \
             (session_id, workspace_id, project_id, tombstone_id, schema_version, purged_at, audit_id) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, NULL \
             WHERE EXISTS (SELECT 1 FROM workspaces WHERE id = ?2) \
               AND EXISTS (SELECT 1 FROM projects WHERE id = ?3)",
            params![
                tombstone.session_id.as_bytes(),
                tombstone.workspace_id.as_bytes(),
                tombstone.project_id.as_bytes(),
                tombstone.tombstone_id.as_bytes(),
                tombstone.schema_version,
                tombstone.purged_at.as_microsecond(),
            ],
        )?;
        inserted += affected;
    }
    tx.commit()?;
    Ok(inserted)
}

/// Session ids that a tombstone covers but whose rows are still present.
///
/// After a restore this is the work list: each one is a session the ledger says
/// was deleted and the restored snapshot brought back. An empty result is the
/// fail-closed check that the restore is safe to expose.
///
/// # Errors
/// Returns [`StoreError`] on SQLite failure or a malformed stored row.
pub fn resurrected_sessions(conn: &Connection) -> StoreResult<Vec<SessionTombstone>> {
    Ok(all_tombstones(conn)?
        .into_iter()
        .filter(|tombstone| {
            conn.query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![tombstone.session_id.as_bytes()],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
        })
        .collect())
}

/// Purge a session whose rows came back with a restored snapshot.
///
/// Distinct from [`purge_session`] because the tombstone already exists: the
/// ordinary path would short-circuit on it and report `AlreadyPurged` without
/// removing the resurrected rows.
///
/// # Errors
/// Returns [`StoreError`] on SQLite failure.
pub fn repurge_tombstoned_session(
    conn: &mut Connection,
    tombstone: &SessionTombstone,
) -> StoreResult<SessionPurgeReceipt> {
    // Drop the ledger row, run the ordinary purge, then restore the ORIGINAL
    // tombstone id and time: the receipt an operator already holds must keep
    // identifying this deletion.
    conn.execute(
        "DELETE FROM session_tombstones WHERE session_id = ?1",
        params![tombstone.session_id.as_bytes()],
    )?;
    let mut receipt = purge_session(
        conn,
        tombstone.workspace_id,
        tombstone.project_id,
        tombstone.session_id,
    )?;
    conn.execute(
        "UPDATE session_tombstones SET tombstone_id = ?2, purged_at = ?3 WHERE session_id = ?1",
        params![
            tombstone.session_id.as_bytes(),
            tombstone.tombstone_id.as_bytes(),
            tombstone.purged_at.as_microsecond(),
        ],
    )?;
    receipt.tombstone_id = tombstone.tombstone_id;
    receipt.purged_at = tombstone.purged_at;
    Ok(receipt)
}

/// Read the tombstone ledger from a database file directly.
///
/// Restore runs with the server stopped, so it uses a plain connection rather
/// than the writer actor: there is no concurrent writer to serialize against,
/// and the ledger must be read before the actor's database is overwritten.
///
/// A database that predates the ledger (no `session_tombstones` table) yields
/// an empty ledger rather than an error, so restoring an old backup still works.
///
/// # Errors
/// Returns [`StoreError`] when the file exists but cannot be read.
pub fn read_ledger_from_db(db_path: &std::path::Path) -> StoreResult<Vec<SessionTombstone>> {
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db_path)?;
    let has_table: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_tombstones'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if has_table.is_none() {
        return Ok(Vec::new());
    }
    all_tombstones(&conn)
}

/// One session that a restore brought back and this call removed again.
#[derive(Debug, Clone)]
pub struct RepurgedSession {
    /// The session the ledger said was deleted.
    pub session_id: SessionId,
    /// Wiki paths, relative to the wiki root, whose files and history the
    /// caller must still remove. Scoped `<workspace>/<project>/<page path>`
    /// because that is how they sit in the shared wiki tree.
    pub repo_paths: Vec<String>,
}

/// Import `ledger` into the restored database and re-purge anything it covers
/// that came back with the snapshot.
///
/// Returns what was re-purged, so the caller can reapply the filesystem and git
/// layers the snapshot also restored.
///
/// # Errors
/// Returns [`StoreError`] on SQLite failure. A non-empty
/// [`resurrected_sessions`] result after this returns means the restore is NOT
/// safe to expose and the caller must refuse.
pub fn reapply_ledger(
    db_path: &std::path::Path,
    ledger: &[SessionTombstone],
) -> StoreResult<Vec<RepurgedSession>> {
    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    import_tombstones(&mut conn, ledger)?;

    let mut repurged = Vec::new();
    for tombstone in resurrected_sessions(&conn)? {
        let receipt = repurge_tombstoned_session(&mut conn, &tombstone)?;
        repurged.push(RepurgedSession {
            session_id: tombstone.session_id,
            repo_paths: receipt
                .removed_pages
                .iter()
                .map(|page| {
                    format!(
                        "{}/{}/{}",
                        tombstone.workspace_id,
                        tombstone.project_id,
                        page.as_str()
                    )
                })
                .collect(),
        });
    }
    Ok(repurged)
}

/// How many purged sessions still have rows in this database.
///
/// The fail-closed check a restore runs last: anything other than zero means
/// the restored state serves content the ledger says was deleted.
///
/// # Errors
/// Returns [`StoreError`] on SQLite failure.
pub fn count_resurrected(db_path: &std::path::Path) -> StoreResult<usize> {
    let conn = Connection::open(db_path)?;
    Ok(resurrected_sessions(&conn)?.len())
}

fn collect_blob_ids(
    tx: &rusqlite::Transaction<'_>,
    sql: &str,
    session_id: SessionId,
) -> StoreResult<Vec<Vec<u8>>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map(params![session_id.as_bytes()], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn collect_children(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    parents: &[Vec<u8>],
) -> StoreResult<Vec<Vec<u8>>> {
    if parents.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    // One statement per parent rather than a built IN-list: the id count is
    // small and bounded, and this keeps every value parameterized.
    let mut stmt = tx.prepare(&format!("SELECT id FROM {table} WHERE {column} = ?1"))?;
    for parent in parents {
        let rows = stmt.query_map(params![parent], |row| row.get::<_, Vec<u8>>(0))?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

fn delete_children(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    ids: &[Vec<u8>],
) -> StoreResult<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut stmt = tx.prepare(&format!("DELETE FROM {table} WHERE {column} = ?1"))?;
    let mut removed = 0u64;
    for id in ids {
        removed += rows(stmt.execute(params![id])?)?;
    }
    Ok(removed)
}

/// Rejections reach the session three ways, and all three must be cut: the
/// immutable provenance column, the run they came from, and the proposal they
/// rejected. The latter two are `SET NULL`, so a rejection whose run is deleted
/// first would keep its session-derived `reason` / `summary` / `evidence_json`
/// with no pointer left to find it by.
fn delete_rejections(
    tx: &rusqlite::Transaction<'_>,
    session_id: SessionId,
    run_ids: &[Vec<u8>],
    proposal_ids: &[Vec<u8>],
) -> StoreResult<u64> {
    let mut removed = rows(tx.execute(
        "DELETE FROM auto_improve_rejections WHERE source_session_id = ?1",
        params![session_id.as_bytes()],
    )?)?;
    removed += delete_children(tx, "auto_improve_rejections", "source_run_id", run_ids)?;
    removed += delete_children(
        tx,
        "auto_improve_rejections",
        "source_proposal_id",
        proposal_ids,
    )?;
    Ok(removed)
}

/// Every wiki path whose content is derived from this session.
///
/// Three sources, all by identity: the relational provenance column, the
/// deterministic `sessions/<uuid>.md` path, and pages applied from this
/// session's auto-improvement proposals.
fn collect_page_paths(
    tx: &rusqlite::Transaction<'_>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    proposal_ids: &[Vec<u8>],
) -> StoreResult<Vec<PagePath>> {
    let mut paths: Vec<String> = Vec::new();

    let mut stmt = tx.prepare(
        "SELECT DISTINCT path FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 \
           AND (source_session_id = ?3 OR path = ?4)",
    )?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            session_id.as_bytes(),
            session_page_path(session_id),
        ],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        paths.push(row?);
    }

    if !proposal_ids.is_empty() {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT p.path FROM pages p \
             JOIN auto_improve_proposals ap ON ap.applied_page_id = p.id \
             WHERE ap.id = ?1",
        )?;
        for proposal in proposal_ids {
            let rows = stmt.query_map(params![proposal], |row| row.get::<_, String>(0))?;
            for row in rows {
                paths.push(row?);
            }
        }
    }

    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .map(|p| PagePath::new(&p).map_err(|e| StoreError::InvalidState(format!("page path: {e}"))))
        .collect()
}

#[cfg(test)]
mod tests {
    use ai_memory_core::{
        AgentKind, NewHandoff, NewObservation, NewPage, NewSession, ObservationKind, Tier,
    };
    use rusqlite::Connection;

    use super::*;
    use crate::ops::{
        begin_session, get_or_create_project, get_or_create_workspace, insert_handoff,
        insert_observation, upsert_page,
    };

    /// Every fixture writes a canary string. The tests assert on identity AND
    /// on the canary, because absent relational rows with a live FTS index is
    /// exactly the failure the issue reproduced.
    const CANARY_A: &str = "canary-alpha-9f3c";
    const CANARY_B: &str = "canary-bravo-71de";

    struct Fixture {
        _tmp: tempfile::TempDir,
        conn: Connection,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut conn = Connection::open(tmp.path().join("test.sqlite")).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let workspace_id = get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id = get_or_create_project(&mut conn, &workspace_id, "proj", None).unwrap();
        Fixture {
            _tmp: tmp,
            conn,
            workspace_id,
            project_id,
        }
    }

    /// A session with an observation, a handoff, and its heuristic page — the
    /// minimum shape every layer of the purge has to reach.
    fn seed_session(fx: &mut Fixture, canary: &str) -> SessionId {
        let session_id = SessionId::new();
        begin_session(
            &mut fx.conn,
            &NewSession {
                id: session_id,
                workspace_id: fx.workspace_id,
                project_id: fx.project_id,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            },
        )
        .unwrap();
        insert_observation(
            &mut fx.conn,
            &NewObservation {
                session_id,
                workspace_id: fx.workspace_id,
                project_id: fx.project_id,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: canary.into(),
                importance: 5,
            },
        )
        .unwrap();
        insert_handoff(
            &mut fx.conn,
            &NewHandoff {
                workspace_id: fx.workspace_id,
                project_id: fx.project_id,
                from_session_id: Some(session_id),
                from_agent: AgentKind::Codex,
                to_agent: None,
                cwd: None,
                summary: canary.into(),
                open_questions: Vec::new(),
                next_steps: Vec::new(),
                files_touched: Vec::new(),
                owner_user: None,
            },
        )
        .unwrap();
        upsert_page(
            &mut fx.conn,
            &NewPage {
                source_session_id: Some(session_id),
                workspace_id: fx.workspace_id,
                project_id: fx.project_id,
                path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
                title: "session".into(),
                body: canary.into(),
                tier: Tier::Episodic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                entities: Vec::new(),
                author_id: None,
                expires_at: None,
            },
        )
        .unwrap();
        session_id
    }

    fn count(conn: &Connection, sql: &str, canary: &str) -> i64 {
        conn.query_row(sql, params![canary], |row| row.get(0))
            .unwrap()
    }

    /// FTS5 reads `-` as an operator and `x:` as a column filter, so a canary
    /// has to be matched as a quoted phrase rather than a bare term.
    fn fts_count(conn: &Connection, sql: &str, canary: &str) -> i64 {
        count(conn, sql, &format!("\"{canary}\""))
    }

    /// Search every layer that can retain content, by canary rather than by id.
    fn canary_hits(conn: &Connection, canary: &str) -> Vec<&'static str> {
        let mut hits = Vec::new();
        if count(
            conn,
            "SELECT COUNT(*) FROM observations WHERE body LIKE '%' || ?1 || '%'",
            canary,
        ) > 0
        {
            hits.push("observations");
        }
        if fts_count(
            conn,
            "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH ?1",
            canary,
        ) > 0
        {
            hits.push("observations_fts");
        }
        if count(
            conn,
            "SELECT COUNT(*) FROM handoffs WHERE summary LIKE '%' || ?1 || '%'",
            canary,
        ) > 0
        {
            hits.push("handoffs");
        }
        if count(
            conn,
            "SELECT COUNT(*) FROM pages WHERE body LIKE '%' || ?1 || '%'",
            canary,
        ) > 0
        {
            hits.push("pages");
        }
        if fts_count(
            conn,
            "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH ?1",
            canary,
        ) > 0
        {
            hits.push("pages_fts");
        }
        hits
    }

    #[test]
    fn purge_removes_every_layer_and_leaves_only_a_tombstone() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);
        assert!(
            !canary_hits(&fx.conn, CANARY_A).is_empty(),
            "fixture must actually store the canary"
        );

        let receipt =
            purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap();

        assert_eq!(receipt.status, SessionPurgeStatus::Purged);
        assert_eq!(receipt.counts.sessions, 1);
        assert_eq!(receipt.counts.observations, 1);
        assert_eq!(receipt.counts.handoffs, 1);
        assert_eq!(receipt.counts.page_versions, 1);
        assert_eq!(
            receipt.removed_pages,
            vec![PagePath::new(format!("sessions/{session_id}.md")).unwrap()]
        );

        // Identity is gone.
        assert_eq!(
            fx.conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                    params![session_id.as_bytes()],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        // And so is the content, including both external-content FTS indexes:
        // absent relational rows with a live index is not deletion.
        assert_eq!(canary_hits(&fx.conn, CANARY_A), Vec::<&str>::new());

        let tombstone = find_session_tombstone(&fx.conn, session_id)
            .unwrap()
            .expect("a tombstone must survive");
        assert_eq!(tombstone.tombstone_id, receipt.tombstone_id);
        assert_eq!(tombstone.schema_version, SESSION_PURGE_SCHEMA_VERSION);
        assert_eq!(tombstone.workspace_id, fx.workspace_id);
    }

    #[test]
    fn purge_leaves_a_sibling_session_untouched() {
        let mut fx = fixture();
        let session_a = seed_session(&mut fx, CANARY_A);
        let session_b = seed_session(&mut fx, CANARY_B);

        purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_a).unwrap();

        assert_eq!(canary_hits(&fx.conn, CANARY_A), Vec::<&str>::new());
        assert_eq!(
            canary_hits(&fx.conn, CANARY_B),
            vec![
                "observations",
                "observations_fts",
                "handoffs",
                "pages",
                "pages_fts"
            ],
            "B must survive in every layer"
        );
        assert!(
            find_session_tombstone(&fx.conn, session_b)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn purge_is_idempotent_and_keeps_the_original_tombstone() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);

        let first =
            purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap();
        let second =
            purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap();

        assert_eq!(second.status, SessionPurgeStatus::AlreadyPurged);
        assert_eq!(second.tombstone_id, first.tombstone_id);
        assert_eq!(second.purged_at, first.purged_at);
        assert_eq!(second.counts, SessionPurgeCounts::default());
        // A repeat must not fail on the absence of what the first call removed.
        assert_eq!(canary_hits(&fx.conn, CANARY_A), Vec::<&str>::new());
    }

    #[test]
    fn purge_refuses_a_session_that_lives_in_another_project() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);
        let other = get_or_create_project(&mut fx.conn, &fx.workspace_id, "other", None).unwrap();

        let err = purge_session(&mut fx.conn, fx.workspace_id, other, session_id).unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidState(ref m) if m.contains("different workspace or project")),
            "unexpected error: {err:?}"
        );
        // Naming the wrong project must not have removed anything.
        assert!(!canary_hits(&fx.conn, CANARY_A).is_empty());
    }

    #[test]
    fn purge_refuses_an_unknown_session() {
        let mut fx = fixture();
        let err = purge_session(
            &mut fx.conn,
            fx.workspace_id,
            fx.project_id,
            SessionId::new(),
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn tombstones_are_scoped_so_a_same_id_session_elsewhere_still_ingests() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);
        let other = get_or_create_project(&mut fx.conn, &fx.workspace_id, "other", None).unwrap();
        purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap();

        assert!(is_session_purged(&fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap());
        assert!(
            !is_session_purged(&fx.conn, fx.workspace_id, other, session_id).unwrap(),
            "a tombstone must not suppress the same UUID in a different project"
        );
    }

    #[test]
    fn busy_detection_reports_an_open_session() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);
        assert_eq!(
            session_purge_is_busy(&fx.conn, session_id)
                .unwrap()
                .as_deref(),
            Some("session is still open")
        );
        fx.conn
            .execute(
                "UPDATE sessions SET ended_at = 1 WHERE id = ?1",
                params![session_id.as_bytes()],
            )
            .unwrap();
        assert_eq!(session_purge_is_busy(&fx.conn, session_id).unwrap(), None);
    }

    /// The `SET NULL` trap: a later page version that declares no provenance
    /// inherits the superseded version's, so an LLM rewrite of a session page
    /// cannot launder it out of the purge's reach.
    #[test]
    fn page_provenance_survives_a_rewrite_that_declares_none() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);
        let path = PagePath::new(format!("sessions/{session_id}.md")).unwrap();

        upsert_page(
            &mut fx.conn,
            &NewPage {
                source_session_id: None,
                workspace_id: fx.workspace_id,
                project_id: fx.project_id,
                path: path.clone(),
                title: "rewritten".into(),
                body: format!("rewritten {CANARY_A}"),
                tier: Tier::Episodic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                entities: Vec::new(),
                author_id: None,
                expires_at: None,
            },
        )
        .unwrap();

        let inherited: Option<Vec<u8>> = fx
            .conn
            .query_row(
                "SELECT source_session_id FROM pages WHERE path = ?1 AND is_latest = 1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            inherited.as_deref(),
            Some(&session_id.as_bytes()[..]),
            "a rewrite must inherit the superseded version's provenance"
        );

        let receipt =
            purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap();
        assert_eq!(receipt.counts.page_versions, 2, "both versions must go");
        assert_eq!(canary_hits(&fx.conn, CANARY_A), Vec::<&str>::new());
    }

    /// Multi-page consolidation writes arbitrary paths, so only the relational
    /// link can find them. Without it the page would survive the purge.
    #[test]
    fn purge_removes_multi_page_output_by_provenance_alone() {
        let mut fx = fixture();
        let session_id = seed_session(&mut fx, CANARY_A);
        let derived = PagePath::new("architecture/decisions.md").unwrap();
        upsert_page(
            &mut fx.conn,
            &NewPage {
                source_session_id: Some(session_id),
                workspace_id: fx.workspace_id,
                project_id: fx.project_id,
                path: derived.clone(),
                title: "derived".into(),
                body: format!("derived from {CANARY_A}"),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                entities: Vec::new(),
                author_id: None,
                expires_at: None,
            },
        )
        .unwrap();

        let receipt =
            purge_session(&mut fx.conn, fx.workspace_id, fx.project_id, session_id).unwrap();
        assert!(
            receipt.removed_pages.contains(&derived),
            "removed pages must include multi-page output: {:?}",
            receipt.removed_pages
        );
        assert_eq!(canary_hits(&fx.conn, CANARY_A), Vec::<&str>::new());
    }
}

-- Strong per-session deletion (#387), part 1: the durable tombstone ledger and
-- the structural provenance a purge needs to find derived content.
--
-- Why provenance columns rather than following the existing foreign keys: the
-- links that would identify derived rows are exactly the ones the delete
-- destroys. `auto_improve_runs.session_id` and
-- `auto_improve_rejections.source_run_id` / `.source_proposal_id` are all
-- `ON DELETE SET NULL`, so deleting the session first silently orphans the
-- evidence instead of removing it. These columns are deliberately FK-free and
-- write-once: they must outlive the row they point at so the purge can collect
-- targets before cutting anything, and so a partially-applied purge converges
-- on a retry instead of losing its bearings.

-- One row per purged session. Content-free by contract: scope, id, when, the
-- schema version of the purge that produced it, and the audit-log id. Never a
-- title, path, body, or count — a tombstone that quoted the deleted session
-- would defeat the deletion it records.
CREATE TABLE session_tombstones (
    session_id      BLOB PRIMARY KEY NOT NULL,
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    tombstone_id    BLOB NOT NULL,
    schema_version  INTEGER NOT NULL,
    purged_at       INTEGER NOT NULL,
    audit_id        INTEGER
) WITHOUT ROWID;

-- The ledger is consulted on every ingest for a session that no longer exists,
-- so the lookup must be keyed the same way the hook path resolves scope.
CREATE INDEX idx_session_tombstones_scope
    ON session_tombstones(workspace_id, project_id, purged_at DESC);

-- Backup snapshots taken before this instant may still contain purged content;
-- restore reapplies the ledger, so the newest purge time is the cutoff.
CREATE INDEX idx_session_tombstones_purged_at
    ON session_tombstones(purged_at DESC);

-- Which session a page version was derived from. NULL means "not derived from
-- a single session" (hand-written pages, multi-source consolidations that
-- predate this column) — never "unknown", so the purge can distinguish a page
-- it may keep from one it must rebuild or remove.
--
-- Deliberately not a foreign key: a purge deletes the session row, and this
-- must still name it while the derived rows are being collected.
ALTER TABLE pages ADD COLUMN source_session_id BLOB;

CREATE INDEX idx_pages_source_session
    ON pages(source_session_id)
    WHERE source_session_id IS NOT NULL;

-- Immutable copy of `auto_improve_runs.session_id`, which is `SET NULL` on
-- delete and therefore useless to a purge that has already begun.
ALTER TABLE auto_improve_runs ADD COLUMN source_session_id BLOB;

CREATE INDEX idx_auto_improve_runs_source_session
    ON auto_improve_runs(source_session_id)
    WHERE source_session_id IS NOT NULL;

-- Rejections keep reason, summary, and evidence derived from the session, and
-- reach it only through run/proposal pointers that are themselves `SET NULL`.
ALTER TABLE auto_improve_rejections ADD COLUMN source_session_id BLOB;

CREATE INDEX idx_auto_improve_rejections_source_session
    ON auto_improve_rejections(source_session_id)
    WHERE source_session_id IS NOT NULL;

-- Backfill what is still recoverable at migration time. Runs whose session was
-- already reaped have a NULL `session_id` and stay NULL here: this migration
-- recovers provenance, it does not invent it.
UPDATE auto_improve_runs
   SET source_session_id = session_id
 WHERE session_id IS NOT NULL;

UPDATE auto_improve_rejections
   SET source_session_id = (
           SELECT r.source_session_id
             FROM auto_improve_runs r
            WHERE r.id = auto_improve_rejections.source_run_id
       )
 WHERE source_run_id IS NOT NULL;

-- `pages` is intentionally NOT backfilled. The heuristic session page is
-- addressed by the deterministic path `sessions/<uuid>.md`, which the purge
-- resolves by identity, and SQLite cannot parse a UUID text into the BLOB
-- form stored in `sessions.id`. Guessing provenance from page text would be
-- exactly the textual search the deletion contract forbids.

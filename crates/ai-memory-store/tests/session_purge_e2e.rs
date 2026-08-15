//! End-to-end verification of strong per-session deletion (#387).
//!
//! The issue's reproduction is the shape this test follows: seed two sessions
//! with distinct canaries, purge one, then search **every retained layer** for
//! both canaries — by identity AND by content. Absent relational rows with a
//! live FTS index, a surviving wiki file, or a recoverable git blob is not
//! deletion, and only a content search catches those.
//!
//! Session B is the control throughout: a purge that also damages unrelated
//! sessions is not a correct purge, so every check that asserts A is gone has a
//! partner asserting B is untouched.

use ai_memory_core::{
    AgentKind, NewHandoff, NewObservation, NewPage, NewSession, ObservationKind, PagePath,
    Sanitized, Sanitizer, SessionId, Tier,
};
use ai_memory_store::Store;
use ai_memory_store::session_purge::{
    SessionPurgeStatus, count_resurrected, purge_session, read_ledger_from_db, reapply_ledger,
};
use rusqlite::Connection;

/// Unique per session, so a hit anywhere is unambiguous evidence.
const CANARY_A: &str = "canaryalpha9f3c";
const CANARY_B: &str = "canarybravo71de";

struct Seeded {
    session_id: SessionId,
    page: PagePath,
}

async fn seed(store: &Store, canary: &str) -> Seeded {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "audit".to_string(), None)
        .await
        .unwrap();
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: format!("please remember {canary}"),
                importance: 7,
            },
            &Sanitizer::default(),
        ))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(session_id),
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: None,
            summary: format!("handoff mentioning {canary}"),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: None,
        })
        .await
        .unwrap();
    let page = PagePath::new(format!("sessions/{session_id}.md")).unwrap();
    store
        .writer
        .upsert_page(NewPage {
            source_session_id: Some(session_id),
            workspace_id: ws,
            project_id: proj,
            path: page.clone(),
            title: "session".into(),
            body: format!("session page holding {canary}"),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            entities: Vec::new(),
            author_id: None,
            expires_at: None,
        })
        .await
        .unwrap();
    // A second, arbitrarily-named page, as multi-page consolidation produces.
    // Only the relational provenance can find this one.
    store
        .writer
        .upsert_page(NewPage {
            source_session_id: Some(session_id),
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(format!("derived/{session_id}-notes.md")).unwrap(),
            title: "derived".into(),
            body: format!("derived note quoting {canary}"),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            entities: Vec::new(),
            author_id: None,
            expires_at: None,
        })
        .await
        .unwrap();
    Seeded { session_id, page }
}

/// Every layer that can retain content, searched by canary rather than by id.
fn layers_holding(conn: &Connection, canary: &str) -> Vec<&'static str> {
    let count = |sql: &str, param: &str| -> i64 {
        conn.query_row(sql, rusqlite::params![param], |row| row.get(0))
            .unwrap()
    };
    let mut hits = Vec::new();
    if count(
        "SELECT COUNT(*) FROM observations WHERE body LIKE '%' || ?1 || '%'",
        canary,
    ) > 0
    {
        hits.push("observations");
    }
    if count(
        "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH ?1",
        canary,
    ) > 0
    {
        hits.push("observations_fts");
    }
    if count(
        "SELECT COUNT(*) FROM handoffs WHERE summary LIKE '%' || ?1 || '%'",
        canary,
    ) > 0
    {
        hits.push("handoffs");
    }
    if count(
        "SELECT COUNT(*) FROM pages WHERE body LIKE '%' || ?1 || '%'",
        canary,
    ) > 0
    {
        hits.push("pages");
    }
    if count(
        "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH ?1",
        canary,
    ) > 0
    {
        hits.push("pages_fts");
    }
    hits
}

/// Any file under the data dir whose bytes contain the canary.
fn files_holding(root: &std::path::Path, canary: &str) -> Vec<std::path::PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains(canary))
                .unwrap_or(false)
            {
                hits.push(path);
            }
        }
    }
    hits
}

/// Copy the database the way a backup must: checkpoint first, so the snapshot
/// is a complete database rather than one missing whatever is still in the WAL.
fn snapshot_db(db_path: &std::path::Path, dest: &std::path::Path) {
    let conn = Connection::open(db_path).unwrap();
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    drop(conn);
    std::fs::copy(db_path, dest).unwrap();
}

#[tokio::test]
async fn purge_removes_a_session_from_every_layer_and_spares_its_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = seed(&store, CANARY_A).await;
    let b = seed(&store, CANARY_B).await;
    let db_path = store.db_path().to_path_buf();

    // The fixture must genuinely store both canaries, or the test proves
    // nothing about the purge.
    {
        let conn = Connection::open(&db_path).unwrap();
        assert!(!layers_holding(&conn, CANARY_A).is_empty());
        assert!(!layers_holding(&conn, CANARY_B).is_empty());
    }

    let (ws, proj, _) = store
        .reader
        .find_session_scope(a.session_id)
        .await
        .unwrap()
        .unwrap();
    let receipt = store
        .writer
        .purge_session(ws, proj, a.session_id)
        .await
        .unwrap();
    assert_eq!(receipt.status, SessionPurgeStatus::Purged);
    assert!(
        receipt.removed_pages.contains(&a.page),
        "the session page must be among the removed pages"
    );
    assert_eq!(
        receipt.removed_pages.len(),
        2,
        "multi-page output is found by provenance, not just the session path"
    );

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        layers_holding(&conn, CANARY_A),
        Vec::<&str>::new(),
        "A must be gone from every layer, including both FTS indexes"
    );
    assert_eq!(
        layers_holding(&conn, CANARY_B),
        vec![
            "observations",
            "observations_fts",
            "handoffs",
            "pages",
            "pages_fts"
        ],
        "B must be untouched in every layer"
    );
    assert!(
        store
            .reader
            .find_session_scope(a.session_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .reader
            .find_session_scope(b.session_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .reader
            .is_session_tombstoned(a.session_id)
            .await
            .unwrap()
    );
    assert!(
        !store
            .reader
            .is_session_tombstoned(b.session_id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_restored_pre_purge_snapshot_is_re_purged_before_it_can_be_served() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = seed(&store, CANARY_A).await;
    seed(&store, CANARY_B).await;
    let db_path = store.db_path().to_path_buf();

    // Snapshot BEFORE the purge — the dangerous case: it holds everything the
    // purge is about to remove.
    let snapshot = tmp.path().join("pre-purge.sqlite");
    snapshot_db(&db_path, &snapshot);

    let (ws, proj, _) = store
        .reader
        .find_session_scope(a.session_id)
        .await
        .unwrap()
        .unwrap();
    store
        .writer
        .purge_session(ws, proj, a.session_id)
        .await
        .unwrap();

    // The ledger is what has to survive the restore.
    let ledger = read_ledger_from_db(&db_path).unwrap();
    assert_eq!(ledger.len(), 1);
    drop(store);

    // Restoring the snapshot brings the purged session back...
    std::fs::copy(&snapshot, &db_path).unwrap();
    {
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            !layers_holding(&conn, CANARY_A).is_empty(),
            "the snapshot must genuinely resurrect A, or this proves nothing"
        );
    }
    assert_eq!(
        count_resurrected(&db_path).unwrap(),
        0,
        "the restored snapshot has no ledger of its own yet"
    );

    // ...and reapplying the ledger must remove it again.
    let repurged = reapply_ledger(&db_path, &ledger).unwrap();
    assert_eq!(repurged.len(), 1);
    assert_eq!(repurged[0].session_id, a.session_id);
    assert_eq!(
        repurged[0].repo_paths.len(),
        2,
        "the caller is told which files and history to reapply"
    );

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        layers_holding(&conn, CANARY_A),
        Vec::<&str>::new(),
        "a restore must not serve content the ledger says was deleted"
    );
    assert_eq!(
        layers_holding(&conn, CANARY_B),
        vec![
            "observations",
            "observations_fts",
            "handoffs",
            "pages",
            "pages_fts"
        ],
        "reapplying the ledger must not harm sessions it does not cover"
    );
    assert_eq!(
        count_resurrected(&db_path).unwrap(),
        0,
        "the fail-closed check must pass once the ledger is reapplied"
    );

    // The original receipt must keep identifying this deletion.
    let after = read_ledger_from_db(&db_path).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].tombstone_id, ledger[0].tombstone_id);
    assert_eq!(after[0].purged_at, ledger[0].purged_at);
}

#[tokio::test]
async fn a_snapshot_taken_after_the_purge_carries_the_tombstone_and_not_the_data() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = seed(&store, CANARY_A).await;
    let db_path = store.db_path().to_path_buf();
    let (ws, proj, _) = store
        .reader
        .find_session_scope(a.session_id)
        .await
        .unwrap()
        .unwrap();
    let receipt = store
        .writer
        .purge_session(ws, proj, a.session_id)
        .await
        .unwrap();
    drop(store);

    let post = tmp.path().join("post-purge.sqlite");
    snapshot_db(&db_path, &post);

    let conn = Connection::open(&post).unwrap();
    assert_eq!(
        layers_holding(&conn, CANARY_A),
        Vec::<&str>::new(),
        "a post-purge snapshot must not contain the purged content"
    );
    let ledger = read_ledger_from_db(&post).unwrap();
    assert_eq!(ledger.len(), 1, "and it must carry the tombstone");
    assert_eq!(ledger[0].tombstone_id, receipt.tombstone_id);
    assert!(receipt.backup_cutoff >= receipt.purged_at);
}

#[tokio::test]
async fn a_purge_leaves_no_trace_of_the_canary_anywhere_under_the_data_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = seed(&store, CANARY_A).await;
    seed(&store, CANARY_B).await;

    assert!(
        !files_holding(tmp.path(), CANARY_A).is_empty(),
        "the fixture must write A's canary to disk"
    );

    let (ws, proj, _) = store
        .reader
        .find_session_scope(a.session_id)
        .await
        .unwrap()
        .unwrap();
    let receipt = store
        .writer
        .purge_session(ws, proj, a.session_id)
        .await
        .unwrap();

    // The store purge clears the index; the caller removes the files it names.
    let wiki_root = tmp.path().join("wiki");
    for page in &receipt.removed_pages {
        let abs = wiki_root
            .join(ws.to_string())
            .join(proj.to_string())
            .join(page.as_str());
        let _ = std::fs::remove_file(abs);
    }
    // Checkpoint so the deletions are reflected in the working tree state the
    // next reader sees.
    drop(store);

    let remaining = files_holding(tmp.path(), CANARY_A);
    assert!(
        remaining.is_empty(),
        "A's canary must not survive in any file under the data dir: {remaining:?}"
    );
    assert!(
        !files_holding(tmp.path(), CANARY_B).is_empty(),
        "B's files must survive"
    );
}

#[tokio::test]
async fn purging_a_second_time_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = seed(&store, CANARY_A).await;
    let db_path = store.db_path().to_path_buf();
    let (ws, proj, _) = store
        .reader
        .find_session_scope(a.session_id)
        .await
        .unwrap()
        .unwrap();

    let first = store
        .writer
        .purge_session(ws, proj, a.session_id)
        .await
        .unwrap();
    let counts_after_first = {
        let conn = Connection::open(&db_path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM pages", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };

    let second = store
        .writer
        .purge_session(ws, proj, a.session_id)
        .await
        .unwrap();
    assert_eq!(second.status, SessionPurgeStatus::AlreadyPurged);
    assert_eq!(second.tombstone_id, first.tombstone_id);
    assert_eq!(second.purged_at, first.purged_at);

    let conn = Connection::open(&db_path).unwrap();
    let counts_after_second = conn
        .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(
        counts_after_first, counts_after_second,
        "a repeat purge must not change any counts"
    );
}

#[tokio::test]
async fn a_purge_cannot_be_aimed_at_another_projects_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut conn_holder = Store::open(tmp.path()).unwrap();
    let a = seed(&conn_holder, CANARY_A).await;
    let ws = conn_holder
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let other = conn_holder
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();
    let db_path = conn_holder.db_path().to_path_buf();
    conn_holder = Store::open(tmp.path()).unwrap();
    drop(conn_holder);

    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let err = purge_session(&mut conn, ws, other, a.session_id).unwrap_err();
    assert!(
        format!("{err}").contains("different workspace or project"),
        "unexpected error: {err}"
    );
    assert!(
        !layers_holding(&conn, CANARY_A).is_empty(),
        "a misdirected purge must not remove anything"
    );
}

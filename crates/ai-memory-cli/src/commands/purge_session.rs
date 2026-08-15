//! `ai-memory purge-session` — thin HTTP client for strong per-session
//! deletion (#387).

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PurgeSessionArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/purge-session`.
#[derive(Serialize)]
struct PurgeSessionRequest {
    workspace: String,
    project: String,
    session_id: String,
    confirm: bool,
    /// Purge even when the session is open or has work in flight.
    force: bool,
}

/// Run the `purge-session` subcommand.
///
/// The UUID is validated client-side before anything is sent: a typo that
/// happened to name a different real session would be unrecoverable, so the
/// only accepted form is a complete, well-formed session id.
///
/// # Errors
/// Returns an error when `--confirm` is absent, `--session-id` is not a valid
/// UUID, the server is unreachable, or the server returns a non-2xx response.
pub async fn run(config: &Config, args: PurgeSessionArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    let session_id: ai_memory_core::SessionId = args.session_id.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "--session-id must be a complete session UUID (got {:?}). \
             Prefixes and titles are not accepted: this deletion is irreversible.",
            args.session_id
        )
    })?;

    if !args.confirm {
        bail!(
            "purge-session is destructive and irreversible. It removes the session, its\n\
             observations, handoffs, derived pages, and auto-improvement artifacts across\n\
             every layer, leaving only a content-free tombstone.\n\n\
             Re-run with --confirm to proceed:\n\n  \
             ai-memory purge-session --workspace {workspace} --project {project} \
             --session-id {session_id} --confirm",
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let receipt: serde_json::Value = post_json(
        &endpoint,
        "/admin/purge-session",
        &PurgeSessionRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            session_id: session_id.to_string(),
            confirm: true,
            force: args.force,
        },
    )
    .await?;

    // The server's tombstone already refuses these events with 410, so a drain
    // would discard them eventually. Sweeping now means the content is not
    // still sitting in a plaintext spool file on this host while the operator
    // believes the deletion is done. Spools on OTHER hosts cannot be reached
    // from here — the tombstone is what protects those.
    let sweep = crate::commands::hook_spool::sweep_session(&config.data_dir, session_id)?;

    if args.json {
        let mut receipt = receipt;
        receipt["local_spool"] = serde_json::json!({
            "removed": sweep.removed,
            "kept": sweep.kept,
            "unreadable": sweep.unreadable,
        });
        println!("{}", serde_json::to_string_pretty(&receipt)?);
        return Ok(());
    }

    let status = receipt["status"].as_str().unwrap_or("unknown");
    let counts = &receipt["counts"];
    let total: u64 = counts
        .as_object()
        .map(|map| map.values().filter_map(serde_json::Value::as_u64).sum())
        .unwrap_or(0);

    if status == "already_purged" {
        println!("already purged: {workspace}/{project} session {session_id}");
    } else {
        println!("purged {workspace}/{project} session {session_id} ({total} rows)");
        for (layer, value) in counts.as_object().into_iter().flatten() {
            if value.as_u64().unwrap_or(0) > 0 {
                println!("  {layer}: {value}");
            }
        }
    }
    if sweep.removed > 0 {
        println!("  local spool: {} queued events removed", sweep.removed);
    }
    if sweep.unreadable > 0 {
        eprintln!(
            "  warning: {} local spool entries could not be read or removed",
            sweep.unreadable
        );
    }
    if let Some(cutoff) = receipt["backup_cutoff"].as_str() {
        println!("  backups taken before {cutoff} still contain this session");
    }
    println!(
        "  spools on other hosts are not reachable from here; the server tombstone refuses them with 410"
    );
    for warning in receipt["warnings"].as_array().into_iter().flatten() {
        if let Some(text) = warning.as_str() {
            eprintln!("  warning: {text}");
        }
    }
    println!("\n{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

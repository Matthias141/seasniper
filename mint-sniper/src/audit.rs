//! Persistent, append-only audit log for the events RUNBOOK.md's incident
//! checklists actually need a durable record of: arm, disarm, fire, and
//! config-change (plus per-wallet mint results, since "did a specific mint
//! succeed" is exactly what RUNBOOK.md's post-fire-verification section
//! asks). Deliberately a separate file, not a reuse of tracing's stdout
//! output (`tracing_subscriber::fmt::init()` in main.rs): tracing is for
//! operator debugging — every level, verbose, formatted for a human
//! watching a live terminal, and gone the moment that terminal isn't
//! captured. RUNBOOK.md's post-fire-verification section is written to
//! assume exactly that loss already happened ("if that's gone too... you
//! have no durable record") — this file is what closes that gap. JSON
//! Lines format: one compact JSON object per line, so appending needs no
//! read-modify-write of the whole file and the result is trivially
//! greppable/`jq`-able without special tooling.
//!
//! Implemented as a `bus.rs` subscriber, not scattered call sites in
//! `api.rs`/`main.rs`: `bus.rs` is already, by its own doc comment, "the
//! single source of truth for everything the UI displays" for exactly
//! these event kinds. Subscribing here means every future arm/disarm/
//! fire/mint-result/config-change is audited automatically, with no risk
//! of a new call site forgetting to also call an audit function.
//!
//! One real trade-off, stated plainly rather than assumed away:
//! `bus.rs`'s broadcast channel has a 256-event buffer and drops the
//! oldest event on a slow receiver instead of blocking the sender (see
//! `bus.rs`'s own doc comment, made for the same reason there). If this
//! task ever falls more than 256 events behind — not plausible at this
//! bot's event volume, but worth stating — it sees a `Lagged` error and
//! whatever events were dropped in between are gone from the audit log
//! too, not just the UI. That gap is itself written to the log as an
//! `audit_gap` entry rather than silently passed over.

use crate::bus::{now_ts, EventBus, ServerEvent};
use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::RecvError;

#[derive(Serialize)]
struct AuditRecord<'a> {
    ts: u64,
    event: &'a str,
    #[serde(flatten)]
    detail: serde_json::Value,
}

async fn append(path: &str, event: &str, detail: serde_json::Value) -> Result<()> {
    let record = AuditRecord {
        ts: now_ts(),
        event,
        detail,
    };
    let mut line = serde_json::to_string(&record).context("serializing audit record")?;
    line.push('\n');

    // Opened fresh on every write rather than held open for the process
    // lifetime: writes here are rare (arm/disarm/fire/config-change, not a
    // per-block or per-request path), so the extra open() cost is
    // irrelevant, and it means a log-rotation tool moving the file out
    // from under a long-running bot doesn't silently keep writing to a
    // deleted inode.
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("opening audit log at {path}"))?;
    file.write_all(line.as_bytes())
        .await
        .context("writing audit log entry")?;
    Ok(())
}

/// Subscribes to `bus` and persists the security/money-relevant subset of
/// its events to `path` as they happen. Runs for the lifetime of the
/// process — spawn with `tokio::spawn` from `main()` and never await the
/// handle. A write failure (disk full, permissions) is logged via
/// `tracing::error!` and otherwise ignored — audit logging must never be
/// able to take down arm/disarm/fire itself; it supplements `bus.rs`'s
/// live feed, it doesn't gate anything.
pub async fn run_audit_writer(bus: EventBus, path: String) {
    let mut rx = bus.subscribe();
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            Err(RecvError::Closed) => return,
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "audit writer lagged behind the event bus");
                let _ = append(
                    &path,
                    "audit_gap",
                    serde_json::json!({ "missed_events": missed }),
                )
                .await;
                continue;
            }
        };

        let (name, detail) = match &event {
            ServerEvent::ArmedState { armed } => (
                if *armed { "arm" } else { "disarm" },
                serde_json::json!({}),
            ),
            ServerEvent::TriggerFired { manual } => {
                ("fire", serde_json::json!({ "manual": manual }))
            }
            ServerEvent::MintResult {
                address,
                success,
                detail,
            } => (
                "mint_result",
                serde_json::json!({ "address": address, "success": success, "detail": detail }),
            ),
            ServerEvent::ConfigChanged => ("config_change", serde_json::json!({})),
            // Log/WalletUpdate/RpcHealth are routine and/or high-volume —
            // not what RUNBOOK.md's checklists need a durable record of.
            // Deliberately not persisted here; bus.rs's live feed still
            // carries them for the UI.
            ServerEvent::Log { .. } | ServerEvent::WalletUpdate { .. } | ServerEvent::RpcHealth { .. } => {
                continue
            }
        };

        if let Err(e) = append(&path, name, detail).await {
            tracing::error!(error = %e, event = name, "failed to write audit log entry");
        }
    }
}

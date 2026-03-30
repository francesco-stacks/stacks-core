//! `run_benchmark` tool – executes a benchmark run via the shared commands layer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::model::{Meta, ProgressNotificationParam, ProgressToken, ServerNotification};
use rmcp::{Peer, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use stacks_bench::StacksBlockRef;
use stacks_bench::bench_events::BenchEvent;
use tokio::sync::mpsc;

use crate::commands::bench::run::{BenchRunParams, FilterKind, RunResult};
use crate::commands::common::{IndexerUiSpawner, TxIdArg, silent_indexer_ui};
use crate::mcp::server::StacksBenchServer;

/// Parameters for the `run_benchmark` tool.
#[derive(Deserialize, JsonSchema)]
pub struct RunBenchmarkParams {
    /// Path to the Stacks node data directory (the directory containing the
    /// `chainstate` folder).
    pub source_dir: String,

    /// Stacks block (height or hex block id) to start at, inclusive.
    /// Defaults to block 1 if omitted.
    #[serde(default)]
    pub start_at: Option<String>,

    /// Stacks block (height or hex block id) to end at, inclusive.
    #[serde(default)]
    pub end_at: Option<String>,

    /// Number of blocks to process, starting from `start_at`.
    #[serde(default)]
    pub count: Option<u32>,

    /// A specific transaction id (hex) to benchmark. When set, `start_at`,
    /// `end_at`, and `count` are ignored.
    #[serde(default)]
    pub txid: Option<String>,

    /// Number of measured repetitions in `--txid` mode. Default: 10.
    #[serde(default)]
    pub repetitions: Option<u32>,

    /// Number of warmup blocks (block-range mode) or warmup repetitions
    /// (txid mode) before measurement begins. Default: 0.
    #[serde(default)]
    pub warmup: Option<u32>,

    /// Human-readable name for this benchmark run.
    #[serde(default)]
    pub name: Option<String>,

    /// Transaction filter. Currently only `"contract_call"` is supported.
    #[serde(default)]
    pub filter: Option<String>,

    /// Network name (e.g. `"mainnet"`, `"testnet"`). Inferred from the
    /// chainstate if omitted.
    #[serde(default)]
    pub network: Option<String>,

    /// Tip block (height or hex block id) to anchor canonical history
    /// resolution. Defaults to the node's current canonical tip.
    #[serde(default)]
    pub tip: Option<String>,
}

impl RunBenchmarkParams {
    /// Convert tool parameters into the shared `BenchRunParams`.
    fn into_bench_params(self) -> Result<BenchRunParams, String> {
        let filter = match self.filter.as_deref() {
            None => None,
            Some("contract_call") => Some(FilterKind::ContractCall),
            Some(other) => {
                return Err(format!(
                    "Unknown filter '{other}'. Supported filters: contract_call"
                ));
            }
        };

        let network = match self.network.as_deref() {
            None => None,
            Some(s) => Some(
                s.parse()
                    .map_err(|_| format!("Unknown network '{s}'. Use mainnet or testnet"))?,
            ),
        };

        let txid = match self.txid {
            None => None,
            Some(ref hex) => {
                let parsed: TxIdArg = hex
                    .parse()
                    .map_err(|e| format!("Invalid txid '{hex}': {e}"))?;
                Some(parsed)
            }
        };

        let parse_block_ref = |s: &str| -> Result<StacksBlockRef, String> {
            s.parse()
                .map_err(|e| format!("Invalid block ref '{s}': {e}"))
        };

        Ok(BenchRunParams {
            source_dir: self.source_dir.into(),
            start_at: self.start_at.as_deref().map(parse_block_ref).transpose()?,
            end_at: self.end_at.as_deref().map(parse_block_ref).transpose()?,
            tip: self.tip.as_deref().map(parse_block_ref).transpose()?,
            network,
            block_count: self.count,
            txid,
            repetitions: self.repetitions.unwrap_or(10),
            calibration: 20,
            warmup: self.warmup.unwrap_or(0) as usize,
            filter,
            no_profiler_kv: false,
            include_pre_nakamoto_blocks: false,
            name: self.name,
        })
    }
}

impl StacksBenchServer {
    pub async fn exec_run_benchmark(
        &self,
        params: RunBenchmarkParams,
        meta: Meta,
        client: Peer<RoleServer>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> anyhow::Result<String> {
        let bench_params = params
            .into_bench_params()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Wire up cancellation: MCP cancellation token OR ctrl-c.
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let flag = interrupted.clone();
            let ct = context.ct.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = ct.cancelled() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                flag.store(true, Ordering::Relaxed);
            });
        }

        // Spawn progress notification forwarder if the client provided a
        // progress token.
        let progress_token = meta.get_progress_token();
        if let Some(token) = progress_token {
            tokio::spawn(forward_bench_events(event_rx, client, token));
        } else {
            // Silently drain events.
            tokio::spawn(async move {
                let mut rx = event_rx;
                while rx.recv().await.is_some() {}
            });
        }

        let indexer_ui: IndexerUiSpawner = silent_indexer_ui();

        let mut app_db = self.app_db.clone();
        let result = crate::commands::bench::run::run_benchmark(
            &mut app_db,
            &bench_params,
            event_tx,
            interrupted,
            indexer_ui,
        )
        .await?;

        Ok(serde_json::to_string(&result)?)
    }
}

/// Format a `RunResult` as a concise summary string suitable for MCP tool
/// output (returned as the tool result text).
#[allow(dead_code)]
pub fn format_run_result(result: &RunResult) -> String {
    serde_json::to_string(result).unwrap_or_else(|_| "Failed to serialize result".into())
}

// ---------------------------------------------------------------------------
// Progress notification forwarder
// ---------------------------------------------------------------------------

pub(super) async fn forward_bench_events(
    mut rx: mpsc::UnboundedReceiver<BenchEvent>,
    client: Peer<RoleServer>,
    token: ProgressToken,
) {
    // Debounce: only send high-frequency progress events when the whole-
    // percent value changes (at most ~100 notifications per phase).
    let mut last_sent_pct: i32 = -1;

    while let Some(event) = rx.recv().await {
        let notification = match &event {
            BenchEvent::ShadowDirStarted => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    0.0,
                    None,
                    Some("Creating shadow directory..."),
                ))
            }
            BenchEvent::ShadowDirComplete { duration } => Some(progress(
                &token,
                0.0,
                None,
                Some(&format!(
                    "Shadow directory ready ({:.1}s)",
                    duration.as_secs_f64()
                )),
            )),
            BenchEvent::BaselineStarted {
                warmup_blocks,
                measured_blocks,
            } => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    0.0,
                    None,
                    Some(&format!(
                        "Measuring baseline overhead (warmup: {warmup_blocks}, measured: {measured_blocks})"
                    )),
                ))
            }
            BenchEvent::BaselineRoundProgress {
                round,
                completed,
                total,
            } => debounced(&mut last_sent_pct, *completed as f64, *total as f64, || {
                progress(
                    &token,
                    *completed as f64,
                    Some(*total as f64),
                    Some(&format!("Baseline round {round}")),
                )
            }),
            BenchEvent::ReplayStarted {
                total_blocks,
                warmup_blocks,
                ..
            } => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    0.0,
                    Some(*total_blocks as f64),
                    Some(&format!(
                        "Replaying blocks (warmup: {warmup_blocks}, measured: {})",
                        total_blocks - warmup_blocks
                    )),
                ))
            }
            BenchEvent::ReplayWarmupProgress { completed, total } => {
                debounced(&mut last_sent_pct, *completed as f64, *total as f64, || {
                    progress(
                        &token,
                        *completed as f64,
                        Some(*total as f64),
                        Some("Warmup"),
                    )
                })
            }
            BenchEvent::ReplayProgress { completed, total } => {
                debounced(&mut last_sent_pct, *completed as f64, *total as f64, || {
                    progress(&token, *completed as f64, Some(*total as f64), None)
                })
            }
            BenchEvent::ReplayComplete {
                measured_blocks,
                duration,
            } => {
                last_sent_pct = -1;
                Some(progress(
                    &token,
                    *measured_blocks as f64,
                    Some(*measured_blocks as f64),
                    Some(&format!("Replay complete ({:.1}s)", duration.as_secs_f64())),
                ))
            }
            BenchEvent::CleanupStarted => Some(progress(&token, 0.0, None, Some("Cleaning up..."))),
            BenchEvent::CleanupComplete => {
                Some(progress(&token, 0.0, None, Some("Cleanup complete")))
            }
            // Other events are not mapped to progress notifications.
            _ => None,
        };

        if let Some(params) = notification {
            let _ = client
                .send_notification(ServerNotification::ProgressNotification(
                    rmcp::model::ProgressNotification::new(params),
                ))
                .await;
        }
    }
}

/// Only produce a notification when the whole-percent value changes (or on
/// the very first and very last tick). This caps high-frequency per-block
/// events to ~100 notifications per phase.
fn debounced(
    last_pct: &mut i32,
    completed: f64,
    total: f64,
    make: impl FnOnce() -> ProgressNotificationParam,
) -> Option<ProgressNotificationParam> {
    if total <= 0.0 {
        return Some(make());
    }
    let pct = ((completed / total) * 100.0) as i32;
    if pct != *last_pct {
        *last_pct = pct;
        Some(make())
    } else {
        None
    }
}

fn progress(
    token: &ProgressToken,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) -> ProgressNotificationParam {
    let mut p = ProgressNotificationParam::new(token.clone(), progress);
    if let Some(t) = total {
        p = p.with_total(t);
    }
    if let Some(m) = message {
        p = p.with_message(m.to_owned());
    }
    p
}

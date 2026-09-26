use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::http::parse_error;
use crate::printer::Printer;

#[derive(Debug, Deserialize, Clone)]
struct PipelineChildStatus {
    /// `None` when the stage hasn't reported yet. Used on snapshot/reconnect
    /// to dump already-failed stages without waiting for a missed event.
    #[serde(default)]
    status: Option<String>,
    log_output: String,
}

#[derive(Debug, Deserialize, Clone)]
struct PipelineRun {
    /// Planned stages in execution order, each reported under a key of the
    /// same name. Stages are read by name rather than as fixed fields so that a
    /// CLI keeps working against a platform that has added, renamed or dropped
    /// one: a new CLI is published before the platform it was built against
    /// rolls out, and users keep old CLIs long after.
    stages: Vec<String>,
    status: String,
    error_summary: Option<String>,
    #[serde(flatten)]
    fields: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct PipelineStreamDelta {
    #[serde(default)]
    created_at: Option<String>,
    event: StreamEvent,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    stage: String,
    kind: String,
    /// `info` (headline, always shown) or `debug` (verbose-only). ADR 0003.
    #[serde(default = "default_log_level")]
    level: String,
    #[serde(default)]
    detail: Option<String>,
}

fn default_log_level() -> String {
    "debug".to_string()
}

/// Per-stage Debug-line buffer, dumped if the stage fails.
type StageLogBuffer = HashMap<String, Vec<(Option<String>, String)>>;

/// Newest log timestamp seen per stage. Log deltas carry no SSE id (they left
/// the journal cursor — ADR 0018), so a reconnect replays lines from the last
/// lifecycle event; anything at or before this mark was already shown.
type StageLogClock = HashMap<String, chrono::DateTime<chrono::Utc>>;

enum StreamOutcome {
    /// The pipeline reached a terminal state; carries the run's result.
    Terminal(Result<()>),
    /// The connection dropped (transport EOF, idle reset, proxy reload) before a
    /// terminal event. The caller reconnects with `?since=` and resumes.
    Disconnected,
}

/// Follow a run to its end. `label` names the run the way the user started
/// it, "Deploy" or "Promotion", and heads every line that speaks about the
/// run as a whole.
pub(crate) async fn watch_pipeline(
    printer: &Printer,
    client: &reqwest::Client,
    platform_api_origin: &str,
    token: &str,
    pipeline_run_id: i64,
    label: &str,
) -> Result<()> {
    // The server's event stream is resumable (`?since=N`; each event carries its
    // sequence as the SSE `id:`), so a dropped connection mid-build isn't fatal:
    // reconnect from the last sequence we saw and carry on. The longest deploys
    // (a from-scratch backend build) hold the stream open for minutes and are the
    // most exposed to a transient drop. Only a run of reconnects that make no
    // progress gives up.
    const MAX_RECONNECTS: u32 = 10;
    let mut buffers: StageLogBuffer = HashMap::new();
    let mut log_clock: StageLogClock = HashMap::new();
    let mut last_event_id: Option<i64> = None;
    let mut failures: u32 = 0;

    loop {
        let resume_from = last_event_id;
        match stream_once(
            printer,
            client,
            platform_api_origin,
            token,
            pipeline_run_id,
            resume_from,
            &mut buffers,
            &mut log_clock,
            &mut last_event_id,
            label,
        )
        .await?
        {
            StreamOutcome::Terminal(result) => return result,
            StreamOutcome::Disconnected => {
                // Forward progress on the dropped connection earns a fresh
                // reconnect budget; only a stall (repeated drops with nothing
                // new) trips the limit.
                if last_event_id != resume_from {
                    failures = 0;
                }
                failures += 1;
                if failures > MAX_RECONNECTS {
                    bail!(
                        "lost connection to the {lower} event stream after {MAX_RECONNECTS} reconnect attempts \
                         — the {lower} may still be running on the platform. \
                         Check the project dashboard or `gbandit logs backend` to see how it ended.",
                        lower = label.to_lowercase(),
                    );
                }
                printer.debug(
                    "pipeline",
                    None,
                    &format!(
                        "event stream dropped; reconnecting ({failures}/{MAX_RECONNECTS}) from sequence {}",
                        resume_from.map_or_else(|| "start".to_string(), |s| s.to_string())
                    ),
                );
                tokio::time::sleep(std::time::Duration::from_millis(
                    500 * u64::from(failures.min(6)),
                ))
                .await;
            }
        }
    }
}

/// One connection attempt. Reads SSE events, updating `buffers` and
/// `last_event_id`, until either the pipeline reaches a terminal state
/// (`Terminal`) or the connection drops (`Disconnected`). A non-success HTTP
/// status (gone, unauthorized) is a hard error, not a reconnectable drop.
#[allow(clippy::too_many_arguments)]
async fn stream_once(
    printer: &Printer,
    client: &reqwest::Client,
    platform_api_origin: &str,
    token: &str,
    pipeline_run_id: i64,
    since: Option<i64>,
    buffers: &mut StageLogBuffer,
    log_clock: &mut StageLogClock,
    last_event_id: &mut Option<i64>,
    label: &str,
) -> Result<StreamOutcome> {
    let mut request = client
        .get(format!(
            "{platform_api_origin}/pipelines/{pipeline_run_id}/stream"
        ))
        .bearer_auth(token)
        .header("accept", "text/event-stream");
    if let Some(since) = since {
        request = request.query(&[("since", since)]);
    }

    let mut response = match request.send().await {
        Ok(response) => response,
        // Couldn't open the connection — treat as a drop so the caller retries
        // under its reconnect budget rather than failing the whole deploy.
        Err(_) => return Ok(StreamOutcome::Disconnected),
    };
    if !response.status().is_success() {
        bail!(
            "failed to follow {} progress: {}",
            label.to_lowercase(),
            parse_error(response).await
        );
    }

    let mut buf = String::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            // Clean EOF or a transport error mid-stream, without a terminal
            // event — reconnect and resume from `last_event_id`.
            Ok(None) | Err(_) => return Ok(StreamOutcome::Disconnected),
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buf.find("\n\n") {
            let raw_event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(outcome) = handle_sse_event(
                printer,
                buffers,
                log_clock,
                last_event_id,
                &raw_event,
                label,
            )? {
                return Ok(StreamOutcome::Terminal(outcome));
            }
        }
    }
}

/// `Some(result)` when the run reaches a terminal state.
fn handle_sse_event(
    printer: &Printer,
    buffers: &mut StageLogBuffer,
    log_clock: &mut StageLogClock,
    last_event_id: &mut Option<i64>,
    raw: &str,
    label: &str,
) -> Result<Option<Result<()>>> {
    let mut event_type = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("event:") {
            event_type = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("data:") {
            data_lines.push(v.strip_prefix(' ').unwrap_or(v));
        } else if let Some(v) = line.strip_prefix("id:") {
            // The sequence cursor for `?since=` on reconnect. Tracked even for
            // events we can't fully parse, mirroring the server's replay cursor.
            if let Ok(seq) = v.trim().parse::<i64>() {
                *last_event_id = Some(seq);
            }
        }
    }
    let data = data_lines.join("\n");

    match event_type.as_str() {
        "snapshot" => {
            let snap: PipelineRun =
                serde_json::from_str(&data).context("failed to decode snapshot")?;
            // Reconnect catch-up: seed buffers from the snapshot's
            // log_output, and dump any stage already in `failed` since
            // no Failed event will arrive.
            for (label, child) in pipeline_stages(&snap) {
                if !child.log_output.is_empty() {
                    buffers.insert(label.to_string(), vec![(None, child.log_output.clone())]);
                    if printer.verbose {
                        printer.debug(label, None, &child.log_output);
                    }
                }
                if child.status.as_deref() == Some("failed") {
                    dump_failed_stage(printer, buffers, label, None);
                }
            }
            printer.status("pipeline", None, "status", Some(&snap.status));
            if is_terminal_status(&snap.status) {
                return Ok(Some(finish_terminal(
                    printer,
                    &snap.status,
                    snap.error_summary,
                    label,
                )));
            }
        }
        "" | "pipeline_event" => {
            let delta: PipelineStreamDelta = match serde_json::from_str(&data) {
                Ok(d) => d,
                Err(_) => return Ok(None),
            };
            let stage = delta.event.stage.clone();
            let created_at = delta.created_at.as_deref();
            match delta.event.kind.as_str() {
                "log" => {
                    if replayed_log(log_clock, &stage, delta.created_at.as_deref()) {
                        return Ok(None);
                    }
                    let text = delta.event.detail.as_deref().unwrap_or("");
                    let is_info = delta.event.level == "info";
                    if is_info {
                        printer.info(&stage, created_at, text);
                    } else {
                        // Buffer for potential failure dump; stream in verbose.
                        buffers
                            .entry(stage.clone())
                            .or_default()
                            .push((delta.created_at.clone(), text.to_string()));
                        printer.debug(&stage, created_at, text);
                    }
                }
                "started" | "succeeded" => {
                    if stage != "pipeline" {
                        printer.status(
                            &stage,
                            created_at,
                            delta.event.kind.as_str(),
                            delta.event.detail.as_deref(),
                        );
                    }
                }
                "skipped" => {
                    printer.status(&stage, created_at, "skipped", delta.event.detail.as_deref());
                }
                "failed" => {
                    let reason = delta.event.detail.as_deref().unwrap_or("");
                    if stage != "pipeline" {
                        // Always surface failures, even in default mode.
                        printer.print_event_line(
                            &stage,
                            created_at,
                            &format!("failed: {reason}"),
                            true,
                        );
                        dump_failed_stage(printer, buffers, &stage, created_at);
                    }
                }
                "cancelled" => {
                    let reason = delta.event.detail.as_deref().unwrap_or("");
                    printer.print_event_line(
                        &stage,
                        created_at,
                        &format!("cancelled: {reason}"),
                        true,
                    );
                }
                _ => {}
            }

            if stage == "pipeline" {
                match delta.event.kind.as_str() {
                    "succeeded" => {
                        printer.progress(format!("{label} succeeded."));
                        return Ok(Some(Ok(())));
                    }
                    "failed" => {
                        let summary = delta
                            .event
                            .detail
                            .clone()
                            .unwrap_or_else(|| format!("{} failed", label.to_lowercase()));
                        return Ok(Some(Err(anyhow::anyhow!("{}", summary))));
                    }
                    "cancelled" => {
                        let reason = delta
                            .event
                            .detail
                            .clone()
                            .unwrap_or_else(|| "cancelled".into());
                        return Ok(Some(Err(anyhow::anyhow!(
                            "{} cancelled: {}",
                            label.to_lowercase(),
                            reason
                        ))));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

/// True when a log delta is a reconnect replay: at or before the newest line
/// already shown for its stage. Advances the clock otherwise.
fn replayed_log(log_clock: &mut StageLogClock, stage: &str, created_at: Option<&str>) -> bool {
    let Some(timestamp) = created_at
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
    else {
        return false;
    };
    match log_clock.get(stage) {
        Some(newest) if timestamp <= *newest => true,
        _ => {
            log_clock.insert(stage.to_string(), timestamp);
            false
        }
    }
}

/// Print the buffered Debug log for a failed stage, with `--- log ---`
/// markers so the failure seam stands out.
fn dump_failed_stage(
    printer: &Printer,
    buffers: &mut StageLogBuffer,
    stage: &str,
    created_at: Option<&str>,
) {
    let entries = buffers.remove(stage).unwrap_or_default();
    if entries.is_empty() && printer.verbose {
        return;
    }
    println!("--- {stage} log ---");
    for (entry_ts, text) in &entries {
        for line in text.lines() {
            printer.print_event_line(stage, entry_ts.as_deref(), line, false);
        }
    }
    println!("--- end {stage} log ---");
    let _ = created_at;
}

fn finish_terminal(
    printer: &Printer,
    status: &str,
    error_summary: Option<String>,
    label: &str,
) -> Result<()> {
    if status == "succeeded" {
        printer.progress(format!("{label} succeeded."));
        Ok(())
    } else {
        bail!(
            "{}",
            error_summary.unwrap_or_else(|| format!("{} {status}", label.to_lowercase()))
        )
    }
}

fn pipeline_stages(pipeline: &PipelineRun) -> Vec<(&str, PipelineChildStatus)> {
    pipeline
        .stages
        .iter()
        .filter_map(|stage| {
            let child = serde_json::from_value(pipeline.fields.get(stage)?.clone()).ok()?;
            Some((stage.as_str(), child))
        })
        .collect()
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "skipped" | "cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_names(snapshot: serde_json::Value) -> Vec<String> {
        let run: PipelineRun = serde_json::from_value(snapshot).unwrap();
        pipeline_stages(&run)
            .into_iter()
            .map(|(stage, _)| stage.to_string())
            .collect()
    }

    fn child(status: &str) -> serde_json::Value {
        serde_json::json!({ "status": status, "log_output": "" })
    }

    #[test]
    fn reads_the_stages_the_platform_names_in_its_order() {
        let snapshot = serde_json::json!({
            "id": 7,
            "status": "running",
            "error_summary": null,
            "stages": ["frontend_build", "backend_build", "rollout"],
            "frontend_build": child("succeeded"),
            "backend_build": child("running"),
            "rollout": { "status": null, "log_output": "" },
        });
        assert_eq!(
            stage_names(snapshot),
            ["frontend_build", "backend_build", "rollout"]
        );
    }

    #[test]
    fn reads_stages_from_a_platform_older_than_the_cli() {
        let snapshot = serde_json::json!({
            "status": "running",
            "error_summary": null,
            "stages": ["frontend_build", "frontend_publish", "backend_build", "backend_deploy"],
            "frontend_build": child("succeeded"),
            "frontend_publish": child("succeeded"),
            "backend_build": child("succeeded"),
            "backend_deploy": child("running"),
        });
        assert_eq!(
            stage_names(snapshot),
            ["frontend_build", "frontend_publish", "backend_build", "backend_deploy"]
        );
    }

    #[test]
    fn skips_a_planned_stage_the_snapshot_reports_nothing_for() {
        let snapshot = serde_json::json!({
            "status": "pending",
            "error_summary": null,
            "stages": ["plan", "rollout"],
            "rollout": child("pending"),
        });
        assert_eq!(stage_names(snapshot), ["rollout"]);
    }
}

//! Scheduled-task domain: entities, schedule parsing (one-time UTC timestamps
//! and cron expressions), and presentation. Execution lives in
//! `application::task_runner`, persistence in `connector::sqlite`.

use crate::domain::rendering;
use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Discord message limit, minus headroom for the `»` dispatch prefix.
pub const PROMPT_MAX_LEN: usize = 1900;

/// A persisted scheduled task.
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: i64,
    pub status: String,
    pub schedule_kind: String,
    pub run_at: Option<DateTime<Utc>>,
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub attempts: i64,
    pub payload_json: String,
    pub prompt_preview: String,
    pub channel_id: Option<String>,
    pub thread_id: Option<String>,
}

/// A scheduled task about to be persisted.
#[derive(Debug, Clone)]
pub struct NewScheduledTask {
    pub schedule_kind: String,
    pub run_at: Option<DateTime<Utc>>,
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
    pub next_run_at: DateTime<Utc>,
    pub payload_json: String,
    pub prompt_preview: String,
    pub channel_id: Option<String>,
    pub thread_id: Option<String>,
}

/// What to do when a task fires, stored as JSON in `scheduled_tasks.payload_json`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskPayload {
    /// Post into an existing thread and continue its session.
    Thread { thread_id: String, prompt: String, user_id: Option<String> },
    /// Post a starter message in a project channel, create a thread, and
    /// (unless `notify_only`) start a session with the prompt.
    Channel {
        channel_id: String,
        prompt: String,
        /// Thread name; defaults to a preview of the prompt.
        name: Option<String>,
        notify_only: bool,
        user_id: Option<String>,
    },
}

#[derive(Debug)]
pub struct ParsedSendAt {
    pub schedule_kind: &'static str,
    pub run_at: Option<DateTime<Utc>>,
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
    pub next_run_at: DateTime<Utc>,
}

impl ParsedSendAt {
    /// An immediate one-shot: picked up on the runner's next poll.
    pub fn immediately(now: DateTime<Utc>) -> Self {
        Self {
            schedule_kind: "at",
            run_at: Some(now),
            cron_expr: None,
            timezone: None,
            next_run_at: now,
        }
    }
}

/// Parse a `--send-at` value: a UTC ISO timestamp ending in `Z` (one-time) or
/// a cron expression (recurring, evaluated in UTC).
pub fn parse_send_at(value: &str, now: DateTime<Utc>) -> Result<ParsedSendAt> {
    let value = value.trim();
    let looks_like_cron = value.starts_with('@') || value.split_whitespace().count() >= 5;
    if looks_like_cron {
        let next = next_cron_run(value, now)?;
        return Ok(ParsedSendAt {
            schedule_kind: "cron",
            run_at: None,
            cron_expr: Some(value.to_string()),
            timezone: Some("UTC".to_string()),
            next_run_at: next,
        });
    }
    if !value.ends_with('Z') {
        return Err(anyhow!(
            "--send-at must be a UTC ISO timestamp ending in Z (e.g. 2026-03-01T09:00:00Z) or a cron expression"
        ));
    }
    let at = DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid timestamp: {value}"))?
        .with_timezone(&Utc);
    if at <= now {
        return Err(anyhow!("--send-at must be in the future ({value} is not)"));
    }
    Ok(ParsedSendAt {
        schedule_kind: "at",
        run_at: Some(at),
        cron_expr: None,
        timezone: None,
        next_run_at: at,
    })
}

pub fn next_cron_run(expr: &str, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let cron = Cron::from_str(expr).map_err(|e| anyhow!("invalid cron expression {expr:?}: {e}"))?;
    let next = cron
        .find_next_occurrence(&from, false)
        .map_err(|e| anyhow!("no next occurrence for {expr:?}: {e}"))?;
    Ok(next)
}

pub fn build_task(payload: &TaskPayload, send_at: &ParsedSendAt) -> Result<NewScheduledTask> {
    let (prompt, channel_id, thread_id) = match payload {
        TaskPayload::Thread { thread_id, prompt, .. } => (prompt, None, Some(thread_id.clone())),
        TaskPayload::Channel { channel_id, prompt, .. } => (prompt, Some(channel_id.clone()), None),
    };
    if prompt.len() > PROMPT_MAX_LEN {
        return Err(anyhow!("prompt exceeds {PROMPT_MAX_LEN} characters"));
    }
    Ok(NewScheduledTask {
        schedule_kind: send_at.schedule_kind.to_string(),
        run_at: send_at.run_at,
        cron_expr: send_at.cron_expr.clone(),
        timezone: send_at.timezone.clone(),
        next_run_at: send_at.next_run_at,
        payload_json: serde_json::to_string(payload)?,
        prompt_preview: rendering::prompt_preview(prompt, 120),
        channel_id,
        thread_id,
    })
}

/// Human-readable task list used by both `/tasks` and `lily task list`.
pub fn describe_task(t: &ScheduledTask) -> String {
    let when = match t.schedule_kind.as_str() {
        "cron" => format!(
            "cron `{}` {} (next {})",
            t.cron_expr.as_deref().unwrap_or("?"),
            t.timezone.as_deref().unwrap_or("UTC"),
            t.next_run_at.format("%Y-%m-%d %H:%M UTC")
        ),
        _ => format!("at {}", t.next_run_at.format("%Y-%m-%d %H:%M UTC")),
    };
    let target = match (&t.thread_id, &t.channel_id) {
        (Some(th), _) => format!(" → thread {th}"),
        (None, Some(ch)) => format!(" → channel {ch}"),
        _ => String::new(),
    };
    let error = match (&t.last_error, t.attempts) {
        (Some(e), n) if n > 0 => format!(" (attempts: {n}, last error: {e})"),
        _ => String::new(),
    };
    let last_run = t
        .last_run_at
        .map(|d| format!(" (last run {})", d.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default();
    format!("#{} [{}] {}{}{} — {}{}", t.id, t.status, when, last_run, target, t.prompt_preview, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_one_time_utc_timestamp() {
        let parsed = parse_send_at("2026-07-01T09:00:00Z", now()).unwrap();
        assert_eq!(parsed.schedule_kind, "at");
        assert_eq!(parsed.next_run_at, Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap());
    }

    #[test]
    fn rejects_past_timestamp() {
        assert!(parse_send_at("2020-01-01T00:00:00Z", now()).is_err());
    }

    #[test]
    fn rejects_non_utc_timestamp() {
        assert!(parse_send_at("2026-07-01T09:00:00+02:00", now()).is_err());
    }

    #[test]
    fn parses_cron_expression() {
        // Every Monday 9am UTC; 2026-06-11 is a Thursday → next is the 15th.
        let parsed = parse_send_at("0 9 * * 1", now()).unwrap();
        assert_eq!(parsed.schedule_kind, "cron");
        assert_eq!(parsed.next_run_at, Utc.with_ymd_and_hms(2026, 6, 15, 9, 0, 0).unwrap());
    }

    #[test]
    fn parses_cron_shortcut() {
        let parsed = parse_send_at("@hourly", now()).unwrap();
        assert_eq!(parsed.schedule_kind, "cron");
        assert_eq!(parsed.next_run_at, Utc.with_ymd_and_hms(2026, 6, 11, 13, 0, 0).unwrap());
    }

    #[test]
    fn build_task_rejects_overlong_prompt() {
        let payload = TaskPayload::Channel {
            channel_id: "1".into(),
            prompt: "x".repeat(PROMPT_MAX_LEN + 1),
            name: None,
            notify_only: false,
            user_id: None,
        };
        assert!(build_task(&payload, &ParsedSendAt::immediately(now())).is_err());
    }
}

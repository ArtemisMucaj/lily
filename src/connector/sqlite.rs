//! SQLite adapter: persists channel↔directory mappings, thread↔session
//! mappings, worktree state, and scheduled tasks. Entities live in `domain`.
//!
//! The queue itself is in-memory (per-thread runtime state); only durable
//! orchestration state lives here.

use crate::domain::task::{NewScheduledTask, ScheduledTask};
use crate::domain::worktree::ThreadWorktree;
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

/// `(schedule_kind, run_at, cron_expr, timezone, next_run_at)` for task edits.
pub type ScheduleUpdate<'a> =
    (&'a str, Option<DateTime<Utc>>, Option<&'a str>, Option<&'a str>, DateTime<Utc>);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS channel_directories (
    channel_id TEXT PRIMARY KEY,
    directory  TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS thread_sessions (
    thread_id  TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS thread_worktrees (
    thread_id          TEXT PRIMARY KEY,
    worktree_name      TEXT NOT NULL,
    worktree_directory TEXT,
    project_directory  TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'pending',
    error_message      TEXT,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS scheduled_tasks (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    status             TEXT NOT NULL DEFAULT 'planned',
    schedule_kind      TEXT NOT NULL,
    run_at             TEXT,
    cron_expr          TEXT,
    timezone           TEXT,
    next_run_at        TEXT NOT NULL,
    running_started_at TEXT,
    last_run_at        TEXT,
    last_error         TEXT,
    attempts           INTEGER NOT NULL DEFAULT 0,
    payload_json       TEXT NOT NULL,
    prompt_preview     TEXT NOT NULL,
    channel_id         TEXT,
    thread_id          TEXT,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_tasks_due ON scheduled_tasks (status, next_run_at);
";

fn ts(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("bad timestamp in db: {s}"))?
        .with_timezone(&Utc))
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        f(&conn)
    }

    // ---- channel directories ----

    pub fn set_channel_directory(&self, channel_id: &str, directory: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO channel_directories (channel_id, directory) VALUES (?1, ?2)
                 ON CONFLICT(channel_id) DO UPDATE SET directory = excluded.directory",
                params![channel_id, directory],
            )?;
            Ok(())
        })
    }

    pub fn get_channel_directory(&self, channel_id: &str) -> Result<Option<String>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT directory FROM channel_directories WHERE channel_id = ?1",
                params![channel_id],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    pub fn list_channel_directories(&self) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT channel_id, directory FROM channel_directories")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    // ---- thread sessions ----

    pub fn set_thread_session(&self, thread_id: &str, session_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO thread_sessions (thread_id, session_id) VALUES (?1, ?2)
                 ON CONFLICT(thread_id) DO UPDATE SET session_id = excluded.session_id",
                params![thread_id, session_id],
            )?;
            Ok(())
        })
    }

    pub fn delete_thread_session(&self, thread_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM thread_sessions WHERE thread_id = ?1", params![thread_id])?;
            Ok(())
        })
    }

    pub fn get_thread_session(&self, thread_id: &str) -> Result<Option<String>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT session_id FROM thread_sessions WHERE thread_id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    // ---- worktrees ----

    pub fn create_pending_worktree(
        &self,
        thread_id: &str,
        worktree_name: &str,
        project_directory: &str,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO thread_worktrees (thread_id, worktree_name, project_directory, status)
                 VALUES (?1, ?2, ?3, 'pending')
                 ON CONFLICT(thread_id) DO UPDATE SET
                   worktree_name = excluded.worktree_name,
                   project_directory = excluded.project_directory,
                   status = 'pending', error_message = NULL, worktree_directory = NULL",
                params![thread_id, worktree_name, project_directory],
            )?;
            Ok(())
        })
    }

    pub fn set_worktree_ready(&self, thread_id: &str, worktree_directory: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE thread_worktrees SET status='ready', worktree_directory=?2, error_message=NULL
                 WHERE thread_id = ?1",
                params![thread_id, worktree_directory],
            )?;
            Ok(())
        })
    }

    pub fn set_worktree_error(&self, thread_id: &str, error: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE thread_worktrees SET status='error', error_message=?2 WHERE thread_id = ?1",
                params![thread_id, error],
            )?;
            Ok(())
        })
    }

    pub fn get_thread_worktree(&self, thread_id: &str) -> Result<Option<ThreadWorktree>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT thread_id, worktree_name, worktree_directory, project_directory, status, error_message
                 FROM thread_worktrees WHERE thread_id = ?1",
                params![thread_id],
                |r| {
                    Ok(ThreadWorktree {
                        thread_id: r.get(0)?,
                        worktree_name: r.get(1)?,
                        worktree_directory: r.get(2)?,
                        project_directory: r.get(3)?,
                        status: r.get(4)?,
                        error_message: r.get(5)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub fn delete_thread_worktree(&self, thread_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM thread_worktrees WHERE thread_id = ?1", params![thread_id])?;
            Ok(())
        })
    }

    pub fn list_worktrees_for_project(&self, project_directory: &str) -> Result<Vec<ThreadWorktree>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT thread_id, worktree_name, worktree_directory, project_directory, status, error_message
                 FROM thread_worktrees WHERE project_directory = ?1",
            )?;
            let rows = stmt
                .query_map(params![project_directory], |r| {
                    Ok(ThreadWorktree {
                        thread_id: r.get(0)?,
                        worktree_name: r.get(1)?,
                        worktree_directory: r.get(2)?,
                        project_directory: r.get(3)?,
                        status: r.get(4)?,
                        error_message: r.get(5)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    // ---- scheduled tasks ----

    pub fn create_scheduled_task(&self, t: &NewScheduledTask) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO scheduled_tasks
                   (schedule_kind, run_at, cron_expr, timezone, next_run_at,
                    payload_json, prompt_preview, channel_id, thread_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    t.schedule_kind,
                    t.run_at.map(ts),
                    t.cron_expr,
                    t.timezone,
                    ts(t.next_run_at),
                    t.payload_json,
                    t.prompt_preview,
                    t.channel_id,
                    t.thread_id,
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    fn task_from_row(
        r: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<(ScheduledTask, Option<String>, String, Option<String>)> {
        // Returns task plus raw run_at / next_run_at / last_run_at strings for
        // later parsing.
        let run_at_raw: Option<String> = r.get(3)?;
        let next_run_raw: String = r.get(6)?;
        let last_run_raw: Option<String> = r.get(7)?;
        Ok((
            ScheduledTask {
                id: r.get(0)?,
                status: r.get(1)?,
                schedule_kind: r.get(2)?,
                run_at: None,
                cron_expr: r.get(4)?,
                timezone: r.get(5)?,
                next_run_at: Utc::now(), // placeholder, replaced by caller
                last_run_at: None,
                last_error: r.get(8)?,
                attempts: r.get(9)?,
                payload_json: r.get(10)?,
                prompt_preview: r.get(11)?,
                channel_id: r.get(12)?,
                thread_id: r.get(13)?,
            },
            run_at_raw,
            next_run_raw,
            last_run_raw,
        ))
    }

    const TASK_COLS: &'static str = "id, status, schedule_kind, run_at, cron_expr, timezone, \
        next_run_at, last_run_at, last_error, attempts, payload_json, prompt_preview, channel_id, thread_id";

    fn finish_task(
        raw: (ScheduledTask, Option<String>, String, Option<String>),
    ) -> Result<ScheduledTask> {
        let (mut t, run_at_raw, next_run_raw, last_run_raw) = raw;
        t.run_at = run_at_raw.as_deref().map(parse_ts).transpose()?;
        t.next_run_at = parse_ts(&next_run_raw)?;
        t.last_run_at = last_run_raw.as_deref().map(parse_ts).transpose()?;
        Ok(t)
    }

    pub fn get_due_planned_tasks(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<ScheduledTask>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM scheduled_tasks
                 WHERE status = 'planned' AND next_run_at <= ?1
                 ORDER BY next_run_at ASC, id ASC LIMIT ?2",
                Self::TASK_COLS
            ))?;
            let rows = stmt
                .query_map(params![ts(now), limit as i64], Self::task_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.into_iter().map(Self::finish_task).collect()
        })
    }

    pub fn list_tasks(&self, include_finished: bool) -> Result<Vec<ScheduledTask>> {
        self.with(|c| {
            let filter = if include_finished {
                ""
            } else {
                "WHERE status IN ('planned','running')"
            };
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM scheduled_tasks {} ORDER BY next_run_at ASC",
                Self::TASK_COLS,
                filter
            ))?;
            let rows = stmt
                .query_map([], Self::task_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.into_iter().map(Self::finish_task).collect()
        })
    }

    /// Atomically claim a planned task for execution. Returns false when the
    /// task was already taken (or cancelled) by someone else.
    pub fn claim_task_running(&self, task_id: i64, started_at: DateTime<Utc>) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE scheduled_tasks SET status='running', running_started_at=?2, updated_at=?2
                 WHERE id = ?1 AND status = 'planned'",
                params![task_id, ts(started_at)],
            )?;
            Ok(n == 1)
        })
    }

    pub fn recover_stale_running_tasks(&self, stale_before: DateTime<Utc>) -> Result<usize> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE scheduled_tasks SET status='planned', running_started_at=NULL
                 WHERE status='running' AND running_started_at <= ?1",
                params![ts(stale_before)],
            )?;
            Ok(n)
        })
    }

    pub fn mark_task_completed(&self, task_id: i64, completed_at: DateTime<Utc>) -> Result<()> {
        self.with(|c| {
            c.execute(
                // Guarded on 'running' so a finishing worker never overwrites
                // a cancellation that landed mid-run.
                "UPDATE scheduled_tasks SET status='completed', last_run_at=?2,
                   running_started_at=NULL, last_error=NULL, updated_at=?2
                 WHERE id = ?1 AND status='running'",
                params![task_id, ts(completed_at)],
            )?;
            Ok(())
        })
    }

    pub fn mark_task_cron_rescheduled(
        &self,
        task_id: i64,
        completed_at: DateTime<Utc>,
        next_run_at: DateTime<Utc>,
        error: Option<&str>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE scheduled_tasks SET status='planned', next_run_at=?3, last_run_at=?2,
                   running_started_at=NULL, last_error=?4,
                   attempts = attempts + (CASE WHEN ?4 IS NULL THEN 0 ELSE 1 END), updated_at=?2
                 WHERE id = ?1 AND status='running'",
                params![task_id, ts(completed_at), ts(next_run_at), error],
            )?;
            Ok(())
        })
    }

    pub fn mark_task_failed(&self, task_id: i64, failed_at: DateTime<Utc>, error: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE scheduled_tasks SET status='failed', last_run_at=?2, last_error=?3,
                   running_started_at=NULL, attempts = attempts + 1, updated_at=?2
                 WHERE id = ?1 AND status='running'",
                params![task_id, ts(failed_at), error],
            )?;
            Ok(())
        })
    }

    pub fn cancel_task(&self, task_id: i64) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE scheduled_tasks SET status='cancelled', running_started_at=NULL
                 WHERE id = ?1 AND status IN ('planned','running')",
                params![task_id],
            )?;
            Ok(n == 1)
        })
    }

    /// Replace the prompt payload and schedule of a still-planned task in one
    /// statement, so a concurrent scheduler claim can never observe (or
    /// preserve) a half-applied edit. Returns false when the task already
    /// started or finished.
    pub fn update_task(
        &self,
        task_id: i64,
        payload_json: &str,
        prompt_preview: &str,
        schedule: ScheduleUpdate<'_>,
    ) -> Result<bool> {
        let (kind, run_at, cron, tz, next) = schedule;
        self.with(|c| {
            let n = c.execute(
                "UPDATE scheduled_tasks SET payload_json=?2, prompt_preview=?3,
                   schedule_kind=?4, run_at=?5, cron_expr=?6, timezone=?7, next_run_at=?8
                 WHERE id = ?1 AND status='planned'",
                params![task_id, payload_json, prompt_preview, kind, run_at.map(ts), cron, tz, ts(next)],
            )?;
            Ok(n == 1)
        })
    }
}

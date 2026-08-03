//! The scheduler, port of `ir.cron` (`odoo/addons/base/models/
//! ir_cron.py`): work the server does on its own, on a clock.
//!
//! A job names a model and one of its methods — the same methods a
//! client calls by name. There is no second kind of code to write for
//! scheduled work, and no second place for it to live.
//!
//! Claiming is what makes this safe: a due job is taken and rescheduled
//! in one statement, with `FOR UPDATE SKIP LOCKED`, so two servers on
//! the same database run it once between them rather than twice each.

use crate::dispatch::OrmService;
use rusdoo_orm::sql::quote_ident;
use serde_json::{Map, Value};
use std::time::Duration;

/// A claimed job: what to run, and what it is called in the log.
struct Job {
    id: i64,
    name: String,
    model: String,
    code: String,
}

/// Claim every job that is due, rescheduling each as it is taken.
///
/// The reschedule happens with the claim, not after the run: a job that
/// panics or takes an hour must not be picked up again by the next tick,
/// which is how a slow job becomes a stampede.
async fn claim_due(service: &OrmService) -> Result<Vec<Job>, sqlx::Error> {
    let Some(model) = service.registry_ref().get("ir.cron") else {
        return Ok(Vec::new());
    };
    let table = match quote_ident(&model.meta.table) {
        Ok(table) => table,
        Err(error) => {
            tracing::error!("ir.cron: {error}");
            return Ok(Vec::new());
        }
    };
    let sql = format!(
        r#"UPDATE {table} AS c
           SET "lastcall" = CURRENT_TIMESTAMP,
               "nextcall" = CURRENT_TIMESTAMP
                   + (COALESCE(GREATEST("interval_number", 1), 1) || ' '
                      || COALESCE("interval_type", 'days'))::interval
           WHERE "id" IN (
               SELECT "id" FROM {table}
               WHERE COALESCE("active", true)
                 AND ("nextcall" IS NULL OR "nextcall" <= CURRENT_TIMESTAMP)
               ORDER BY "nextcall" NULLS FIRST
               FOR UPDATE SKIP LOCKED
           )
           RETURNING c."id", COALESCE(c."name", '') AS "name", c."model", c."code""#
    );
    let rows: Vec<(i32, String, String, String)> =
        sqlx::query_as(&sql).fetch_all(service.pool_ref()).await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, model, code)| Job {
            id: i64::from(id),
            name,
            model,
            code,
        })
        .collect())
}

/// Run the jobs that are due, once.
///
/// Answers how many ran. A job whose method is not registered is logged
/// and skipped: a database row naming code that does not exist is a
/// configuration problem, not a reason to stop the scheduler.
pub async fn run_due(service: &OrmService) -> usize {
    let jobs = match claim_due(service).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!("ir.cron: the queue could not be read: {error}");
            return 0;
        }
    };
    let mut ran = 0;
    for job in jobs {
        if service.method_operation(&job.model, &job.code).is_none() {
            tracing::warn!(
                "ir.cron {}: {}.{} is not a registered method",
                job.name,
                job.model,
                job.code
            );
            continue;
        }
        // scheduled work runs as the superuser: there is no session
        // behind it, and inventing one would put somebody's name on a
        // write they did not make
        let outcome = service
            .call_kw(
                rusdoo_core::SUPERUSER_ID,
                &job.model,
                &job.code,
                &[],
                &Map::new(),
            )
            .await;
        match outcome {
            Ok(value) => {
                ran += 1;
                tracing::debug!("ir.cron {} ({}): {}", job.name, job.id, summarize(&value));
            }
            // one job failing is not the scheduler failing
            Err(error) => tracing::error!("ir.cron {} ({}): {}", job.name, job.id, error.message),
        }
    }
    ran
}

fn summarize(value: &Value) -> String {
    match value {
        Value::Null => "sem retorno".into(),
        other => other.to_string().chars().take(120).collect(),
    }
}

/// Start the scheduler in the background, waking every `tick`.
pub fn spawn(service: OrmService, tick: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            run_due(&service).await;
        }
    })
}

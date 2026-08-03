//! What a pile of ratings says about a record.
//!
//! Port of `rating.mixin`'s statistics (`rating_get_stats`,
//! `rating_get_grades`, `_rating_get_repartition`,
//! `_compute_rating_satisfaction`) and of `rating.parent.mixin`, which
//! asks the same questions one level up.
//!
//! Deviation from Odoo: these are methods, not fields. `rating_avg`,
//! `rating_count` and friends are computed there by a grouped query over
//! another table, and a compute in this ORM is a pure function of the
//! record's own values — it cannot query. The numbers are the same; what
//! is lost is sorting and filtering a list by them.

use crate::data::{self, Stats};
use crate::models::RATING;
use rusdoo_core::RusdooError;
use rusdoo_orm::defaults::DATETIME_FORMAT;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::group::{Aggregate, GroupBy, GroupOptions, COUNT_SPEC};
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};

/// The two ends a record can be counted at: the rated record itself, or
/// the one it hangs from (`rating.parent.mixin`).
fn columns(parent: bool) -> (&'static str, &'static str) {
    if parent {
        ("parent_res_model", "parent_res_id")
    } else {
        ("res_model", "res_id")
    }
}

/// Read the ratings of `ctx.ids` and fold them into the statistics.
///
/// `parent`: count the ratings whose *parent* is these records.
/// `days`: only the last N days, Odoo's `_rating_satisfaction_days`.
async fn gather(
    ctx: &MethodCtx<'_>,
    parent: bool,
    days: Option<i64>,
) -> Result<Stats, RusdooError> {
    if ctx.ids.is_empty() {
        return Ok(data::stats_of(&[]));
    }
    let (model_column, id_column) = columns(parent);
    let mut terms = vec![
        json!([model_column, "=", ctx.model]),
        json!([id_column, "in", ctx.ids]),
        json!(["consumed", "=", true]),
        // a rating below 1 is a request nobody answered; averaging it in
        // would drag every statistic down with silence
        json!(["rating", ">=", data::RATING_LIMIT_MIN]),
    ];
    if let Some(days) = days {
        let since = chrono::Utc::now() - chrono::Duration::days(days);
        terms.push(json!([
            "write_date",
            ">=",
            since.format(DATETIME_FORMAT).to_string()
        ]));
    }
    let domain = parse_domain(&Value::Array(terms))?;
    let model = ctx
        .registry
        .get(RATING)
        .ok_or_else(|| RusdooError::Validation(format!("model {RATING} is not registered")))?;
    let groups = ctx
        .registry
        .read_group(
            ctx.pool,
            RATING,
            &domain,
            &[GroupBy::parse(model, "rating")?],
            &[Aggregate::parse(model, COUNT_SPEC)?],
            &GroupOptions::default(),
        )
        .await?;
    let pairs: Vec<(f64, i64)> = groups
        .iter()
        .filter_map(|group| {
            let value = group.get("rating").and_then(Value::as_f64)?;
            let count = group.get(COUNT_SPEC).and_then(Value::as_i64)?;
            Some((value, count))
        })
        .collect();
    Ok(data::stats_of(&pairs))
}

/// What the caller asked for: which end to count at, and how far back.
fn options(kwargs: &Map<String, Value>) -> (bool, Option<i64>) {
    let parent = kwargs
        .get("parent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let days = kwargs
        .get("days")
        .and_then(Value::as_i64)
        .filter(|days| *days > 0);
    (parent, days)
}

/// A rating value as the key of a JSON object: `5`, or `4.5` when
/// somebody imported half a star.
fn key_of(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// `rating_get_stats` — the average, the total and the repartition.
///
/// The two figures `rating.mixin` publishes as fields travel with them:
/// `avg_text` (`rating_avg_text`) and `percentage_satisfaction`
/// (`rating_percentage_satisfaction`), which have nowhere else to live
/// here.
pub fn rating_get_stats<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let (parent, days) = options(kwargs);
        let stats = gather(&ctx, parent, days).await?;
        let percent: Map<String, Value> = stats
            .percent()
            .into_iter()
            .map(|(value, share)| (key_of(value), json!(share)))
            .collect();
        Ok(json!({
            "total": stats.total,
            "avg": stats.avg,
            "avg_text": stats.avg_text(),
            "percent": percent,
            "percentage_satisfaction": stats.percentage_satisfaction(),
        }))
    })
}

/// `rating_get_grades` — how many great, okay and bad.
pub fn rating_get_grades<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let (parent, days) = options(kwargs);
        let stats = gather(&ctx, parent, days).await?;
        Ok(json!({
            data::GRADE_GREAT: stats.great,
            data::GRADE_OKAY: stats.okay,
            data::GRADE_BAD: stats.bad,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_is_counted_on_its_own_columns() {
        assert_eq!(columns(false), ("res_model", "res_id"));
        assert_eq!(columns(true), ("parent_res_model", "parent_res_id"));
    }

    #[test]
    fn a_whole_value_keys_as_a_whole_number() {
        assert_eq!(key_of(5.0), "5");
        assert_eq!(key_of(4.5), "4.5");
    }

    #[test]
    fn an_age_of_zero_days_is_no_age_at_all() {
        let mut kwargs = Map::new();
        kwargs.insert("days".into(), json!(0));
        assert_eq!(options(&kwargs), (false, None));
        kwargs.insert("days".into(), json!(30));
        kwargs.insert("parent".into(), json!(true));
        assert_eq!(options(&kwargs), (true, Some(30)));
    }
}

//! The small operations every part of this bridge repeats: reading one
//! record, following the link between a sale line and the purchase lines
//! it raised, and saying something on the other document's thread.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::MethodCtx;
use serde_json::{json, Map, Value};

/// The id out of a many2one value, which reads as `[id, name]`.
pub(crate) fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// The name out of a many2one value; empty when the link is unset.
pub(crate) fn linked_name(value: &Value) -> String {
    value
        .as_array()
        .and_then(|items| items.get(1))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A numeric field, whatever shape the driver decoded it in — a
/// `numeric(16,2)` column comes back as a number, but a value that
/// travelled as text must not silently read as zero.
pub(crate) fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// A text field, or the empty string when it is unset.
pub(crate) fn text(record: &Map<String, Value>, name: &str) -> String {
    record
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The ids of an x2many field, as `read` hands it over.
pub(crate) fn id_list(record: &Map<String, Value>, name: &str) -> Vec<i64> {
    record
        .get(name)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Keep the first occurrence of each id, in the order they arrived: a
/// count of documents must not count one of them twice, and a list the
/// user sees must not reshuffle itself between two reads.
pub(crate) fn deduplicated(ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut seen: Vec<i64> = Vec::new();
    for id in ids {
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    seen
}

/// One record, with a complaint that names it when it is not there.
pub(crate) async fn read_one(
    ctx: &MethodCtx<'_>,
    model: &str,
    id: i64,
    fields: &[&str],
) -> Result<Map<String, Value>, RusdooError> {
    ctx.registry
        .read(ctx.pool, model, &[id], fields)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| RusdooError::Validation(format!("{model} {id} does not exist")))
}

/// The purchase lines raised by `sale_lines`, oldest first.
///
/// This is the link the whole module turns on, and it is read from the
/// purchase side on purpose: a sale line that was never bought simply
/// brings nothing back, where a read of `purchase_line_ids` per line
/// would be one query per row.
pub(crate) async fn purchase_lines_of(
    ctx: &MethodCtx<'_>,
    sale_lines: &[i64],
    fields: &[&str],
) -> Result<Vec<Map<String, Value>>, RusdooError> {
    if sale_lines.is_empty() {
        return Ok(Vec::new());
    }
    let domain = parse_domain(&json!([["sale_line_id", "in", sale_lines]]))?;
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "purchase.order.line",
            &domain,
            &SearchOptions {
                order: Some("id".into()),
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    ctx.registry
        .read(ctx.pool, "purchase.order.line", &ids, fields)
        .await
}

/// Say something on a document's thread.
///
/// Odoo schedules a `mail.activity` here, assigned to the document's
/// salesperson and due today. There is no activity model in this port,
/// so the warning is a message on the same thread: the sentence Odoo's
/// template writes still reaches the person who opens the document, but
/// it does not land on anybody's to-do list. A server without `mail` gets
/// no notice at all rather than a failed write — the cancellation itself
/// already happened, and refusing it afterwards would be worse.
pub(crate) async fn post_notice(
    ctx: &MethodCtx<'_>,
    model: &str,
    res_id: i64,
    body: String,
) -> Result<(), RusdooError> {
    if ctx.registry.get("mail.message").is_none() {
        tracing::warn!("no mail.message installed: {model} {res_id} was not warned ({body})");
        return Ok(());
    }
    ctx.registry
        .create_as(
            ctx.pool,
            ctx.uid,
            "mail.message",
            vec![
                ("model", json!(model)),
                ("res_id", json!(res_id)),
                ("body", json!(body)),
                ("message_type", json!("notification")),
                ("author_id", json!(ctx.uid)),
            ],
        )
        .await?;
    Ok(())
}

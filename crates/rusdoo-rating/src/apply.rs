//! The customer flow: asking for a token, answering with it, and what
//! the chatter shows afterwards.
//!
//! Port of `odoo/addons/rating/models/mail_thread.py`
//! (`_rating_get_access_token`, `rating_apply`) and of the two actions
//! `rating.rating` carries itself (`reset`, `action_open_rated_object`).

use crate::data;
use crate::models::{MESSAGE, RATING};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::defaults::DATETIME_FORMAT;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};

/// The id inside a many2one, which reads as `[id, name]`.
pub(crate) fn first_id(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Now, in the format the ORM stores a datetime in.
fn now() -> String {
    chrono::Utc::now().format(DATETIME_FORMAT).to_string()
}

/// The single record a rating method was called on.
fn only_one(ctx: &MethodCtx<'_>, complaint: &str) -> Result<i64, RusdooError> {
    match ctx.ids[..] {
        [id] => Ok(id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

/// An optional id out of the call's keyword arguments.
fn wanted_id(kwargs: &Map<String, Value>, name: &str) -> Option<i64> {
    kwargs.get(name).and_then(Value::as_i64)
}

/// The value of a many2one on a record, when the model has that field at
/// all — the port of Odoo's `'partner_id' in self`.
async fn linked_partner(
    ctx: &MethodCtx<'_>,
    model: &str,
    id: i64,
    field: &str,
) -> Result<Option<i64>, RusdooError> {
    let has_field = ctx
        .registry
        .get(model)
        .and_then(|model| model.field(field))
        .is_some();
    if !has_field {
        return Ok(None);
    }
    let rows = ctx.registry.read(ctx.pool, model, &[id], &[field]).await?;
    Ok(rows.first().and_then(|row| first_id(row.get(field))))
}

/// `_rating_get_partner` — the customer who is asked to rate.
async fn rating_partner(ctx: &MethodCtx<'_>, id: i64) -> Result<Option<i64>, RusdooError> {
    linked_partner(ctx, ctx.model, id, "partner_id").await
}

/// `_rating_get_operator` — the person being rated: the record's
/// salesperson, as a partner.
async fn rating_operator(ctx: &MethodCtx<'_>, id: i64) -> Result<Option<i64>, RusdooError> {
    let Some(user) = linked_partner(ctx, ctx.model, id, "user_id").await? else {
        return Ok(None);
    };
    linked_partner(ctx, "res.users", user, "partner_id").await
}

/// The name a rating records for what it is about, falling back to
/// `model/id` like Odoo's `_compute_res_name`.
async fn name_of(ctx: &MethodCtx<'_>, model: &str, id: i64) -> Result<String, RusdooError> {
    if ctx.registry.get(model).is_none() {
        return Ok(format!("{model}/{id}"));
    }
    let names = ctx.registry.display_names(ctx.pool, model, &[id]).await?;
    Ok(names
        .get(&id)
        .filter(|name| !name.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| format!("{model}/{id}")))
}

/// The pending rating of `partner` on this record, if there is one.
///
/// "Pending" is what makes the token reusable: sending the same customer
/// two requests about the same order must not open two surveys, or the
/// second answer would silently replace the first without anybody saying
/// so.
async fn pending_rating(
    ctx: &MethodCtx<'_>,
    res_id: i64,
    partner: Option<i64>,
) -> Result<Option<i64>, RusdooError> {
    let domain = parse_domain(&json!([
        ["res_model", "=", ctx.model],
        ["res_id", "=", res_id],
        ["consumed", "=", false]
    ]))?;
    let ids = ctx
        .registry
        .search(ctx.pool, RATING, &domain, &SearchOptions::default())
        .await?;
    if ids.is_empty() {
        return Ok(None);
    }
    // the partner is compared here rather than in the domain: a rating
    // with no customer is a NULL column, and "the one with no partner" is
    // not a condition every domain backend spells the same way
    let rows = ctx
        .registry
        .read(ctx.pool, RATING, &ids, &["partner_id"])
        .await?;
    Ok(rows
        .iter()
        .find(|row| first_id(row.get("partner_id")) == partner)
        .and_then(|row| row.get("id").and_then(Value::as_i64)))
}

/// `_rating_get_access_token` — the token the customer's link carries.
///
/// Odoo checks read access on the record and then works as superuser,
/// because the caller may be a mail template rendered for somebody who
/// cannot see ratings. Here the method is declared as a `Read` on the
/// rated model, which is the same statement made by the dispatcher.
pub fn rating_get_access_token<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let res_id = only_one(&ctx, "ask for the token of one record at a time")?;
        let partner = match wanted_id(kwargs, "partner_id") {
            Some(partner) => Some(partner),
            None => rating_partner(&ctx, res_id).await?,
        };
        if let Some(existing) = pending_rating(&ctx, res_id, partner).await? {
            return Ok(json!(token_of(&ctx, existing).await?));
        }

        let rated_partner = match wanted_id(kwargs, "rated_partner_id") {
            Some(operator) => Some(operator),
            None => rating_operator(&ctx, res_id).await?,
        };
        let mut values: Vec<(&str, Value)> = vec![
            ("res_model", json!(ctx.model)),
            ("res_id", json!(res_id)),
            ("res_name", json!(name_of(&ctx, ctx.model, res_id).await?)),
            ("partner_id", json!(partner)),
            ("rated_partner_id", json!(rated_partner)),
            // a rating asked of a customer is never an internal note
            ("is_internal", json!(false)),
        ];
        // the parent is given, not discovered: Odoo asks the record
        // through `_rating_get_parent_field_name`, a hook that belongs to
        // a mixin this ORM has no way to express
        if let Some(parent_model) = kwargs.get("parent_res_model").and_then(Value::as_str) {
            let parent_id = wanted_id(kwargs, "parent_res_id").ok_or_else(|| {
                RusdooError::Validation(
                    "parent_res_model was given without parent_res_id: say which record".into(),
                )
            })?;
            values.push(("parent_res_model", json!(parent_model)));
            values.push(("parent_res_id", json!(parent_id)));
            values.push((
                "parent_res_name",
                json!(name_of(&ctx, parent_model, parent_id).await?),
            ));
        }
        let rating = ctx
            .registry
            .create_as(ctx.pool, ctx.uid, RATING, values)
            .await?;
        Ok(json!(token_of(&ctx, rating).await?))
    })
}

/// The token of a rating, read back from the row that was just written —
/// it is the ORM's default that produced it, not this code.
async fn token_of(ctx: &MethodCtx<'_>, rating: i64) -> Result<String, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, RATING, &[rating], &["access_token"])
        .await?;
    rows.first()
        .and_then(|row| row.get("access_token"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RusdooError::Missing(format!("rating {rating} has no access token")))
}

/// The rating a call names: by token (the customer's link) or by id (a
/// module applying one it already holds).
async fn wanted_rating(
    ctx: &MethodCtx<'_>,
    kwargs: &Map<String, Value>,
) -> Result<i64, RusdooError> {
    if let Some(token) = kwargs.get("token").and_then(Value::as_str) {
        let domain = parse_domain(&json!([["access_token", "=", token]]))?;
        let found = ctx
            .registry
            .search(
                ctx.pool,
                RATING,
                &domain,
                &SearchOptions {
                    limit: Some(1),
                    ..SearchOptions::default()
                },
            )
            .await?;
        return found
            .first()
            .copied()
            // the same sentence for a token that never existed and for one
            // that no longer does: telling them apart is telling a stranger
            // which tokens are real
            .ok_or_else(|| RusdooError::Validation("Invalid token or rating.".into()));
    }
    wanted_id(kwargs, "rating_id")
        .ok_or_else(|| RusdooError::Validation("Invalid token or rating.".into()))
}

/// `rating_apply(rate, token=, rating_id=, feedback=)` — the answer.
///
/// Called on the rated record, like Odoo's, so that the message it posts
/// lands on the right chatter.
pub fn rating_apply<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let res_id = only_one(&ctx, "apply a rating to one record at a time")?;
        let rate = kwargs
            .get("rate")
            .or_else(|| ctx.rest.first())
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                RusdooError::Validation("say which rating to apply: pass rate".into())
            })?;
        if !data::is_rating_value(rate) {
            return Err(RusdooError::Validation(format!(
                "Wrong rating value. A rate should be between 0 and 5 (received {rate})."
            )));
        }
        let feedback = kwargs
            .get("feedback")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|feedback| !feedback.is_empty())
            .map(str::to_string);

        let rating = wanted_rating(&ctx, kwargs).await?;
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                RATING,
                &[rating],
                &["res_model", "res_id", "message_id", "partner_id"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("Invalid token or rating.".into()))?;
        // not in Odoo, where the controller looks the record up *from* the
        // rating and cannot disagree with it. Here the caller names both,
        // and a rating applied to a record it does not belong to would
        // move somebody else's opinion onto this one.
        let belongs = row.get("res_model").and_then(Value::as_str) == Some(ctx.model)
            && row.get("res_id").and_then(Value::as_i64) == Some(res_id);
        if !belongs {
            return Err(RusdooError::Validation(
                "that rating belongs to another record".into(),
            ));
        }

        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                RATING,
                &[rating],
                vec![
                    ("rating", json!(rate)),
                    ("feedback", json!(feedback)),
                    ("consumed", json!(true)),
                    // Odoo stamps this from its `create`/`write` overrides;
                    // this ORM has no such hook, so every path that writes
                    // a value stamps it here
                    ("rated_on", json!(now())),
                ],
            )
            .await?;

        let body = chatter_body(rate, feedback.as_deref());
        match first_id(row.get("message_id")) {
            // Odoo edits the message it already posted rather than saying
            // it twice: a customer who changes their mind leaves one trace
            Some(message) => {
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        MESSAGE,
                        &[message],
                        vec![("body", json!(body))],
                    )
                    .await?;
            }
            None => {
                let message = ctx
                    .registry
                    .create_as(
                        ctx.pool,
                        ctx.uid,
                        MESSAGE,
                        vec![
                            ("model", json!(ctx.model)),
                            ("res_id", json!(res_id)),
                            ("body", json!(body)),
                            ("message_type", json!("comment")),
                            // deviation: Odoo credits the customer
                            // (`author_id` is a partner there); a message
                            // here is authored by a *user*, and inventing
                            // one for a customer would be inventing a login
                            ("author_id", json!(ctx.uid)),
                        ],
                    )
                    .await?;
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        RATING,
                        &[rating],
                        vec![("message_id", json!(message))],
                    )
                    .await?;
            }
        }
        Ok(json!(rating))
    })
}

/// What the chatter says about a rating.
///
/// Odoo writes an `<img>` pointing at the face plus the feedback as HTML.
/// A message body here is stored as written and escaped when rendered, so
/// markup would show up as markup; the same three facts are said in text.
pub(crate) fn chatter_body(rate: f64, feedback: Option<&str>) -> String {
    let text = data::RATING_TEXT
        .iter()
        .find(|(value, _)| *value == data::rating_to_text(rate))
        .map_or("", |(_, label)| label);
    let rate = if rate.fract() == 0.0 {
        format!("{rate:.0}")
    } else {
        format!("{rate}")
    };
    match feedback {
        Some(feedback) => format!("Rating: {rate}/5 ({text}) — {feedback}"),
        None => format!("Rating: {rate}/5 ({text})"),
    }
}

/// `reset` — the rating goes back to being a question.
///
/// A new token comes with it: the old link was in a mail somebody may
/// still have, and a reset survey answered through the old link would be
/// an answer nobody asked for.
pub fn reset<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "choose at least one rating to reset".into(),
            ));
        }
        // one write per rating, not one write over all of them: they may
        // not share a token, and a batch write would hand the same one to
        // every customer on the list
        for rating in &ctx.ids {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    RATING,
                    &[*rating],
                    vec![
                        ("rating", json!(0)),
                        (
                            "access_token",
                            json!(uuid::Uuid::new_v4().simple().to_string()),
                        ),
                        ("feedback", Value::Null),
                        ("consumed", json!(false)),
                        // Odoo does not write this here — its `write`
                        // override stamps `rated_on` whenever `rating`
                        // or `feedback` is touched (rating.py:137). The
                        // port has no write override, so the stamp has
                        // to be explicit or a reset would leave the old
                        // date next to a cleared rating: a record that
                        // claims to have been rated on a day when it
                        // was not.
                        (
                            "rated_on",
                            json!(chrono::Utc::now()
                                .format(rusdoo_orm::defaults::DATETIME_FORMAT)
                                .to_string()),
                        ),
                    ],
                )
                .await?;
        }
        Ok(json!(true))
    })
}

/// `action_open_rated_object` — open what was rated.
pub fn action_open_rated_object<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let rating = only_one(&ctx, "open the record of one rating at a time")?;
        let rows = ctx
            .registry
            .read(ctx.pool, RATING, &[rating], &["res_model", "res_id"])
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Missing(format!("rating {rating} does not exist")))?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "res_model": row.get("res_model").cloned().unwrap_or(Value::Null),
            "res_id": row.get("res_id").cloned().unwrap_or(Value::Null),
            "views": [[false, "form"]],
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chatter_says_the_value_the_face_and_what_was_written() {
        assert_eq!(
            chatter_body(5.0, Some("fast delivery")),
            "Rating: 5/5 (Happy) — fast delivery"
        );
        // no feedback is not an empty sentence with a dash hanging off it
        assert_eq!(chatter_body(1.0, None), "Rating: 1/5 (Unhappy)");
        assert_eq!(chatter_body(3.0, None), "Rating: 3/5 (Neutral)");
        // and a half star still reads as a number
        assert_eq!(chatter_body(4.5, None), "Rating: 4.5/5 (Happy)");
    }
}

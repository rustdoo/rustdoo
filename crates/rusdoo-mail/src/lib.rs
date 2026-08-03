//! rusdoo-mail — port of `odoo/addons/mail/models/`: the chatter.
//!
//! In Odoo every business record carries a discussion, and `mail.thread`
//! is the mixin that gives it one. There is no mixin here yet, so the
//! same two methods are attached to each model that wants a thread —
//! declared by the module that owns the model, which is the same
//! decision `_inherit = ['mail.thread']` makes, spelled differently.
//!
//! Deviation from Odoo: a message's author is a `res.users`, not a
//! `res.partner`. The port has users logging in and partners being
//! written about; pointing at the partner would mean inventing one for
//! every user, and inventing records is how a system starts lying.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// How many messages a fetch returns when the caller does not say.
const DEFAULT_FETCH_LIMIT: u64 = 30;

/// The hard cap on one fetch: the thread of a busy record is unbounded,
/// and a client asking for all of it is a query nobody meant to run.
const MAX_FETCH_LIMIT: u64 = 200;

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(message())?;
    reg.register(follower())?;
    Ok(())
}

/// Attach a thread to each of `thread_models`: the two methods a chatter
/// calls. A model named here must exist in the registry — a chatter on a
/// model nobody registered would fail at the first click instead of at
/// boot.
pub fn extend_methods(
    methods: &mut MethodRegistry,
    thread_models: &[&str],
) -> Result<(), RusdooError> {
    for model in thread_models {
        // posting is not editing: a colleague may comment on an order
        // they may read, which is what Odoo's chatter does too
        methods.register(model, "message_post", Operation::Read, message_post)?;
        methods.register(model, "message_fetch", Operation::Read, message_fetch)?;
    }
    Ok(())
}

/// `mail.message` — one thing somebody said about one record.
fn message() -> Model {
    Model::new(
        ModelMeta {
            name: "mail.message".into(),
            table: "mail_message".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            // which record the message is about (Odoo's model/res_id pair)
            Field::new("model", FieldType::Char { size: None }).required(),
            Field::new("res_id", FieldType::Integer).required(),
            Field::new("subject", FieldType::Char { size: None }),
            Field::new("body", FieldType::Text),
            Field::new(
                "message_type",
                FieldType::Selection(vec![
                    ("comment".into(), "Comment".into()),
                    ("notification".into(), "Notification".into()),
                ]),
            )
            .default_value(json!("comment")),
            Field::new(
                "author_id",
                FieldType::Many2one {
                    comodel: "res.users".into(),
                },
            ),
        ],
    )
    // a conversa mais recente no topo, como o chatter desenha
    .ordered("id desc")
}

/// `mail.followers` — who hears about a record.
fn follower() -> Model {
    Model::new(
        ModelMeta {
            name: "mail.followers".into(),
            table: "mail_followers".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("res_model", FieldType::Char { size: None }).required(),
            Field::new("res_id", FieldType::Integer).required(),
            Field::new(
                "partner_id",
                FieldType::Many2one {
                    comodel: "res.partner".into(),
                },
            ),
            Field::new(
                "user_id",
                FieldType::Many2one {
                    comodel: "res.users".into(),
                },
            ),
        ],
    )
}

/// `message_post(body=..., subject=...)` — say something about a record.
fn message_post<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [res_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "message_post speaks about one record at a time".into(),
            ));
        };
        let body = kwargs
            .get("body")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|body| !body.is_empty())
            .ok_or_else(|| RusdooError::Validation("the message is empty".into()))?;
        let mut values = vec![
            ("model", json!(ctx.model)),
            ("res_id", json!(res_id)),
            // the body is stored as written and escaped when rendered:
            // storing it "sanitized" would be a lie about what was said
            ("body", json!(body)),
            ("message_type", json!("comment")),
            ("author_id", json!(ctx.uid)),
        ];
        if let Some(subject) = kwargs
            .get("subject")
            .and_then(Value::as_str)
            .filter(|subject| !subject.trim().is_empty())
        {
            values.push(("subject", json!(subject)));
        }
        let id = ctx
            .registry
            .create_as(ctx.pool, ctx.uid, "mail.message", values)
            .await?;
        Ok(json!(id))
    })
}

/// `message_fetch(limit=...)` — the thread of a record, newest first,
/// with the author's name resolved so a chatter is one round trip.
fn message_fetch<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [res_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "message_fetch reads a record's history".into(),
            ));
        };
        let limit = kwargs
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_FETCH_LIMIT)
            .min(MAX_FETCH_LIMIT);
        let domain = parse_domain(&json!([
            ["model", "=", ctx.model],
            ["res_id", "=", res_id]
        ]))?;
        let ids = ctx
            .registry
            .search(
                ctx.pool,
                "mail.message",
                &domain,
                &SearchOptions {
                    limit: Some(limit),
                    // newest first: a chatter is read from the top
                    order: Some("id desc".into()),
                    ..SearchOptions::default()
                },
            )
            .await?;
        if ids.is_empty() {
            return Ok(json!([]));
        }
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "mail.message",
                &ids,
                &["subject", "body", "message_type", "author_id", "create_date"],
            )
            .await?;
        let authors: Vec<i64> = rows
            .iter()
            .filter_map(|row| row.get("author_id").and_then(first_id))
            .collect();
        let names = ctx
            .registry
            .display_names(ctx.pool, "res.users", &authors)
            .await?;
        let messages: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let author = row.get("author_id").and_then(first_id);
                json!({
                    "id": row.get("id").cloned().unwrap_or(Value::Null),
                    "subject": row.get("subject").cloned().unwrap_or(Value::Null),
                    "body": row.get("body").cloned().unwrap_or(Value::Null),
                    "message_type": row.get("message_type").cloned().unwrap_or(Value::Null),
                    "date": row.get("create_date").cloned().unwrap_or(Value::Null),
                    "author": author
                        .and_then(|id| names.get(&id).cloned())
                        .unwrap_or_else(|| "Sistema".to_string()),
                })
            })
            .collect();
        Ok(json!(messages))
    })
}

/// The id out of a many2one value, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        assert!(reg.get("mail.message").is_some());
        assert!(reg.get("mail.followers").is_some());
    }

    #[test]
    fn a_thread_model_gets_both_methods() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods, &["sale.order", "res.partner"]).unwrap();
        assert_eq!(
            methods.names_for("sale.order"),
            vec!["message_fetch", "message_post"]
        );
        assert_eq!(
            methods.get("res.partner", "message_post").unwrap().operation,
            Operation::Read,
            "commenting is not editing"
        );
    }

    #[test]
    fn a_many2one_reads_as_its_id() {
        assert_eq!(first_id(&json!([7, "Ana"])), Some(7));
        assert_eq!(first_id(&json!(7)), Some(7));
        assert_eq!(first_id(&Value::Null), None);
    }
}

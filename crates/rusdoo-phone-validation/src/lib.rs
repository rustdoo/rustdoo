//! rusdoo-phone-validation — port of `odoo/addons/phone_validation/`:
//! reading a phone number, and refusing to call one.
//!
//! Two jobs, and they are less separate than they look.
//!
//! The first is [`phone`]: turn whatever somebody typed into the number
//! they meant. `0456998877` on a Belgian screen and `+32 456 99 88 77`
//! on a French one are the same telephone, and a system that stores them
//! as two strings has two customers where it has one.
//!
//! The second is the **blacklist**: the numbers this database has been
//! asked to stop contacting. It only works because of the first. Matching
//! a blacklist is string equality — it has to be, or every campaign would
//! run a parser over every row — so the numbers on both sides must be
//! written the same way, always, which is why every number entering the
//! blacklist is put in E164 form first and why a raw one is refused.
//!
//! ## What this port does differently, and why
//!
//! * **No `mail.thread.phone` model.** Odoo ships the phone fields as a
//!   mixin. This ORM has no mixins, so the fields are handed out by
//!   [`phone_mixin_fields`] and the methods by [`extend_phone_methods`] —
//!   the same decision `_inherit = ['mail.thread.phone']` makes, spelled
//!   as a function call. `res.partner` gets both here, as in Odoo.
//! * **`phone_sanitized` is a stored compute, the blacklist flags are
//!   methods.** Odoo computes `phone_sanitized_blacklisted` from a query
//!   against `phone.blacklist`; a compute here is a pure function of the
//!   record's own fields and cannot ask the database. The answer is
//!   served by `phone_blacklist_state` instead.
//! * **The blacklist sanitizes in its methods, not in `create`.** Odoo
//!   overrides `create`/`write` to sanitize whatever is written. There is
//!   no create/write hook in this ORM, so `add`/`remove` sanitize, and a
//!   constraint refuses any number written straight into the model that
//!   is not already E164 — it cannot fix the value, but it can stop the
//!   database quietly holding a number no lookup will ever match.

pub mod metadata;
pub mod phone;

pub use phone::{
    phone_format, phone_get_country_code_for_number, phone_get_region_data_for_number, phone_parse,
    Format, PhoneNumber, RegionData,
};

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The fields a record may carry a number in, in the order Odoo tries
/// them (`BaseModel._phone_get_number_fields`). Mobile first: it is the
/// number an SMS goes to, and the one a person actually answers.
pub const NUMBER_FIELDS: [&str; 2] = ["mobile", "phone"];

/// What `phone_sanitized` watches. The country matters as much as the
/// digits do — the same `0456998877` is a different telephone depending
/// on which country is reading it.
const SANITIZE_TRIGGERS: [&str; 4] = [
    "mobile",
    "phone",
    "country_id.code",
    "country_id.phone_code",
];

fn char_field(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The id out of a many2one value, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(blacklist())?;
    reg.register(blacklist_remove())?;
    reg.register(phone_partner())?;
    Ok(())
}

/// The blacklist's buttons, and the phone mixin's methods on the one
/// model this addon puts them on.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // adding and removing a number is a write on the blacklist; opening
    // the dialog only reads
    methods.register("phone.blacklist", "add", Operation::Write, action_add)?;
    methods.register("phone.blacklist", "remove", Operation::Write, action_remove)?;
    methods.register(
        "phone.blacklist",
        "action_add",
        Operation::Write,
        action_add_self,
    )?;
    methods.register(
        "phone.blacklist",
        "phone_action_blacklist_remove",
        Operation::Read,
        open_unblacklist_dialog,
    )?;
    methods.register(
        "phone.blacklist.remove",
        "action_unblacklist_apply",
        Operation::Write,
        action_unblacklist_apply,
    )?;
    extend_phone_methods(methods, &["res.partner"])
}

/// Give another module's model the phone mixin's methods — the port's
/// spelling of `_inherit = ['mail.thread.phone']`, in the same shape
/// `rusdoo_mail::extend_methods` uses for the chatter.
///
/// The model must have been given [`phone_mixin_fields`] too; a model
/// with the methods and not the fields answers every question with
/// nothing.
pub fn extend_phone_methods(
    methods: &mut MethodRegistry,
    models: &[&str],
) -> Result<(), RusdooError> {
    for model in models {
        // formatting and asking whether a number is blocked are reads;
        // blacklisting from a record writes the blacklist, and the record
        // it is asked from is only read
        methods.register(model, "phone_format", Operation::Read, record_phone_format)?;
        methods.register(
            model,
            "phone_blacklist_state",
            Operation::Read,
            phone_blacklist_state,
        )?;
        methods.register(
            model,
            "phone_set_blacklisted",
            Operation::Read,
            phone_set_blacklisted,
        )?;
        methods.register(
            model,
            "phone_reset_blacklisted",
            Operation::Read,
            phone_reset_blacklisted,
        )?;
        methods.register(
            model,
            "phone_action_blacklist_remove",
            Operation::Read,
            open_unblacklist_dialog_for_record,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------

/// The fields `mail.thread.phone` adds to a model that carries a phone
/// number.
///
/// Only `phone_sanitized` survives the port as a field. Odoo's
/// `phone_sanitized_blacklisted` and `phone_blacklisted` are computes
/// that query `phone.blacklist`, which a pure compute cannot do here;
/// `phone_blacklist_state` answers the same question as a method.
/// `phone_mobile_search` is a search-only field with no counterpart in
/// this ORM at all.
pub fn phone_mixin_fields() -> Vec<Field> {
    vec![
        // materialised on purpose: this is the column every campaign and
        // every blacklist lookup joins on, and recomputing it per row
        // would turn one indexed comparison into a parse per contact
        char_field("phone_sanitized")
            .computed(&SANITIZE_TRIGGERS, compute_phone_sanitized)
            .store(),
    ]
}

/// `phone.blacklist` — the numbers this database stops contacting.
fn blacklist() -> Model {
    Model::new(
        meta("phone.blacklist", "phone_blacklist"),
        vec![
            char_field("number").required(),
            // a number taken off the blacklist is archived, never
            // deleted: the row is the record that somebody asked, and
            // when
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    // the newest block at the top, which is what somebody looking at this
    // screen came to see
    .ordered("id desc")
    // Odoo's `_unique_number`. In the database and not in Rust: two
    // requests blacklisting the same number at once would both find
    // nothing and both insert.
    .sql_constrained(
        "phone_blacklist_number_uniq",
        r#"UNIQUE ("number")"#,
        "that number is already on the blacklist",
    )
    .constrained("number in E164", &["number"], is_sanitized)
}

/// `phone.blacklist.remove` — the dialog that asks why.
///
/// Transient, like Odoo's: the row is a dialog somebody has open. The
/// reason it collects is the whole point — a number coming off the
/// blacklist without one is a decision nobody can review later.
fn blacklist_remove() -> Model {
    Model::new(
        meta("phone.blacklist.remove", "phone_blacklist_remove"),
        vec![
            // not `readonly` here, unlike Odoo: in this ORM readonly
            // forbids the write, and the dialog is created carrying the
            // number. The form arch is what keeps it out of reach.
            char_field("phone").required(),
            char_field("reason"),
        ],
    )
    .transient()
}

/// `res.partner` extended, as `models/res_partner.py` does.
fn phone_partner() -> Model {
    Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec!["res.partner".into()],
            inherits: vec![],
        },
        phone_mixin_fields(),
    )
}

// ---------------------------------------------------------------------
// The compute, and the rule the blacklist enforces
// ---------------------------------------------------------------------

/// A value reached through a many2one dependency, which the ORM hands
/// over as a one-element list.
fn through_many2one(record: &Map<String, Value>, path: &str) -> Option<String> {
    match record.get(path)? {
        Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_string),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// `phone_sanitized` — the record's number in E164, or nothing.
///
/// Port of `_compute_phone_sanitized`: the first field that yields a
/// number wins, and a number that cannot be read leaves the column empty
/// rather than storing something a lookup would never match.
///
/// Deviation: Odoo falls back to the company's country when the record
/// has none (`BaseModel._phone_format`). A compute here sees only the
/// record's own fields, so a record with no country can only be
/// sanitized when its number is written in full, with the `+`.
fn compute_phone_sanitized(record: &Map<String, Value>) -> Value {
    let country = through_many2one(record, "country_id.code");
    let phone_code = through_many2one(record, "country_id.phone_code")
        .and_then(|code| code.trim().parse::<u32>().ok());
    for name in NUMBER_FIELDS {
        let Some(number) = record
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|number| !number.is_empty())
        else {
            continue;
        };
        if let Ok(sanitized) = phone::phone_format(
            number,
            country.as_deref(),
            phone_code,
            Format::E164,
            true,
        ) {
            return json!(sanitized);
        }
    }
    Value::Null
}

/// A blacklist row whose number is not in E164 form.
///
/// It cannot sanitize — a constraint answers yes or no — but refusing is
/// the point: the blacklist is matched by exact string, so a row holding
/// `04 56 99 88 77` blocks nothing at all while looking like it does.
fn is_sanitized(record: &Map<String, Value>) -> Result<(), String> {
    let Some(number) = record
        .get("number")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|number| !number.is_empty())
    else {
        return Err("a blacklist entry without a number blocks nobody".into());
    };
    match phone::phone_parse(number, None) {
        Ok(parsed) => {
            let sanitized = phone::format(&parsed, Format::E164);
            if sanitized == number {
                Ok(())
            } else {
                Err(format!(
                    "{number} is not in E164 form: write {sanitized}. The blacklist is matched \
                     string against string, so two spellings of one number block only one of them"
                ))
            }
        }
        Err(error) => Err(format!(
            "{number} cannot go on the blacklist: {}",
            strip_kind(&error.to_string())
        )),
    }
}

/// The message out of a `RusdooError`'s display form, without the
/// `"user error: "` the enum prefixes it with — a constraint's text is
/// shown to somebody fixing a form, not to somebody reading a log.
fn strip_kind(rendered: &str) -> &str {
    rendered.split_once(": ").map_or(rendered, |(_, rest)| rest)
}

// ---------------------------------------------------------------------
// Reading a number the way the acting user would
// ---------------------------------------------------------------------

/// The country a number typed by somebody is read against.
#[derive(Debug, Clone, Default)]
struct Country {
    code: Option<String>,
    phone_code: Option<u32>,
}

async fn country_of(ctx: &MethodCtx<'_>, country_id: Option<i64>) -> Country {
    let Some(country_id) = country_id else {
        return Country::default();
    };
    let Ok(rows) = ctx
        .registry
        .read(ctx.pool, "res.country", &[country_id], &["code", "phone_code"])
        .await
    else {
        return Country::default();
    };
    let Some(row) = rows.first() else {
        return Country::default();
    };
    Country {
        code: row
            .get("code")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(str::to_string),
        phone_code: row
            .get("phone_code")
            .and_then(Value::as_str)
            .and_then(|code| code.trim().parse::<u32>().ok()),
    }
}

/// The country the acting user's numbers are read against — Odoo's
/// `self.env.user._phone_format`, which walks user → partner → country.
///
/// Deviation: Odoo falls back to `self.env.company.country_id`, and
/// `res.company` in this port has no country at all. A user with no
/// country on their partner must therefore write numbers in full.
async fn caller_country(ctx: &MethodCtx<'_>) -> Country {
    let Ok(users) = ctx
        .registry
        .read(ctx.pool, "res.users", &[ctx.uid], &["partner_id"])
        .await
    else {
        return Country::default();
    };
    let partner = users.first().and_then(|row| {
        row.get("partner_id")
            .and_then(first_id)
    });
    let Some(partner) = partner else {
        return Country::default();
    };
    let Ok(partners) = ctx
        .registry
        .read(ctx.pool, "res.partner", &[partner], &["country_id"])
        .await
    else {
        return Country::default();
    };
    let country = partners
        .first()
        .and_then(|row| row.get("country_id").and_then(first_id));
    country_of(ctx, country).await
}

/// A number in the form the blacklist stores, or the reason it is not one.
fn sanitize(number: &str, country: &Country, want: Format) -> Result<String, RusdooError> {
    phone::phone_format(
        number,
        country.code.as_deref(),
        country.phone_code,
        want,
        true,
    )
    .map_err(|error| {
        RusdooError::User(format!(
            "{} Please correct the number and try again.",
            strip_kind(&error.to_string())
        ))
    })
}

/// The number a caller named: `number=` in the kwargs, or the first
/// positional argument after the recordset.
fn wanted_number(ctx: &MethodCtx<'_>, kwargs: &Map<String, Value>) -> Result<String, RusdooError> {
    kwargs
        .get("number")
        .or_else(|| ctx.rest.first())
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|number| !number.is_empty())
        .map(str::to_string)
        .ok_or_else(|| RusdooError::User("say which number: pass `number`".into()))
}

// ---------------------------------------------------------------------
// The blacklist itself
// ---------------------------------------------------------------------

/// The blacklist rows for these numbers, archived ones included: a number
/// taken off the list still has its row, and putting it back on means
/// reviving that row rather than inserting a second one the unique
/// constraint would refuse.
async fn existing_rows(
    ctx: &MethodCtx<'_>,
    numbers: &[String],
) -> Result<Vec<Map<String, Value>>, RusdooError> {
    let domain = parse_domain(&json!([["number", "in", numbers]]))?;
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "phone.blacklist",
            &domain,
            &SearchOptions {
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    ctx.registry
        .read(ctx.pool, "phone.blacklist", &ids, &["number", "active"])
        .await
}

/// Say in a blacklist entry's chatter why it is there.
///
/// Odoo's `_add(message=...)`. It is skipped when `mail` is not
/// installed rather than required: a database without a chatter must
/// still be able to stop contacting somebody.
async fn log_reason(ctx: &MethodCtx<'_>, id: i64, body: &str) -> Result<(), RusdooError> {
    if ctx.registry.get("mail.message").is_none() || body.is_empty() {
        return Ok(());
    }
    ctx.registry
        .create_as(
            ctx.pool,
            ctx.uid,
            "mail.message",
            vec![
                ("model", json!("phone.blacklist")),
                ("res_id", json!(id)),
                ("body", json!(body)),
                ("message_type", json!("notification")),
                ("author_id", json!(ctx.uid)),
            ],
        )
        .await?;
    Ok(())
}

/// Put sanitized numbers on the blacklist, port of `_add`.
///
/// Creating a number that is already there is not an error: it answers
/// with the row that already exists, and revives it if it had been taken
/// off. Anything else would make "block this person" fail depending on
/// whether they had ever been unblocked.
async fn blacklist_numbers(
    ctx: &MethodCtx<'_>,
    numbers: &[String],
    reason: &str,
) -> Result<Vec<i64>, RusdooError> {
    let existing = existing_rows(ctx, numbers).await?;
    let mut ids: Vec<i64> = Vec::with_capacity(numbers.len());
    for number in numbers {
        let found = existing.iter().find(|row| {
            row.get("number")
                .and_then(Value::as_str)
                .is_some_and(|stored| stored == number)
        });
        match found {
            Some(row) => {
                let id = row
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| RusdooError::Database("a blacklist row with no id".into()))?;
                if !row.get("active").and_then(Value::as_bool).unwrap_or(true) {
                    ctx.registry
                        .write_as(
                            ctx.pool,
                            ctx.uid,
                            "phone.blacklist",
                            &[id],
                            vec![("active", json!(true))],
                        )
                        .await?;
                    log_reason(ctx, id, reason).await?;
                }
                ids.push(id);
            }
            None => {
                let id = ctx
                    .registry
                    .create_as(
                        ctx.pool,
                        ctx.uid,
                        "phone.blacklist",
                        vec![("number", json!(number)), ("active", json!(true))],
                    )
                    .await?;
                log_reason(ctx, id, reason).await?;
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Take sanitized numbers off the blacklist, port of `_remove`.
///
/// A number that was never on it still gets a row, archived. That looks
/// odd until you need it: it is the record that somebody asked to be
/// contactable, and without it the request leaves no trace at all.
async fn unblacklist_numbers(
    ctx: &MethodCtx<'_>,
    numbers: &[String],
    reason: &str,
) -> Result<Vec<i64>, RusdooError> {
    let existing = existing_rows(ctx, numbers).await?;
    let mut ids: Vec<i64> = Vec::with_capacity(numbers.len());
    for number in numbers {
        let found = existing.iter().find(|row| {
            row.get("number")
                .and_then(Value::as_str)
                .is_some_and(|stored| stored == number)
        });
        let id = match found {
            Some(row) => {
                let id = row
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| RusdooError::Database("a blacklist row with no id".into()))?;
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "phone.blacklist",
                        &[id],
                        vec![("active", json!(false))],
                    )
                    .await?;
                id
            }
            None => {
                ctx.registry
                    .create_as(
                        ctx.pool,
                        ctx.uid,
                        "phone.blacklist",
                        vec![("number", json!(number)), ("active", json!(false))],
                    )
                    .await?
            }
        };
        log_reason(ctx, id, reason).await?;
        ids.push(id);
    }
    Ok(ids)
}

/// `add(number)` — block a number.
fn action_add<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let number = wanted_number(&ctx, kwargs)?;
        let country = caller_country(&ctx).await;
        let sanitized = sanitize(&number, &country, Format::E164)?;
        let reason = kwargs
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ids = blacklist_numbers(&ctx, &[sanitized], reason).await?;
        Ok(json!(ids))
    })
}

/// `remove(number)` — let a number be contacted again.
fn action_remove<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let number = wanted_number(&ctx, kwargs)?;
        let country = caller_country(&ctx).await;
        let sanitized = sanitize(&number, &country, Format::E164)?;
        let reason = kwargs
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ids = unblacklist_numbers(&ctx, &[sanitized], reason).await?;
        Ok(json!(ids))
    })
}

/// `action_add` — the button on an archived blacklist entry: block this
/// number again.
fn action_add_self<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::User("choose an entry to blacklist".into()));
        }
        let rows = ctx
            .registry
            .read(ctx.pool, "phone.blacklist", &ctx.ids, &["number"])
            .await?;
        let numbers: Vec<String> = rows
            .iter()
            .filter_map(|row| row.get("number").and_then(Value::as_str).map(str::to_string))
            .collect();
        let ids = blacklist_numbers(&ctx, &numbers, "").await?;
        Ok(json!(ids))
    })
}

/// `phone_action_blacklist_remove` — the dialog, opened over a blacklist
/// entry.
fn open_unblacklist_dialog<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [id] = ctx.ids[..] else {
            return Err(RusdooError::User(
                "unblacklist one number at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(ctx.pool, "phone.blacklist", &[id], &["number"])
            .await?;
        let number = rows
            .first()
            .and_then(|row| row.get("number").cloned())
            .ok_or_else(|| RusdooError::Missing(format!("blacklist entry {id} is gone")))?;
        Ok(unblacklist_dialog(number))
    })
}

/// `phone_action_blacklist_remove` on a record that carries a number —
/// the same dialog, opened from the contact rather than from the list.
fn open_unblacklist_dialog_for_record<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [id] = ctx.ids[..] else {
            return Err(RusdooError::User(
                "unblacklist one record at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(ctx.pool, ctx.model, &[id], &["phone_sanitized"])
            .await?;
        let number = rows
            .first()
            .and_then(|row| row.get("phone_sanitized").cloned())
            .filter(|number| number.is_string())
            .ok_or_else(|| {
                RusdooError::User(
                    "this record has no readable phone number, so nothing to unblacklist".into(),
                )
            })?;
        Ok(unblacklist_dialog(number))
    })
}

fn unblacklist_dialog(number: Value) -> Value {
    json!({
        "type": "ir.actions.act_window",
        "name": "Are you sure you want to unblacklist this phone number?",
        "res_model": "phone.blacklist.remove",
        "view_mode": "form",
        // a dialog about the number, not a screen replacing the one it
        // was opened from
        "target": "new",
        "context": {"default_phone": number, "dialog_size": "medium"},
    })
}

/// `action_unblacklist_apply` — the dialog's button.
fn action_unblacklist_apply<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [wizard] = ctx.ids[..] else {
            return Err(RusdooError::User("the dialog is gone".into()));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "phone.blacklist.remove",
                &[wizard],
                &["phone", "reason"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::User("the dialog is gone".into()))?;
        let number = row
            .get("phone")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|number| !number.is_empty())
            .ok_or_else(|| RusdooError::User("the dialog names no number".into()))?
            .to_string();
        let reason = row
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(|reason| format!("Unblock Reason: {reason}"))
            .unwrap_or_default();
        let country = caller_country(&ctx).await;
        let sanitized = sanitize(&number, &country, Format::E164)?;
        let ids = unblacklist_numbers(&ctx, &[sanitized], &reason).await?;
        Ok(json!(ids))
    })
}

// ---------------------------------------------------------------------
// The mixin's methods
// ---------------------------------------------------------------------

/// The number a record carries, and the country it is read against.
async fn record_number(
    ctx: &MethodCtx<'_>,
    id: i64,
    fname: Option<&str>,
) -> Result<(Option<String>, Country), RusdooError> {
    let names: Vec<&str> = match fname {
        Some(name) => vec![name],
        None => NUMBER_FIELDS.to_vec(),
    };
    let mut wanted = names.clone();
    wanted.push("country_id");
    let rows = ctx.registry.read(ctx.pool, ctx.model, &[id], &wanted).await?;
    let row = rows
        .first()
        .ok_or_else(|| RusdooError::Missing(format!("{} {id} is gone", ctx.model)))?;
    let number = names.iter().find_map(|name| {
        row.get(*name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|number| !number.is_empty())
            .map(str::to_string)
    });
    let country = country_of(ctx, row.get("country_id").and_then(first_id)).await;
    Ok((number, country))
}

/// `phone_format(number=…, fname=…, force_format=…)` — port of
/// `BaseModel._phone_format`.
///
/// Either a number is given, in which case only the formatting matters,
/// or the record is asked for one. A number that cannot be read answers
/// `false`, as Odoo's does — a screen showing a contact must not fail
/// over the one field nobody ever filled in properly.
fn record_phone_format<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let want = match kwargs.get("force_format").and_then(Value::as_str) {
            Some(name) => Format::named(name)?,
            None => Format::E164,
        };
        let given = kwargs
            .get("number")
            .or_else(|| ctx.rest.first())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|number| !number.is_empty())
            .map(str::to_string);
        let fname = kwargs.get("fname").and_then(Value::as_str);

        let (number, country) = match given {
            // a number given outright is read against the caller's own
            // country: there is no record to ask
            Some(number) => (Some(number), caller_country(&ctx).await),
            None => {
                let [id] = ctx.ids[..] else {
                    return Err(RusdooError::User(
                        "phone_format reads one record at a time, or takes a number".into(),
                    ));
                };
                record_number(&ctx, id, fname).await?
            }
        };
        let Some(number) = number else {
            return Ok(json!(false));
        };
        match phone::phone_format(
            &number,
            country.code.as_deref(),
            country.phone_code,
            want,
            true,
        ) {
            Ok(formatted) => Ok(json!(formatted)),
            Err(_) => Ok(json!(false)),
        }
    })
}

/// `phone_blacklist_state` — is this record's number blocked?
///
/// Odoo answers with two stored-nothing computed fields,
/// `phone_sanitized_blacklisted` and `phone_blacklisted`; both need a
/// query against `phone.blacklist`, which a compute cannot make here. The
/// answer is the same, one entry per record.
fn phone_blacklist_state<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Ok(json!([]));
        }
        let mut wanted = NUMBER_FIELDS.to_vec();
        wanted.push("phone_sanitized");
        wanted.push("country_id");
        let rows = ctx
            .registry
            .read(ctx.pool, ctx.model, &ctx.ids, &wanted)
            .await?;
        let sanitized: Vec<String> = rows
            .iter()
            .filter_map(|row| {
                row.get("phone_sanitized")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let blocked = blocked_numbers(&ctx, &sanitized).await?;

        let mut state = Vec::with_capacity(rows.len());
        for row in &rows {
            let number = row.get("phone_sanitized").and_then(Value::as_str);
            let is_blocked = number.is_some_and(|number| blocked.iter().any(|b| b == number));
            // Odoo's `phone_blacklisted`: whether the blocked number is
            // the one in `phone` rather than the one in `mobile`. A model
            // with both keeps a single sanitized value, so this can only
            // ever describe the field that produced it.
            let country = country_of(&ctx, row.get("country_id").and_then(first_id)).await;
            let phone_is_the_blocked_one = is_blocked
                && row
                    .get("phone")
                    .and_then(Value::as_str)
                    .and_then(|raw| {
                        phone::phone_format(
                            raw,
                            country.code.as_deref(),
                            country.phone_code,
                            Format::E164,
                            true,
                        )
                        .ok()
                    })
                    .as_deref()
                    == number;
            state.push(json!({
                "id": row.get("id").cloned().unwrap_or(Value::Null),
                "phone_sanitized": number,
                "phone_sanitized_blacklisted": is_blocked,
                "phone_blacklisted": phone_is_the_blocked_one,
            }));
        }
        Ok(json!(state))
    })
}

/// Which of these numbers are on the blacklist right now.
async fn blocked_numbers(
    ctx: &MethodCtx<'_>,
    numbers: &[String],
) -> Result<Vec<String>, RusdooError> {
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    let domain = parse_domain(&json!([["number", "in", numbers]]))?;
    // active_test on: an archived entry is a number that came *off* the
    // blacklist, and treating it as blocked would be the whole feature
    // backwards
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "phone.blacklist",
            &domain,
            &SearchOptions::default(),
        )
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "phone.blacklist", &ids, &["number"])
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("number").and_then(Value::as_str).map(str::to_string))
        .collect())
}

/// The sanitized numbers of the records a mixin method was called on.
async fn sanitized_of(ctx: &MethodCtx<'_>) -> Result<Vec<String>, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::User("choose at least one record".into()));
    }
    let rows = ctx
        .registry
        .read(ctx.pool, ctx.model, &ctx.ids, &["phone_sanitized"])
        .await?;
    let numbers: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            row.get("phone_sanitized")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    if numbers.is_empty() {
        return Err(RusdooError::User(
            "none of the chosen records has a readable phone number".into(),
        ));
    }
    Ok(numbers)
}

/// `phone_set_blacklisted` — block these records' numbers.
fn phone_set_blacklisted<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let numbers = sanitized_of(&ctx).await?;
        let reason = kwargs
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ids = blacklist_numbers(&ctx, &numbers, reason).await?;
        Ok(json!(ids))
    })
}

/// `phone_reset_blacklisted` — let these records' numbers be contacted
/// again. Safe to call on a record that was never blocked, like Odoo's.
fn phone_reset_blacklisted<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let numbers = sanitized_of(&ctx).await?;
        let reason = kwargs
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ids = unblacklist_numbers(&ctx, &numbers, reason).await?;
        Ok(json!(ids))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    }

    #[test]
    fn a_contact_is_sanitized_against_its_own_country() {
        let belgian = record(vec![
            ("phone", json!("0456 99 88 77")),
            ("country_id.code", json!(["BE"])),
            ("country_id.phone_code", json!(["32"])),
        ]);
        assert_eq!(compute_phone_sanitized(&belgian), json!("+32456998877"));
    }

    #[test]
    fn the_mobile_wins_over_the_phone() {
        // `_phone_get_number_fields` puts mobile first: it is the number
        // an SMS reaches
        let both = record(vec![
            ("mobile", json!("0456 99 88 77")),
            ("phone", json!("02 345 67 89")),
            ("country_id.code", json!(["BE"])),
            ("country_id.phone_code", json!(["32"])),
        ]);
        assert_eq!(compute_phone_sanitized(&both), json!("+32456998877"));
    }

    #[test]
    fn a_number_that_cannot_be_read_leaves_the_column_empty() {
        // storing the raw string would make the blacklist match nothing
        // while looking like it matched
        let unreadable = record(vec![
            ("phone", json!("ring me")),
            ("country_id.code", json!(["BE"])),
            ("country_id.phone_code", json!(["32"])),
        ]);
        assert_eq!(compute_phone_sanitized(&unreadable), Value::Null);
        assert_eq!(compute_phone_sanitized(&Map::new()), Value::Null);
    }

    #[test]
    fn a_contact_without_a_country_is_still_sanitized_when_written_in_full() {
        let international = record(vec![("phone", json!("+32 456 99 88 77"))]);
        assert_eq!(
            compute_phone_sanitized(&international),
            json!("+32456998877")
        );
        // but a national number has nothing to be read against
        let national = record(vec![("phone", json!("0456 99 88 77"))]);
        assert_eq!(compute_phone_sanitized(&national), Value::Null);
    }

    #[test]
    fn only_an_e164_number_goes_on_the_blacklist() {
        assert!(is_sanitized(&record(vec![("number", json!("+32456998877"))])).is_ok());

        let readable_but_unsanitized = record(vec![("number", json!("+32 456 99 88 77"))]);
        let error = is_sanitized(&readable_but_unsanitized).expect_err("spacing is not E164");
        assert!(error.contains("write +32456998877"), "{error}");

        let nonsense = record(vec![("number", json!("nope"))]);
        assert!(is_sanitized(&nonsense).is_err());
        assert!(is_sanitized(&Map::new()).is_err());
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in ["phone.blacklist", "phone.blacklist.remove"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        let blacklist = reg.get("phone.blacklist").unwrap();
        assert!(blacklist.field("number").unwrap().required);
        // the uniqueness is the database's: two requests blacklisting the
        // same number at once would both find nothing and both insert
        assert_eq!(blacklist.sql_constraints().len(), 1);
        assert!(reg.get("phone.blacklist.remove").unwrap().is_transient());

        // and the partner keeps what `base` gave it
        let partner = reg.get("res.partner").unwrap();
        assert!(partner.field("name").unwrap().required);
        assert_eq!(partner.meta.table, "res_partner");
        let sanitized = partner.field("phone_sanitized").expect("the mixin's column");
        assert!(sanitized.stored, "every blacklist lookup joins on it");
        assert!(sanitized.compute.is_some());
    }

    #[test]
    fn the_blacklist_and_the_contact_get_their_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("phone.blacklist"),
            vec!["action_add", "add", "phone_action_blacklist_remove", "remove"]
        );
        assert_eq!(
            methods.names_for("phone.blacklist.remove"),
            vec!["action_unblacklist_apply"]
        );
        assert_eq!(
            methods.names_for("res.partner"),
            vec![
                "phone_action_blacklist_remove",
                "phone_blacklist_state",
                "phone_format",
                "phone_reset_blacklisted",
                "phone_set_blacklisted",
            ]
        );
        // reading a number is not editing one
        assert_eq!(
            methods.get("res.partner", "phone_format").unwrap().operation,
            Operation::Read
        );
        assert_eq!(
            methods.get("phone.blacklist", "add").unwrap().operation,
            Operation::Write
        );
    }

    #[test]
    fn the_mixin_can_be_given_to_another_modules_model() {
        let mut methods = MethodRegistry::new();
        extend_phone_methods(&mut methods, &["crm.lead"]).unwrap();
        assert!(methods.get("crm.lead", "phone_blacklist_state").is_some());
        assert_eq!(phone_mixin_fields().len(), 1);
    }
}

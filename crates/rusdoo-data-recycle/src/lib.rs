//! rusdoo-data-recycle — port de `odoo/addons/data_recycle/models/`: as
//! regras que encontram registro velho para arquivar ou apagar.
//!
//! Two models and one cycle. A rule (`data_recycle.model`) says which
//! que modelo procurar, com que filtro, a partir de que idade e o que
//! fazer com o que achar. A varredura materializa cada achado numa
//! suggestion (`data_recycle.record`), and it is the suggestion — not
//! the rule — that somebody validates or discards. The step in between
//! exists because deleting is irreversible: between "the rule matched"
//! and "the record is gone" a human has to fit, and in automatic mode
//! the decision to do without that human is an explicit one.
//!
//! O `ir.cron` do addon chama `cron_recycle_records` uma vez por dia.

use chrono::{Days, Months, NaiveDateTime, Utc};
use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const RULE: &str = "data_recycle.model";
const RECORD: &str = "data_recycle.record";

/// O que a regra faz com o que encontrou.
const ARCHIVE: &str = "archive";
const UNLINK: &str = "unlink";

/// How many candidates one scan brings back per run.
///
/// A freshly written rule over a large table matches everything at
/// once; with no cap the first run would become an insert of millions of
/// rows inside the cron. With one, the next run carries on where this
/// one stopped — Odoo does the same with `DR_CREATE_STEP_*`, only in
/// lotes commitados em vez de um corte.
const SCAN_LIMIT: u64 = 5_000;

fn char(name: &str) -> Field {
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

/// O valor textual de um campo, vazio quando ausente ou nulo.
fn text(record: &Map<String, Value>, name: &str) -> String {
    record
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// O valor inteiro de um campo, zero quando ausente ou nulo.
fn int(record: &Map<String, Value>, name: &str) -> i64 {
    record.get(name).and_then(Value::as_i64).unwrap_or(0)
}

fn flag(record: &Map<String, Value>, name: &str) -> bool {
    record.get(name).and_then(Value::as_bool).unwrap_or(false)
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(recycle_rule())?;
    reg.register(recycle_record())?;
    Ok(())
}

/// The buttons of both screens, plus the work the cron calls.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // scanning creates suggestions: it is a write, even when nothing
    // is deleted
    methods.register(
        RULE,
        "action_recycle_records",
        Operation::Write,
        action_recycle_records,
    )?;
    // o job agendado: mesmo caminho, sobre todas as regras ativas
    methods.register(
        RULE,
        "cron_recycle_records",
        Operation::Write,
        cron_recycle_records,
    )?;
    // validar apaga ou arquiva registro alheio, e depois apaga a
    // a suggestion — whoever may call this must be allowed to delete
    methods.register(
        RECORD,
        "action_validate",
        Operation::Unlink,
        action_validate,
    )?;
    methods.register(RECORD, "action_discard", Operation::Write, action_discard)?;
    Ok(())
}

// ---------------------------------------------------------------- modelos

/// A filter the database cannot read is a rule that only fails at three
/// in the morning, inside the cron, with nobody watching the log. Refuse
/// it at save time, while there is still somebody in front of a screen.
fn filter_is_a_domain(record: &Map<String, Value>) -> Result<(), String> {
    let raw = text(record, "domain");
    if raw.trim().is_empty() {
        return Ok(());
    }
    let terms: Vec<Value> = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "the filter must be a JSON list of conditions, such as \
             [[\"state\", \"=\", \"cancel\"]]: {error}"
        )
    })?;
    parse_domain(&Value::Array(terms))
        .map_err(|error| format!("the filter is not valid: {error}"))?;
    Ok(())
}

/// Uma regra sem filtro e sem idade casa com a tabela inteira: o
/// primeiro clique em "Executar agora" levaria o modelo todo. O Odoo
/// lets such a rule be saved; here it does not, because the damage does
/// not come back and the cost of demanding one of the two is one filled
/// field.
fn rule_selects_something(record: &Map<String, Value>) -> Result<(), String> {
    let raw = text(record, "domain");
    let no_filter = serde_json::from_str::<Vec<Value>>(&raw).is_ok_and(|terms| terms.is_empty())
        || raw.trim().is_empty();
    let no_age = text(record, "time_field_name").trim().is_empty();
    if no_filter && no_age {
        return Err(
            "the rule needs a filter or an age field: with neither it would \
                    match every record of the model"
                .into(),
        );
    }
    Ok(())
}

/// Age zero means "everything", and a negative age is the future.
/// Neither is what anyone meant when they picked a date field.
fn age_is_a_real_interval(record: &Map<String, Value>) -> Result<(), String> {
    if text(record, "time_field_name").trim().is_empty() {
        return Ok(());
    }
    let delta = int(record, "time_field_delta");
    if delta <= 0 {
        return Err(format!(
            "the age interval is {delta}: it must be greater than zero (1 month, 30 days, …)"
        ));
    }
    Ok(())
}

/// How many open suggestions the rule has today.
///
/// A discarded one does not count: whoever discarded it said that
/// record stays, and a counter that kept insisting would be an alarm
/// nobody can switch off.
fn open_suggestions(record: &Map<String, Value>) -> Value {
    let flags = record
        .get("recycle_record_ids.active")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    json!(flags
        .iter()
        .filter(|value| value.as_bool() == Some(true))
        .count())
}

/// `data_recycle.model` — a regra.
///
/// Deviation from Odoo: the target model and the date field are
/// technical names in a `Char`, not many2ones to
/// `ir.model`/`ir.model.fields`. The port has no dynamic model registry,
/// and a many2one to a table that does not exist would be a field that
/// never resolves.
fn recycle_rule() -> Model {
    Model::new(
        meta(RULE, "data_recycle_model"),
        vec![
            char("name").required(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // the technical name of the scanned model, e.g. "mail.message"
            char("res_model_name").required(),
            Field::new(
                "recycle_mode",
                FieldType::Selection(vec![
                    ("manual".into(), "Manual".into()),
                    ("automatic".into(), "Automatic".into()),
                ]),
            )
            .required()
            // manual by default: the mode that deletes on its own is a
            // choice, never what you get for not having chosen
            .default_value(json!("manual")),
            Field::new(
                "recycle_action",
                FieldType::Selection(vec![
                    (ARCHIVE.into(), "Arquivar".into()),
                    (UNLINK.into(), "Excluir".into()),
                ]),
            )
            .required()
            .default_value(json!(ARCHIVE)),
            // the filter, as JSON — the same domain the views use
            char("domain").default_value(json!("[]")),
            char("time_field_name"),
            Field::new("time_field_delta", FieldType::Integer).default_value(json!(1)),
            Field::new(
                "time_field_delta_unit",
                FieldType::Selection(vec![
                    ("days".into(), "Days".into()),
                    ("weeks".into(), "Weeks".into()),
                    ("months".into(), "Months".into()),
                    ("years".into(), "Years".into()),
                ]),
            )
            .default_value(json!("months")),
            Field::new("include_archived", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "recycle_record_ids",
                FieldType::One2many {
                    comodel: RECORD.into(),
                    inverse: "recycle_model_id".into(),
                },
            ),
            // not materialised: the number changes with every scan and
            // every validation, and nobody sorts a list of rules by it
            Field::new("records_to_recycle_count", FieldType::Integer)
                .computed(&["recycle_record_ids.active"], open_suggestions),
            // when the rule last ran. Not `.readonly()`: in this ORM
            // that would lock out the scan itself, which is what writes
            // the field. What stays out is the client, and
            // isso a view resolve.
            Field::new("last_scan", FieldType::Datetime),
        ],
    )
    .constrained("valid filter", &["domain"], filter_is_a_domain)
    .constrained(
        "a rule that selects something",
        &["domain", "time_field_name"],
        rule_selects_something,
    )
    .constrained(
        "valid age",
        &["time_field_name", "time_field_delta"],
        age_is_a_real_interval,
    )
}

/// `data_recycle.record` — um registro que a regra separou.
///
/// Deviation from Odoo: the target's name is written down by the scan,
/// not computed on every read. Odoo reads the original `display_name`
/// each time (and shows "**Record Deleted**" when it cannot find it);
/// here the name is what the record was called when it was set aside,
/// which is exactly the information that disappears the moment the
/// suggestion is applied.
fn recycle_record() -> Model {
    Model::new(
        meta(RECORD, "data_recycle_record"),
        vec![
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new(
                "recycle_model_id",
                FieldType::Many2one {
                    comodel: RULE.into(),
                },
            )
            .required(),
            // o par (modelo, id) que aponta o alvo, como o Odoo faz
            char("res_model_name").required(),
            Field::new("res_id", FieldType::Integer).required(),
            char("name"),
        ],
    )
}

// ---------------------------------------------------------------- varredura

/// A data-limite da regra, no formato que o campo alvo entende.
///
/// Kept apart from the rest because it is where the easy mistake
/// lives: "3 months" is not "90 days", and a `Date` compared against a
/// datetime string does not match
/// nada.
fn cutoff_for(
    ty: &FieldType,
    unit: &str,
    delta: i64,
    now: NaiveDateTime,
) -> Result<String, RusdooError> {
    let moment = subtract(now, unit, delta).ok_or_else(|| {
        RusdooError::Validation(format!(
            "cannot go back {delta} {unit:?} from {now}: review the age interval"
        ))
    })?;
    Ok(match ty {
        FieldType::Date => moment.format("%Y-%m-%d").to_string(),
        FieldType::Datetime => moment.format("%Y-%m-%d %H:%M:%S").to_string(),
        other => {
            return Err(RusdooError::Validation(format!(
                "the age field must be a date or a datetime, not {other:?}"
            )))
        }
    })
}

fn subtract(now: NaiveDateTime, unit: &str, delta: i64) -> Option<NaiveDateTime> {
    let count = u32::try_from(delta).ok()?;
    match unit {
        "days" => now.checked_sub_days(Days::new(u64::from(count))),
        "weeks" => now.checked_sub_days(Days::new(u64::from(count).checked_mul(7)?)),
        // months and years walk the calendar, not multiples of 30 days
        "months" => now.checked_sub_months(Months::new(count)),
        "years" => now.checked_sub_months(Months::new(count.checked_mul(12)?)),
        _ => None,
    }
}

/// The domain the rule looks for: the user's filter, plus the age
/// de idade quando a regra tem uma.
fn rule_domain(
    rule: &Map<String, Value>,
    target: &Model,
    now: NaiveDateTime,
) -> Result<Domain, RusdooError> {
    let raw = text(rule, "domain");
    let raw = if raw.trim().is_empty() {
        "[]".into()
    } else {
        raw
    };
    let mut terms: Vec<Value> = serde_json::from_str(&raw).map_err(|error| {
        RusdooError::Validation(format!(
            "the rule's filter is not a JSON list of conditions: {error}"
        ))
    })?;
    let field_name = text(rule, "time_field_name");
    if !field_name.trim().is_empty() {
        let field = target.field(&field_name).ok_or_else(|| {
            RusdooError::Validation(format!(
                "age field {field_name:?} does not exist on {}",
                target.meta.name
            ))
        })?;
        let cutoff = cutoff_for(
            &field.ty,
            &text(rule, "time_field_delta_unit"),
            int(rule, "time_field_delta"),
            now,
        )?;
        // terms at the same level are an implicit AND: the user's filter
        // continua valendo inteiro ao lado da idade
        terms.push(json!([field_name, "<=", cutoff]));
    }
    parse_domain(&Value::Array(terms))
}

/// Varre uma regra e devolve quantos candidatos novos ela separou.
async fn scan_rule(ctx: &MethodCtx<'_>, rule_id: i64) -> Result<usize, RusdooError> {
    let rules = ctx
        .registry
        .read(
            ctx.pool,
            RULE,
            &[rule_id],
            &[
                "name",
                "res_model_name",
                "recycle_action",
                "domain",
                "time_field_name",
                "time_field_delta",
                "time_field_delta_unit",
                "include_archived",
            ],
        )
        .await?;
    let rule = rules
        .first()
        .ok_or_else(|| RusdooError::Validation(format!("rule {rule_id} does not exist")))?;
    let rule_name = text(rule, "name");
    let target_name = text(rule, "res_model_name");
    let target = ctx.registry.get(&target_name).ok_or_else(|| {
        RusdooError::Validation(format!(
            "rule {rule_name:?} points at model {target_name:?}, which is not registered: \
             fix the model's technical name"
        ))
    })?;
    let action = text(rule, "recycle_action");
    // a mesma recusa do `_check_recycle_action` do Odoo, feita aqui
    // because this is where the model registry is visible
    if action == ARCHIVE && target.field("active").is_none() {
        return Err(RusdooError::Validation(format!(
            "model {target_name} has no archivable records: change the action of rule \
             {rule_name:?} to delete"
        )));
    }

    let domain = rule_domain(rule, target, Utc::now().naive_utc())?;
    // archiving what is already archived does nothing and the rule finds
    // the same record again forever; Odoo hides the option in that case
    let include_archived = flag(rule, "include_archived") && action == UNLINK;
    let found = ctx
        .registry
        .search(
            ctx.pool,
            &target_name,
            &domain,
            &SearchOptions {
                active_test: !include_archived,
                limit: Some(SCAN_LIMIT),
                ..SearchOptions::default()
            },
        )
        .await?;

    let listed = already_listed(ctx, rule_id).await?;
    let fresh: Vec<i64> = found
        .into_iter()
        .filter(|id| !listed.contains(id))
        .collect();
    let names = if fresh.is_empty() {
        HashMap::new()
    } else {
        ctx.registry
            .display_names(ctx.pool, &target_name, &fresh)
            .await?
    };
    for id in &fresh {
        let name = names
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("{target_name},{id}"));
        ctx.registry
            .create_as(
                ctx.pool,
                ctx.uid,
                RECORD,
                vec![
                    ("recycle_model_id", json!(rule_id)),
                    ("res_model_name", json!(target_name)),
                    ("res_id", json!(id)),
                    ("name", json!(name)),
                ],
            )
            .await?;
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            RULE,
            &[rule_id],
            vec![(
                "last_scan",
                json!(Utc::now()
                    .naive_utc()
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()),
            )],
        )
        .await?;
    Ok(fresh.len())
}

/// The `res_id`s the rule has already set aside — the discarded ones
/// included, which must not come back on the next scan.
async fn already_listed(ctx: &MethodCtx<'_>, rule_id: i64) -> Result<HashSet<i64>, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            RECORD,
            &parse_domain(&json!([["recycle_model_id", "=", rule_id]]))?,
            &SearchOptions {
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    Ok(ctx
        .registry
        .read(ctx.pool, RECORD, &ids, &["res_id"])
        .await?
        .iter()
        .filter_map(|row| row.get("res_id").and_then(Value::as_i64))
        .collect())
}

/// Apply the open suggestions of an automatic rule.
async fn apply_open_suggestions(ctx: &MethodCtx<'_>, rule_id: i64) -> Result<usize, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            RECORD,
            &parse_domain(&json!([["recycle_model_id", "=", rule_id]]))?,
            &SearchOptions::default(),
        )
        .await?;
    validate_suggestions(ctx, &ids).await
}

// ---------------------------------------------------------------- methods

/// `action_recycle_records` — the "Run now" button.
///
/// On a manual rule the answer is the suggestions screen: whoever asked
/// it to run wants to see what turned up, not an "ok". On an automatic
/// rule there is nothing to review, and the answer is how many records
/// were dealt with.
fn action_recycle_records<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "the action needs at least one rule".into(),
            ));
        }
        let mut applied = 0;
        let mut manual = false;
        for rule_id in ctx.ids.clone() {
            scan_rule(&ctx, rule_id).await?;
            let rows = ctx
                .registry
                .read(ctx.pool, RULE, &[rule_id], &["recycle_mode"])
                .await?;
            let mode = rows.first().map(|row| text(row, "recycle_mode"));
            if mode.as_deref() == Some("automatic") {
                applied += apply_open_suggestions(&ctx, rule_id).await?;
            } else {
                manual = true;
            }
        }
        if manual {
            return Ok(json!({
                "type": "ir.actions.act_window",
                "name": "Registros a reciclar",
                "res_model": RECORD,
                "views": [[false, "list"], [false, "form"]],
                "domain": [["recycle_model_id", "in", ctx.ids]],
                "target": "current",
            }));
        }
        Ok(json!(applied))
    })
}

/// `cron_recycle_records` — o trabalho de todo dia.
///
/// Scan every active rule and apply only the automatic ones. A rule
/// que quebra (modelo removido, filtro que deixou de casar com o
/// schema) does not bring the others down: the cron exists for the ones
/// funcionam.
fn cron_recycle_records<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let rules = ctx
            .registry
            .search(ctx.pool, RULE, &Domain::True, &SearchOptions::default())
            .await?;
        let mut scanned = 0usize;
        let mut applied = 0usize;
        for rule_id in rules {
            match scan_rule(&ctx, rule_id).await {
                Ok(found) => scanned += found,
                Err(error) => {
                    tracing::error!("data_recycle: a regra {rule_id} falhou: {error}");
                    continue;
                }
            }
            let rows = ctx
                .registry
                .read(ctx.pool, RULE, &[rule_id], &["recycle_mode"])
                .await?;
            if rows.first().map(|row| text(row, "recycle_mode")).as_deref() != Some("automatic") {
                continue;
            }
            match apply_open_suggestions(&ctx, rule_id).await {
                Ok(done) => applied += done,
                Err(error) => {
                    tracing::error!("data_recycle: rule {rule_id} could not run: {error}")
                }
            }
        }
        tracing::info!("data_recycle: {scanned} candidato(s) novos, {applied} aplicado(s)");
        Ok(json!({"scanned": scanned, "applied": applied}))
    })
}

/// `action_validate` — faz o que a regra mandou fazer.
fn action_validate<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "choose at least one record to validate".into(),
            ));
        }
        let ids = ctx.ids.clone();
        Ok(json!(validate_suggestions(&ctx, &ids).await?))
    })
}

/// Archive or delete the targets of `ids`, then remove the suggestions.
///
/// Grouped by (model, action): a thousand suggestions of the same model
/// become one write, not a thousand. A suggestion is only removed after
/// its target was dealt with — if the delete fails, it stays on the list
/// for somebody
/// olhar, em vez de sumir tendo feito nada.
async fn validate_suggestions(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<usize, RusdooError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            RECORD,
            ids,
            &["recycle_model_id", "res_model_name", "res_id"],
        )
        .await?;
    let actions = rule_actions(ctx, &rows).await?;

    // (target model, action) -> the target's ids
    let mut batches: HashMap<(String, String), Vec<i64>> = HashMap::new();
    for row in &rows {
        let (Some(res_id), rule_id) = (
            row.get("res_id").and_then(Value::as_i64),
            first_id(row.get("recycle_model_id")),
        ) else {
            continue;
        };
        let Some(action) = rule_id.and_then(|id| actions.get(&id)).cloned() else {
            continue;
        };
        batches
            .entry((text(row, "res_model_name"), action))
            .or_default()
            .push(res_id);
    }

    for ((model_name, action), mut targets) in batches {
        targets.sort_unstable();
        targets.dedup();
        // the target may be gone between the scan and now: only what is
        // still there is dealt with, and the rest counts as done
        let alive = ctx
            .registry
            .search(
                ctx.pool,
                &model_name,
                &parse_domain(&json!([["id", "in", targets]]))?,
                &SearchOptions {
                    active_test: false,
                    ..SearchOptions::default()
                },
            )
            .await?;
        if alive.is_empty() {
            continue;
        }
        if action == ARCHIVE {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    &model_name,
                    &alive,
                    vec![("active", json!(false))],
                )
                .await?;
        } else {
            let model = ctx.registry.get(&model_name).ok_or_else(|| {
                RusdooError::Validation(format!("model {model_name} is not registered"))
            })?;
            model.unlink(ctx.pool, &alive).await?;
        }
    }

    let record_model = ctx
        .registry
        .get(RECORD)
        .ok_or_else(|| RusdooError::Validation(format!("model {RECORD} is not registered")))?;
    record_model.unlink(ctx.pool, ids).await?;
    Ok(rows.len())
}

/// The action of every rule the suggestions name, in one read.
async fn rule_actions(
    ctx: &MethodCtx<'_>,
    rows: &[Map<String, Value>],
) -> Result<HashMap<i64, String>, RusdooError> {
    let mut rule_ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| first_id(row.get("recycle_model_id")))
        .collect();
    rule_ids.sort_unstable();
    rule_ids.dedup();
    if rule_ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(ctx
        .registry
        .read(ctx.pool, RULE, &rule_ids, &["recycle_action"])
        .await?
        .iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_i64)?;
            Some((id, text(row, "recycle_action")))
        })
        .collect())
}

/// `action_discard` — "esse fica".
///
/// The suggestion is archived, not deleted: deleted, the next scan
/// would recreate it, and the same record would go on asking for a
/// decision forever.
fn action_discard<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "choose at least one record to discard".into(),
            ));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                RECORD,
                &ctx.ids,
                vec![("active", json!(false))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// The id of a many2one, which reads as `[id, name]`.
fn first_id(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn moment() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 31)
            .and_then(|day| day.and_hms_opt(14, 30, 0))
            .expect("a valid date")
    }

    #[test]
    fn a_idade_anda_no_calendario_e_no_formato_do_campo() {
        // one month before 31 March is 28 February, not "30 days"
        assert_eq!(
            cutoff_for(&FieldType::Date, "months", 1, moment()).unwrap(),
            "2026-02-28"
        );
        // a datetime carries the time with it, or the comparison misses
        assert_eq!(
            cutoff_for(&FieldType::Datetime, "days", 30, moment()).unwrap(),
            "2026-03-01 14:30:00"
        );
        assert_eq!(
            cutoff_for(&FieldType::Date, "weeks", 2, moment()).unwrap(),
            "2026-03-17"
        );
        assert_eq!(
            cutoff_for(&FieldType::Date, "years", 3, moment()).unwrap(),
            "2023-03-31"
        );
    }

    #[test]
    fn um_campo_que_nao_e_data_nao_serve_de_idade() {
        let error = cutoff_for(&FieldType::Integer, "days", 1, moment())
            .expect_err("an integer is not a date");
        assert!(
            error.to_string().contains("must be a date"),
            "{error}"
        );
        // and a unit nobody defined does not silently become days
        let error = cutoff_for(&FieldType::Date, "quinzenas", 1, moment())
            .expect_err("an unknown unit is not silently days");
        assert!(error.to_string().contains("review the age interval"), "{error}");
    }

    #[test]
    fn o_filtro_e_a_idade_valem_juntos() {
        let target = Model::new(
            meta("mail.message", "mail_message"),
            vec![
                char("subject"),
                Field::new("create_date", FieldType::Datetime),
            ],
        );
        let mut rule = Map::new();
        rule.insert("domain".into(), json!(r#"[["subject", "=", "spam"]]"#));
        rule.insert("time_field_name".into(), json!("create_date"));
        rule.insert("time_field_delta".into(), json!(6));
        rule.insert("time_field_delta_unit".into(), json!("months"));

        let domain = rule_domain(&rule, &target, moment()).unwrap();
        let Domain::And(parts) = domain else {
            panic!("the filter and the age must both hold: {domain:?}");
        };
        assert_eq!(parts.len(), 2);
        assert!(parts[0].mentions("subject"));
        assert!(parts[1].mentions("create_date"));
    }

    #[test]
    fn um_filtro_vazio_deixa_so_a_idade() {
        let target = Model::new(
            meta("mail.message", "mail_message"),
            vec![Field::new("create_date", FieldType::Datetime)],
        );
        let mut rule = Map::new();
        rule.insert("domain".into(), json!("[]"));
        rule.insert("time_field_name".into(), json!("create_date"));
        rule.insert("time_field_delta".into(), json!(1));
        rule.insert("time_field_delta_unit".into(), json!("days"));
        let domain = rule_domain(&rule, &target, moment()).unwrap();
        assert!(domain.mentions("create_date"));
        assert!(matches!(domain, Domain::Term(_)), "{domain:?}");
    }

    #[test]
    fn uma_regra_que_pegaria_tudo_e_recusada() {
        let mut rule = Map::new();
        rule.insert("domain".into(), json!("[]"));
        rule.insert("time_field_name".into(), json!(""));
        let reason = rule_selects_something(&rule).expect_err("no filter and no age");
        assert!(reason.contains("match every record"), "{reason}");

        // either of the two is enough for the rule to be acceptable
        rule.insert("time_field_name".into(), json!("create_date"));
        assert!(rule_selects_something(&rule).is_ok());
        rule.insert("time_field_name".into(), json!(""));
        rule.insert("domain".into(), json!(r#"[["state", "=", "cancel"]]"#));
        assert!(rule_selects_something(&rule).is_ok());
    }

    #[test]
    fn um_filtro_que_o_banco_nao_entende_nao_e_salvo() {
        let mut rule = Map::new();
        rule.insert("domain".into(), json!("state = cancel"));
        let reason = filter_is_a_domain(&rule).expect_err("that is not JSON");
        assert!(reason.contains("JSON list"), "{reason}");

        // valid JSON, invalid domain: an operator that does not exist
        rule.insert("domain".into(), json!(r#"[["state", "~=", "cancel"]]"#));
        let reason = filter_is_a_domain(&rule).expect_err("operador desconhecido");
        assert!(reason.contains("is not valid"), "{reason}");

        rule.insert("domain".into(), json!(r#"[["state", "=", "cancel"]]"#));
        assert!(filter_is_a_domain(&rule).is_ok());
    }

    #[test]
    fn uma_idade_de_zero_nao_e_uma_idade() {
        let mut rule = Map::new();
        rule.insert("time_field_name".into(), json!("create_date"));
        rule.insert("time_field_delta".into(), json!(0));
        let reason = age_is_a_real_interval(&rule).expect_err("zero is not an interval");
        assert!(reason.contains("greater than zero"), "{reason}");

        // with no age field, the interval is not looked at
        rule.insert("time_field_name".into(), json!(""));
        assert!(age_is_a_real_interval(&rule).is_ok());
    }

    #[test]
    fn o_contador_ignora_o_que_foi_descartado() {
        let mut rule = Map::new();
        rule.insert(
            "recycle_record_ids.active".into(),
            json!([true, false, true]),
        );
        assert_eq!(open_suggestions(&rule), json!(2));
        // a rule with no suggestion at all counts zero, not null
        assert_eq!(open_suggestions(&Map::new()), json!(0));
    }

    #[test]
    fn os_modelos_registram_sobre_a_base() {
        let mut reg = rusdoo_base::registry().expect("a base registra");
        extend(&mut reg).expect("o addon registra");
        for name in [RULE, RECORD] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the counter is derived: no client writes to it
        let count = reg
            .get(RULE)
            .and_then(|model| model.field("records_to_recycle_count"))
            .expect("o contador existe");
        assert!(count.readonly && !count.stored);
    }

    #[test]
    fn a_regra_tem_o_botao_e_o_job_e_a_sugestao_tem_os_dois_dela() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).expect("the methods register");
        assert_eq!(
            methods.names_for(RULE),
            vec!["action_recycle_records", "cron_recycle_records"]
        );
        assert_eq!(
            methods.names_for(RECORD),
            vec!["action_discard", "action_validate"]
        );
    }
}

//! rusdoo-crm — port of `odoo/addons/crm/`: the pipeline, from a name
//! somebody wrote down to a deal that was won or lost.
//!
//! A **lead** is an unqualified scrap — a business card, a form somebody
//! filled in. An **opportunity** is the same record once somebody
//! decided it is worth working: same model, `type` says which, exactly
//! as Odoo does it. The **stages** are the columns of the pipeline, and
//! the two ends of it are what the whole module is for: a deal is won at
//! a stage marked `is_won`, and lost with a **reason**, because "we lost"
//! without why is a number nobody can act on.
//!
//! ## What is deliberately not here
//!
//! * **Automatic assignment.** Odoo's `crm.team` has a lead-mining and
//!   round-robin assignment with domains per salesperson, run by a cron
//!   over a scoring model. It is a module's worth of behaviour on its
//!   own and it needs `ir.cron` to write records nobody asked it to;
//!   here a lead is assigned by whoever writes `user_id`.
//! * **Recurring revenue** (`recurring_plan`, MRR and their prorations),
//!   which is subscription accounting the port has nothing to check
//!   against yet.
//! * **The probability model.** Odoo *predicts* `probability` from won
//!   and lost history per team, stage, country and source
//!   (`crm_lead_prediction`). What is ported is the number and the two
//!   ends it is pinned to — 100 when won, 0 when lost — not the guess in
//!   between, which would be a made-up number wearing a statistic's
//!   clothes.
//! * **Merging duplicates**, which reads every field of both records and
//!   decides which survives; it needs the field-level metadata this ORM
//!   does not expose yet.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
// `crm.tag` is not here: it belongs to `sales_team`, in this port as in
// Odoo, and a lead only points at one.
use serde_json::{json, Map, Value};

/// Money, at the precision an amount is written in.
const AMOUNT: FieldType = FieldType::Float {
    digits: Some((16, 2)),
};

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(stage())?;
    reg.register(lost_reason())?;
    reg.register(lead())?;
    Ok(())
}

/// The two buttons the pipeline exists for, and the one that promotes a
/// scrap of paper into something somebody works on.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register("crm.lead", "action_set_won", Operation::Write, action_set_won)?;
    methods.register(
        "crm.lead",
        "action_set_lost",
        Operation::Write,
        action_set_lost,
    )?;
    methods.register(
        "crm.lead",
        "convert_opportunity",
        Operation::Write,
        convert_opportunity,
    )?;
    Ok(())
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

fn selection(name: &str, options: &[(&str, &str)]) -> Field {
    Field::new(
        name,
        FieldType::Selection(
            options
                .iter()
                .map(|(v, l)| ((*v).to_string(), (*l).to_string()))
                .collect(),
        ),
    )
}

// ── models ──────────────────────────────────────────────────────────

/// `crm.stage` — a column of the pipeline.
fn stage() -> Model {
    Model::new(
        meta("crm.stage", "crm_stage"),
        vec![
            char("name").required().translatable(),
            Field::new("sequence", FieldType::Integer).default_value(json!(1)),
            // the one thing a stage says about the deal in it
            Field::new("is_won", FieldType::Boolean).default_value(json!(false)),
            Field::new("fold", FieldType::Boolean).default_value(json!(false)),
            Field::new("requirements", FieldType::Text),
            m2o("team_id", "crm.team"),
            Field::new("color", FieldType::Integer),
        ],
    )
    .ordered("sequence, name, id")
}

/// `crm.lost.reason` — why a deal was lost, which is the only part of
/// losing one that is worth anything later.
fn lost_reason() -> Model {
    Model::new(
        meta("crm.lost.reason", "crm_lost_reason"),
        vec![
            char("name").required().translatable(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .ordered("name, id")
}

/// A probability is a percentage, and a percentage that is not one is a
/// number that will be summed with others and reported.
fn probability_is_a_percentage(record: &Map<String, Value>) -> Result<(), String> {
    let probability = record
        .get("probability")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if !(0.0..=100.0).contains(&probability) {
        return Err(format!(
            "a probability is between 0 and 100, not {probability}"
        ));
    }
    Ok(())
}

/// An expected revenue below zero is not a deal, it is a refund written
/// in the wrong place.
fn revenue_is_not_negative(record: &Map<String, Value>) -> Result<(), String> {
    let revenue = record
        .get("expected_revenue")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if revenue < 0.0 {
        return Err("the expected revenue cannot be negative".into());
    }
    Ok(())
}

/// `crm.lead` — the scrap of paper and the deal, one model apart by
/// `type`.
fn lead() -> Model {
    Model::new(
        meta("crm.lead", "crm_lead"),
        vec![
            char("name").required(),
            selection("type", &[("lead", "Lead"), ("opportunity", "Opportunity")])
                .required()
                .default_value(json!("lead")),
            // who it is, in the two shapes a lead arrives in: a contact
            // record, or the words somebody typed before there was one
            m2o("partner_id", "res.partner"),
            char("contact_name"),
            char("partner_name"),
            char("email_from"),
            char("phone"),
            char("function"),
            char("website"),
            char("city"),
            m2o("country_id", "res.country"),
            m2o("user_id", "res.users").default_from(defaults::CURRENT_USER),
            m2o("team_id", "crm.team"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("stage_id", "crm.stage"),
            Field::new("expected_revenue", AMOUNT).default_value(json!(0.0)),
            Field::new("probability", FieldType::Float { digits: Some((5, 2)) })
                .default_value(json!(10.0)),
            selection(
                "priority",
                &[("0", "Low"), ("1", "Medium"), ("2", "High"), ("3", "Very High")],
            )
            .default_value(json!("0")),
            Field::new("date_deadline", FieldType::Date),
            // not `readonly` here, unlike Odoo: in this ORM `readonly`
            // forbids the write outright, and it is `action_set_won` /
            // `action_set_lost` that stamp this. What keeps a user out of
            // it is the form's arch.
            Field::new("date_closed", FieldType::Datetime),
            Field::new("description", FieldType::Html),
            char("referred"),
            // a lost deal is archived, never deleted: what was lost and
            // why is the history the next quarter is planned from
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("lost_reason_id", "crm.lost.reason").ondelete(OnDelete::Restrict),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "crm.tag".into(),
                    relation: "crm_tag_rel".into(),
                    column1: "lead_id".into(),
                    column2: "tag_id".into(),
                },
            ),
            // where it came from, which is what a campaign is measured by
            m2o("source_id", "utm.source"),
            m2o("medium_id", "utm.medium"),
            m2o("campaign_id", "utm.campaign"),
            Field::new("color", FieldType::Integer),
        ],
    )
    .constrained(
        "a probability is a percentage",
        &["probability"],
        probability_is_a_percentage,
    )
    .constrained(
        "the expected revenue is not negative",
        &["expected_revenue"],
        revenue_is_not_negative,
    )
    // Odoo orders by priority desc, then by the id: the pipeline shows
    // what somebody flagged first
    .ordered("priority desc, id desc")
}

// ── methods ─────────────────────────────────────────────────────────

fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        other => other.as_i64(),
    }
}

fn wanted<'a>(ctx: &'a MethodCtx<'a>, kwargs: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    kwargs.get(name).or_else(|| ctx.rest.first())
}

/// `action_set_won` — the deal closed, and the pipeline says so at both
/// ends: the probability goes to certainty and the record moves to the
/// stage that means won.
///
/// Odoo picks that stage per sales team; here it is the first stage
/// marked `is_won`, in sequence, of the lead's own team when it has one.
fn action_set_won<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "say which opportunity was won".into(),
            ));
        }
        let leads = ctx
            .registry
            .read(ctx.pool, "crm.lead", &ctx.ids, &["team_id", "active"])
            .await?;
        for lead in leads {
            let id = lead["id"].as_i64().expect("read returns the id");
            let team = lead.get("team_id").and_then(first_id);
            let stage = won_stage(&ctx, team).await?;
            let mut values: Vec<(&str, Value)> = vec![
                ("probability", json!(100.0)),
                ("date_closed", json!(now())),
                // won and archived would hide the good news from every
                // report: winning re-opens a lead that had been lost
                ("active", json!(true)),
                ("lost_reason_id", Value::Null),
            ];
            if let Some(stage) = stage {
                values.push(("stage_id", json!(stage)));
            }
            ctx.registry
                .write_as(ctx.pool, ctx.uid, "crm.lead", &[id], values)
                .await?;
        }
        Ok(json!(true))
    })
}

/// The stage a won deal lands in: the team's own if it has one, else any
/// stage marked won. `None` when nobody has configured one, which is a
/// pipeline with no end — the probability still records the win.
async fn won_stage(ctx: &MethodCtx<'_>, team: Option<i64>) -> Result<Option<i64>, RusdooError> {
    use rusdoo_orm::crud::SearchOptions;
    let opts = SearchOptions {
        limit: Some(1),
        ..SearchOptions::default()
    };
    if let Some(team) = team {
        let domain = rusdoo_orm::domain::parse_domain(&json!([
            ["is_won", "=", true],
            ["team_id", "=", team]
        ]))?;
        let found = ctx
            .registry
            .search(ctx.pool, "crm.stage", &domain, &opts)
            .await?;
        if let Some(stage) = found.first() {
            return Ok(Some(*stage));
        }
    }
    let domain = rusdoo_orm::domain::parse_domain(&json!([["is_won", "=", true]]))?;
    let found = ctx
        .registry
        .search(ctx.pool, "crm.stage", &domain, &opts)
        .await?;
    Ok(found.first().copied())
}

/// `action_set_lost(lost_reason_id=...)` — the deal is off.
///
/// Odoo asks for the reason in a dialog and refuses to close without
/// one; the refusal is the point of the feature, so it is here rather
/// than in the dialog: "we lost" with no why is a row nobody can learn
/// from.
fn action_set_lost<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "say which opportunity was lost".into(),
            ));
        }
        let reason = wanted(&ctx, kwargs, "lost_reason_id")
            .and_then(first_id)
            .ok_or_else(|| {
                RusdooError::Validation(
                    "say why it was lost: pass lost_reason_id with a crm.lost.reason".into(),
                )
            })?;
        // the reason has to be one: a dangling id would archive the deal
        // and record nothing
        let reasons = ctx
            .registry
            .read(ctx.pool, "crm.lost.reason", &[reason], &["name"])
            .await?;
        if reasons.is_empty() {
            return Err(RusdooError::Validation(format!(
                "there is no lost reason with id {reason}"
            )));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "crm.lead",
                &ctx.ids,
                vec![
                    ("probability", json!(0.0)),
                    ("lost_reason_id", json!(reason)),
                    ("date_closed", json!(now())),
                    ("active", json!(false)),
                ],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `convert_opportunity` — somebody decided the scrap of paper is worth
/// working on.
///
/// Odoo's own conversion wizard also merges duplicates and creates the
/// contact; what is ported is the part that changes the record: it stops
/// being a lead and enters the pipeline at its first stage.
fn convert_opportunity<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        use rusdoo_orm::crud::SearchOptions;
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation("say which lead to convert".into()));
        }
        let leads = ctx
            .registry
            .read(ctx.pool, "crm.lead", &ctx.ids, &["type"])
            .await?;
        if let Some(already) = leads
            .iter()
            .find(|lead| lead.get("type").and_then(Value::as_str) == Some("opportunity"))
        {
            return Err(RusdooError::Validation(format!(
                "lead {} is already an opportunity",
                already["id"]
            )));
        }
        let first = ctx
            .registry
            .search(
                ctx.pool,
                "crm.stage",
                &rusdoo_orm::domain::parse_domain(&json!([]))?,
                &SearchOptions {
                    limit: Some(1),
                    ..SearchOptions::default()
                },
            )
            .await?;
        let mut values: Vec<(&str, Value)> = vec![("type", json!("opportunity"))];
        if let Some(stage) = first.first() {
            values.push(("stage_id", json!(stage)));
        }
        ctx.registry
            .write_as(ctx.pool, ctx.uid, "crm.lead", &ctx.ids, values)
            .await?;
        Ok(json!(true))
    })
}

/// The moment a deal closed, in the format the ORM stores.
fn now() -> String {
    chrono::Utc::now()
        .format(defaults::DATETIME_FORMAT)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probability_is_a_percentage() {
        let check = |value: f64| {
            let mut record = Map::new();
            record.insert("probability".into(), json!(value));
            probability_is_a_percentage(&record)
        };
        assert!(check(0.0).is_ok());
        assert!(check(100.0).is_ok());
        assert!(check(-1.0).is_err());
        assert!(check(101.0).is_err());
    }

    #[test]
    fn a_deal_is_not_worth_less_than_nothing() {
        let mut record = Map::new();
        record.insert("expected_revenue".into(), json!(-0.01));
        assert!(revenue_is_not_negative(&record).is_err());
        record.insert("expected_revenue".into(), json!(0.0));
        assert!(revenue_is_not_negative(&record).is_ok());
    }

    #[test]
    fn the_pipeline_is_ordered_the_way_a_salesperson_reads_it() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_utm::extend(&mut reg).unwrap();
        rusdoo_sales_team::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        let lead = reg.get("crm.lead").expect("registered");
        assert_eq!(lead.order(), "priority desc, id desc");
        // a lead and an opportunity are one model told apart by `type`
        assert!(lead.field("type").is_some_and(|f| f.required));
        assert_eq!(reg.get("crm.stage").unwrap().order(), "sequence, name, id");
    }
}

//! `account.analytic.account` and `account.analytic.line` — port of
//! `odoo/addons/analytic/models/analytic_account.py` and
//! `analytic_line.py`.
//!
//! An account is one value on an axis: the *Website redesign* project,
//! the *Support* department. A line is one posting against it, and the
//! sum of the lines is the only reason an analytic account exists at
//! all — it is what somebody looks at when they ask what that project
//! cost.

use crate::{first_id, m2o, meta, o2m, text};
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::registry::Registry;
use rusdoo_orm::{defaults, model::Model};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::HashMap;

/// The amounts posted against an account, as the dependency arrives.
fn amounts(record: &Map<String, Value>) -> Vec<f64> {
    record
        .get("line_ids.amount")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// `credit` — what was posted *to* the account.
///
/// Odoo converts each line from its own currency before adding; there is
/// no `res.currency` in this port yet, so every amount is taken in the
/// company's own money. Which currencies were involved is exactly the
/// kind of thing that must not be guessed, so this is a gap and not an
/// approximation: with one currency the number is right, and with two it
/// is not computed at all rather than computed wrongly.
fn credit(record: &Map<String, Value>) -> Value {
    json!(amounts(record).iter().filter(|a| **a >= 0.0).sum::<f64>())
}

/// `debit` — what was posted *out of* the account, as a positive number.
fn debit(record: &Map<String, Value>) -> Value {
    json!(-amounts(record).iter().filter(|a| **a < 0.0).sum::<f64>())
}

/// `balance` — credit less debit, which is the sum of everything posted.
fn balance(record: &Map<String, Value>) -> Value {
    json!(amounts(record).iter().sum::<f64>())
}

/// `display_name` — "[R&D] Website redesign - Acme", port of
/// `_compute_display_name`.
///
/// Stored, unlike the counts on the plan: it only ever changes when this
/// record's own fields change, which is exactly what the recompute
/// follows. Odoo reaches the partner's *commercial* partner (the company
/// a contact belongs to); there is no such field here, so this is the
/// partner itself.
fn display_name(record: &Map<String, Value>) -> Value {
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut display = match record.get("code").and_then(Value::as_str) {
        Some(code) if !code.trim().is_empty() => format!("[{code}] {name}"),
        _ => name.to_string(),
    };
    if let Some(partner) = record
        .get("partner_id")
        .and_then(Value::as_array)
        .and_then(|pair| pair.get(1))
        .and_then(Value::as_str)
        .filter(|partner| !partner.is_empty())
    {
        display = format!("{display} - {partner}");
    }
    json!(display)
}

/// `account.analytic.account` — one value on one axis.
pub fn account() -> Model {
    Model::new(
        meta("account.analytic.account", "account_analytic_account"),
        vec![
            text("name").required().translatable(),
            // the short reference people actually type
            text("code"),
            // an account that stopped being used is archived, never
            // deleted: the lines posted against it still name it
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // restrict, so that deleting an axis cannot silently orphan
            // the values on it
            m2o("plan_id", "account.analytic.plan")
                .required()
                .ondelete(OnDelete::Restrict),
            // the axis this account really belongs to, whichever subplan
            // it was filed under
            m2o("root_plan_id", "account.analytic.plan").related("plan_id.root_id"),
            Field::new("color", FieldType::Integer).related("plan_id.color"),
            o2m("line_ids", "account.analytic.line", "account_id"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("partner_id", "res.partner"),
            Field::new("balance", FieldType::Monetary).computed(&["line_ids.amount"], balance),
            Field::new("debit", FieldType::Monetary).computed(&["line_ids.amount"], debit),
            Field::new("credit", FieldType::Monetary).computed(&["line_ids.amount"], credit),
            // Not stored, and not `store()`d on purpose. Two reasons,
            // both found in review:
            //
            // * it would freeze one language. A stored compute is
            //   recomputed through the write path, which reads its
            //   dependencies in the source language; `name` is
            //   translated, so the column would hold English for every
            //   reader forever.
            // * it would not be the label anyway. This framework's
            //   many2one label prefers a `name` column when the model
            //   has one (`db.rs::display_names_lang`), so a line's
            //   `account_id` reads `[1, "Website"]` and never
            //   `"[WEB] Website"` — the column would be paid for and
            //   unread.
            //
            // Odoo's `_compute_display_name` *is* the many2one label,
            // which is its whole purpose. Making that true here is a
            // framework change, not an addon one: `display_names_lang`
            // has to prefer a model's `display_name` over its `name`.
            // Tracked rather than worked around.
            text("display_name").computed(&["name", "code", "partner_id"], display_name),
        ],
    )
    // Odoo orders by `plan_id, name asc`. Ordering happens in SQL and
    // `name` is a translated column — a jsonb map of one value per
    // language — so ordering by it would sort by the shape of the map
    // and not by the text anybody reads.
    .ordered("plan_id, id")
    .constrained("analytic_account_named", &["name"], crate::is_named)
}

/// `account.analytic.line` — one posting against an account.
pub fn line() -> Model {
    Model::new(
        meta("account.analytic.line", "account_analytic_line"),
        vec![
            text("name").required(),
            Field::new("date", FieldType::Date)
                .required()
                .default_from(defaults::TODAY),
            Field::new("amount", FieldType::Monetary)
                .required()
                .default_value(json!(0.0)),
            Field::new("unit_amount", FieldType::Float { digits: None }).default_value(json!(0.0)),
            m2o("product_uom_id", "uom.uom"),
            m2o("partner_id", "res.partner"),
            m2o("user_id", "res.users").default_from(defaults::CURRENT_USER),
            m2o("company_id", "res.company")
                .required()
                .default_from(defaults::USER_COMPANY),
            // Odoo's `_check_account_id` refuses a line that names no
            // account on any plan. With one column instead of one per
            // plan, that rule is exactly `required`.
            m2o("account_id", "account.analytic.account")
                .required()
                .ondelete(OnDelete::Restrict),
            Field::new(
                "category",
                FieldType::Selection(vec![("other".into(), "Other".into())]),
            )
            .default_value(json!("other")),
        ],
    )
    .ordered("date desc, id desc")
}

/// The root plan of each of these accounts, keyed by account.
///
/// Accounts that no longer exist are simply absent, which is what the
/// callers want: a distribution that still mentions a deleted account
/// must go on working, and Odoo says the same thing with `.exists()`.
pub async fn root_plans_of(
    registry: &Registry,
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, i64>, RusdooError> {
    let mut wanted: Vec<i64> = account_ids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = registry
        .read(pool, "account.analytic.account", &wanted, &["root_plan_id"])
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let account = row.get("id").and_then(Value::as_i64)?;
            let root = row.get("root_plan_id").and_then(first_id)?;
            Some((account, root))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_amounts(values: Value) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("line_ids.amount".into(), values);
        record
    }

    #[test]
    fn an_account_adds_up_what_was_posted_against_it() {
        let record = with_amounts(json!([100.0, -30.0, 5.5]));
        assert_eq!(credit(&record), json!(105.5));
        assert_eq!(debit(&record), json!(30.0));
        assert_eq!(balance(&record), json!(75.5));
    }

    #[test]
    fn an_account_nobody_posted_against_is_at_zero_not_null() {
        let empty = Map::new();
        assert_eq!(credit(&empty), json!(0.0));
        assert_eq!(debit(&empty), json!(0.0));
        assert_eq!(balance(&empty), json!(0.0));
    }

    #[test]
    fn a_zero_line_counts_as_credit_like_odoo_reads_it() {
        // Odoo's split is `amount >= 0` against `amount < 0`: a zero
        // amount lands on the credit side, and moving it would change
        // every gross-margin figure by nothing except where it is shown
        let record = with_amounts(json!([0.0]));
        assert_eq!(credit(&record), json!(0.0));
        assert_eq!(debit(&record), json!(0.0));
    }

    #[test]
    fn an_accounts_name_says_its_code_and_its_customer() {
        let mut record = Map::new();
        record.insert("name".into(), json!("Website redesign"));
        assert_eq!(display_name(&record), json!("Website redesign"));

        record.insert("code".into(), json!("WEB"));
        assert_eq!(display_name(&record), json!("[WEB] Website redesign"));

        // a many2one reads as [id, name]
        record.insert("partner_id".into(), json!([7, "Acme"]));
        assert_eq!(
            display_name(&record),
            json!("[WEB] Website redesign - Acme")
        );
    }

    #[test]
    fn a_blank_code_does_not_become_empty_brackets() {
        let mut record = Map::new();
        record.insert("name".into(), json!("Support"));
        record.insert("code".into(), json!("  "));
        record.insert("partner_id".into(), Value::Null);
        assert_eq!(display_name(&record), json!("Support"));
    }
}

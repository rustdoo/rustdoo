//! `account.analytic.plan` and `account.analytic.applicability` — port of
//! `odoo/addons/analytic/models/analytic_plan.py`.
//!
//! A plan is one axis of analysis: Projects, Departments, Vehicles. Under
//! it hangs a tree of subplans, and the accounts of the whole tree belong
//! to the axis at its root — which is why almost everything here is asked
//! of the *root* plan and not of the plan somebody happened to pick.
//!
//! An applicability is the answer to "must this document say something
//! about this axis?", decided per business domain and per company, with
//! the plan's own `default_applicability` as the answer when no rule
//! fits.

use crate::{first_id, first_linked_id, m2o, meta, o2m, text};
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use rusdoo_orm::model::Model;
use serde_json::{json, Map, Value};

/// The three answers a plan can give about a document.
pub const OPTIONAL: &str = "optional";
pub const MANDATORY: &str = "mandatory";
pub const UNAVAILABLE: &str = "unavailable";

fn applicability_selection() -> FieldType {
    FieldType::Selection(vec![
        (OPTIONAL.into(), "Optional".into()),
        (MANDATORY.into(), "Mandatory".into()),
        (UNAVAILABLE.into(), "Unavailable".into()),
    ])
}

/// Odoo ships exactly one business domain in this module (`general`);
/// the others — sales order, purchase order, invoice — are added by the
/// modules that own those documents, through a selection extension this
/// registry has no equivalent for yet.
fn business_domain_selection() -> FieldType {
    FieldType::Selection(vec![("general".into(), "Miscellaneous".into())])
}

// ---------------------------------------------------------------------
// Computes
// ---------------------------------------------------------------------

/// `complete_name` — "Projects / Internal / R&D", the name a subplan is
/// recognizable by.
///
/// Not stored, unlike Odoo. A stored one is recursive: renaming a plan
/// has to rewrite the name of every plan beneath it, and the recompute
/// here only follows the record being written. A column that silently
/// stops matching its own tree is worse than a column that is not there.
fn complete_name(record: &Map<String, Value>) -> Value {
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let parent = record
        .get("parent_id.complete_name")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .unwrap_or_default();
    if parent.is_empty() {
        return json!(name);
    }
    json!(format!("{parent} / {name}"))
}

/// `root_id` — the plan at the top of this one's tree, which is the axis
/// the accounts underneath actually belong to.
///
/// Odoo reads it off `parent_path`, the materialized `1/4/7/` column of
/// `_parent_store`. There is no `_parent_store` here, so this walks: a
/// plan with no parent is its own root, and any other asks its parent.
fn root_id(record: &Map<String, Value>) -> Value {
    match first_linked_id(record.get("parent_id.root_id")) {
        Some(root) => json!(root),
        None => record.get("id").cloned().unwrap_or(Value::Null),
    }
}

fn counted(record: &Map<String, Value>, field: &str) -> usize {
    record
        .get(field)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn children_count(record: &Map<String, Value>) -> Value {
    json!(counted(record, "children_ids"))
}

fn account_count(record: &Map<String, Value>) -> Value {
    json!(counted(record, "account_ids"))
}

/// `all_account_count` — the accounts of this plan and of every plan
/// beneath it.
///
/// Odoo asks the database for the whole subtree at once with a
/// `parent_path LIKE` join. This adds up what each child already
/// answered, which reaches the same number over a tree of any depth and
/// needs no materialized path to do it.
fn all_account_count(record: &Map<String, Value>) -> Value {
    let below: i64 = record
        .get("children_ids.all_account_count")
        .and_then(Value::as_array)
        .map(|counts| counts.iter().filter_map(Value::as_i64).sum())
        .unwrap_or(0);
    json!(counted(record, "account_ids") as i64 + below)
}

/// A plan that is its own parent.
///
/// Odoo leans on `_parent_store`'s own recursion check. Here it matters
/// more than a tidy tree: `root_id` and `all_account_count` walk the
/// hierarchy, and a plan pointing at itself would walk forever.
fn not_its_own_parent(record: &Map<String, Value>) -> Result<(), String> {
    let parent = record.get("parent_id").and_then(first_id);
    let id = record.get("id").and_then(Value::as_i64);
    match (parent, id) {
        (Some(parent), Some(id)) if parent == id => {
            Err("a plan cannot be its own parent: the tree would have no root".into())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------

/// `account.analytic.plan` — one axis of analysis.
pub fn plan() -> Model {
    Model::new(
        meta("account.analytic.plan", "account_analytic_plan"),
        vec![
            text("name").required().translatable(),
            Field::new("description", FieldType::Text),
            // a subplan goes with the plan it belongs to: on its own it
            // names an axis that no longer exists
            m2o("parent_id", "account.analytic.plan").ondelete(OnDelete::Cascade),
            o2m("children_ids", "account.analytic.plan", "parent_id"),
            o2m("account_ids", "account.analytic.account", "plan_id"),
            o2m(
                "applicability_ids",
                "account.analytic.applicability",
                "analytic_plan_id",
            ),
            text("complete_name").computed(&["name", "parent_id.complete_name"], complete_name),
            m2o("root_id", "account.analytic.plan").computed(&["id", "parent_id.root_id"], root_id),
            Field::new("children_count", FieldType::Integer)
                .computed(&["children_ids"], children_count),
            Field::new("account_count", FieldType::Integer)
                .computed(&["account_ids"], account_count),
            Field::new("all_account_count", FieldType::Integer).computed(
                &["account_ids", "children_ids.all_account_count"],
                all_account_count,
            ),
            // Odoo draws one of the eleven kanban colours at random; a
            // random default is not something a data file can reproduce,
            // so a new plan is uncoloured until somebody picks
            Field::new("color", FieldType::Integer).default_value(json!(0)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            // `company_dependent` in Odoo — one value per company, held in
            // `ir.default`. There is no company-dependent field here, so
            // this is the one answer the whole database gives.
            Field::new("default_applicability", applicability_selection())
                .required()
                .default_value(json!(OPTIONAL)),
        ],
    )
    .ordered("sequence asc, id")
    .constrained("analytic_plan_named", &["name"], crate::is_named)
    .constrained(
        "analytic_plan_not_its_own_parent",
        &["parent_id"],
        not_its_own_parent,
    )
}

/// `account.analytic.applicability` — when this axis is asked for.
pub fn applicability() -> Model {
    Model::new(
        meta(
            "account.analytic.applicability",
            "account_analytic_applicability",
        ),
        vec![
            // cascade, where Odoo leaves the default `set null`: a rule
            // about an axis that was deleted is a rule that can never
            // apply again, and it would keep answering questions about a
            // plan nobody can open
            m2o("analytic_plan_id", "account.analytic.plan")
                .required()
                .ondelete(OnDelete::Cascade),
            Field::new("business_domain", business_domain_selection()).required(),
            Field::new("applicability", applicability_selection()).required(),
            // no company means every company, which is why it is not
            // defaulted to the caller's
            m2o("company_id", "res.company"),
        ],
    )
}

// ---------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------

/// The single record a plan's button was pressed on.
fn only_one(ids: &[i64], complaint: &str) -> Result<i64, RusdooError> {
    match ids {
        [id] => Ok(*id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

/// `action_view_analytical_accounts` — the accounts of this plan and of
/// everything under it.
pub fn action_view_analytical_accounts<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let plan = only_one(&ctx.ids, "open the accounts of one plan at a time")?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Analytic Accounts",
            "res_model": "account.analytic.account",
            "view_mode": "list,form",
            // `child_of`, not `=`: the subplans' accounts belong to this
            // axis just as much as the ones written directly on it
            "domain": [["plan_id", "child_of", plan]],
            "context": {"default_plan_id": plan},
            "target": "current",
        }))
    })
}

/// `action_view_children_plans` — the subplans of this plan.
pub fn action_view_children_plans<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let plan = only_one(&ctx.ids, "open the subplans of one plan at a time")?;
        let rows = ctx
            .registry
            .read(ctx.pool, "account.analytic.plan", &[plan], &["color"])
            .await?;
        let color = rows
            .first()
            .and_then(|row| row.get("color"))
            .cloned()
            .unwrap_or(json!(0));
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Analytic Plans",
            "res_model": "account.analytic.plan",
            "view_mode": "list,form",
            "domain": [["parent_id", "=", plan]],
            // a subplan is born under this plan and in its colour: the
            // tree is read on a board, and a child in another colour
            // reads as another axis
            "context": {"default_parent_id": plan, "default_color": color},
            "target": "current",
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
        pairs
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }

    #[test]
    fn a_root_plan_is_its_own_root_and_keeps_its_bare_name() {
        let plan = record(vec![("id", json!(4)), ("name", json!("Projects"))]);
        assert_eq!(root_id(&plan), json!(4));
        assert_eq!(complete_name(&plan), json!("Projects"));
    }

    #[test]
    fn a_subplan_takes_its_root_and_its_path_from_its_parent() {
        // the dependency arrives as the ORM hands it over: a list with
        // the parent's value in it, a many2one as [id, name]
        let plan = record(vec![
            ("id", json!(9)),
            ("name", json!("R&D")),
            ("parent_id.root_id", json!([[4, "Projects"]])),
            ("parent_id.complete_name", json!(["Projects / Internal"])),
        ]);
        assert_eq!(root_id(&plan), json!(4));
        assert_eq!(complete_name(&plan), json!("Projects / Internal / R&D"));
    }

    #[test]
    fn a_plan_counts_the_accounts_of_its_whole_tree() {
        let leaf = record(vec![("account_ids", json!([1]))]);
        assert_eq!(all_account_count(&leaf), json!(1));
        assert_eq!(account_count(&leaf), json!(1));

        let branch = record(vec![
            ("account_ids", json!([2])),
            ("children_ids", json!([9])),
            ("children_ids.all_account_count", json!([1])),
        ]);
        assert_eq!(account_count(&branch), json!(1), "its own accounts only");
        assert_eq!(all_account_count(&branch), json!(2), "and the subtree's");
        assert_eq!(children_count(&branch), json!(1));

        // a plan nobody put anything under counts zero, not null
        assert_eq!(all_account_count(&Map::new()), json!(0));
        assert_eq!(children_count(&Map::new()), json!(0));
    }

    #[test]
    fn a_plan_that_is_its_own_parent_is_refused() {
        let looped = record(vec![
            ("id", json!(4)),
            ("parent_id", json!([4, "Projects"])),
        ]);
        assert!(not_its_own_parent(&looped).is_err());
        let fine = record(vec![
            ("id", json!(9)),
            ("parent_id", json!([4, "Projects"])),
        ]);
        assert!(not_its_own_parent(&fine).is_ok());
        let root = record(vec![("id", json!(4)), ("parent_id", Value::Null)]);
        assert!(not_its_own_parent(&root).is_ok());
    }
}

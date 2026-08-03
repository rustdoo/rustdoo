//! rusdoo-analytic — port of `odoo/addons/analytic/`: where the money
//! went, told in a language the chart of accounts does not speak.
//!
//! A general account says *what kind* of expense something was; an
//! analytic account says *what it was for* — this project, that
//! department, that vehicle. The two live side by side and never have to
//! agree, which is the whole point: a company reorganizes its cost
//! centres far more often than it rewrites its chart of accounts.
//!
//! Five models carry that:
//!
//! * `account.analytic.plan` — one axis of analysis, and a tree of
//!   subplans under it.
//! * `account.analytic.applicability` — when a plan is optional, when it
//!   is mandatory, and when it should not be offered at all.
//! * `account.analytic.account` — one value on that axis.
//! * `account.analytic.line` — one posting against an account.
//! * `account.analytic.distribution.model` — a standing rule that
//!   prefills a distribution for a partner or a company.
//!
//! The distribution itself — `{"3,7": 50, "9": 50}`, the JSON that a
//! dozen addons hang their analytics on — is in [`distribution`], with
//! the merge algebra that Odoo runs when two rules apply to one
//! document.
//!
//! ## The one deviation worth knowing about
//!
//! In Odoo, every root plan grows a *column of its own* on
//! `account.analytic.line` and on every model with the analytic mixin:
//! `account_id` for the project plan, `x_plan37_id` for the rest,
//! created at runtime through `ir.model.fields` with `state='manual'`.
//! That machinery is what lets one line be tagged on several axes at
//! once, and it needs a registry that can gain a field while the server
//! is running. This one cannot: models here are Rust, checked by the
//! compiler, and a field that appears at runtime has nowhere to live.
//!
//! So a line carries exactly one account, in `account_id`, and the
//! multi-axis story lives entirely in `analytic_distribution` — which is
//! ported whole, because that is the part the rest of the system reads.
//! `_column_name`, `_sync_plan_column`, `auto_account_id`, the
//! `analytic.project_plan` parameter that decides which plan owns the
//! static column, and the line's distribution-splitting inverse are all
//! absent for the same reason; see the crate's report.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::model::ModelMeta;
use rusdoo_orm::registry::Registry;
use serde_json::Value;

pub mod account;
pub mod applicability;
pub mod distribution;
pub mod plan;

/// A `Char` field, the shape most of these models are made of.
pub(crate) fn text(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

pub(crate) fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

pub(crate) fn o2m(name: &str, comodel: &str, inverse: &str) -> Field {
    Field::new(
        name,
        FieldType::One2many {
            comodel: comodel.to_string(),
            inverse: inverse.to_string(),
        },
    )
}

pub(crate) fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The id out of a many2one value, which reads as `[id, name]`.
pub(crate) fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// The id at the head of a dependency read over a relation, which the
/// ORM hands over as a list of values — one per linked record.
pub(crate) fn first_linked_id(value: Option<&Value>) -> Option<i64> {
    value
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(first_id)
}

/// A record whose name is blank: the database accepts `''` where it
/// refuses NULL, and what is left is a row nobody can find in a list.
pub(crate) fn is_named(record: &serde_json::Map<String, Value>) -> Result<(), String> {
    match record.get("name").and_then(Value::as_str).map(str::trim) {
        Some(name) if !name.is_empty() => Ok(()),
        _ => Err("give it a name: an analytic record with a blank name is unfindable".into()),
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(plan::plan())?;
    reg.register(plan::applicability())?;
    reg.register(account::account())?;
    reg.register(account::line())?;
    reg.register(distribution::distribution_model())?;
    Ok(())
}

/// What analytics lets somebody do from a screen.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // reading which plans apply, and where their accounts are, never
    // writes anything: they are the screen deciding what to draw
    methods.register(
        "account.analytic.plan",
        "get_relevant_plans",
        Operation::Read,
        applicability::get_relevant_plans,
    )?;
    methods.register(
        "account.analytic.plan",
        "action_view_analytical_accounts",
        Operation::Read,
        plan::action_view_analytical_accounts,
    )?;
    methods.register(
        "account.analytic.plan",
        "action_view_children_plans",
        Operation::Read,
        plan::action_view_children_plans,
    )?;
    methods.register(
        "account.analytic.distribution.model",
        "get_distribution",
        Operation::Read,
        distribution::get_distribution,
    )?;
    methods.register(
        "account.analytic.distribution.model",
        "validate_distribution",
        Operation::Read,
        distribution::validate_distribution_method,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_five_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in [
            "account.analytic.plan",
            "account.analytic.applicability",
            "account.analytic.account",
            "account.analytic.line",
            "account.analytic.distribution.model",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }

        // an account without a plan is an account on no axis at all
        let account = reg.get("account.analytic.account").unwrap();
        assert!(account.field("plan_id").unwrap().required);
        // the root plan is read through the plan, never stored twice
        assert_eq!(
            account.field("root_plan_id").unwrap().related.as_deref(),
            Some("plan_id.root_id")
        );
        // a line posts against exactly one account here: the per-plan
        // columns Odoo creates at runtime have nowhere to live
        let line = reg.get("account.analytic.line").unwrap();
        assert!(line.field("account_id").unwrap().required);
        assert!(line.field("auto_account_id").is_none());
        // and the distribution is a column, not a compute: it is what
        // every other module writes into
        let model = reg.get("account.analytic.distribution.model").unwrap();
        let distribution = model.field("analytic_distribution").unwrap();
        assert!(distribution.stored);
        assert!(matches!(distribution.ty, FieldType::Json));
    }

    #[test]
    fn the_plan_is_a_tree_whose_counts_are_never_stale() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        let plan = reg.get("account.analytic.plan").unwrap();
        // none of the counts has a column: they change when ANOTHER
        // record is written (an account, a subplan), and the recompute
        // only follows the fields of what is being written
        for name in [
            "children_count",
            "account_count",
            "all_account_count",
            "complete_name",
            "root_id",
        ] {
            let field = plan.field(name).unwrap();
            assert!(field.compute.is_some(), "{name} is computed");
            assert!(!field.stored, "{name} would go stale in a column");
        }
    }

    #[test]
    fn the_screens_have_their_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("account.analytic.plan"),
            vec![
                "action_view_analytical_accounts",
                "action_view_children_plans",
                "get_relevant_plans",
            ]
        );
        assert_eq!(
            methods.names_for("account.analytic.distribution.model"),
            vec!["get_distribution", "validate_distribution"]
        );
    }

    #[test]
    fn a_many2one_gives_up_its_id_whichever_shape_it_arrives_in() {
        assert_eq!(first_id(&json!([7, "Plan"])), Some(7));
        assert_eq!(first_id(&json!(7)), Some(7));
        assert_eq!(first_id(&Value::Null), None);
        // a dependency read over a relation arrives as a list
        assert_eq!(first_linked_id(Some(&json!([[7, "Plan"]]))), Some(7));
        assert_eq!(first_linked_id(Some(&json!([]))), None);
        assert_eq!(first_linked_id(None), None);
    }

    #[test]
    fn a_blank_name_is_refused() {
        let mut record = serde_json::Map::new();
        record.insert("name".into(), json!("   "));
        assert!(is_named(&record).is_err());
        record.insert("name".into(), json!("Engineering"));
        assert!(is_named(&record).is_ok());
    }
}

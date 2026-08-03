//! When a plan applies — port of `account.analytic.applicability`'s
//! `_get_score` and of `account.analytic.plan.get_relevant_plans`.
//!
//! A screen that carries analytics has to ask two things before it can
//! draw anything: which axes exist for this kind of document, and whether
//! each of them is optional, mandatory, or not to be offered at all. Both
//! answers come from here.

use crate::first_id;
use crate::plan::{OPTIONAL, UNAVAILABLE};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::HashMap;

/// One applicability rule, as the decision below needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub business_domain: String,
    pub applicability: String,
    pub company_id: Option<i64>,
}

/// Whether a rule is addressed to the company being asked about.
///
/// A rule with no company speaks for all of them, and a question with no
/// company is asked of all of them.
pub fn rule_applies_to_company(rule_company: Option<i64>, wanted_company: Option<i64>) -> bool {
    match (rule_company, wanted_company) {
        (Some(rule), Some(wanted)) => rule == wanted,
        _ => true,
    }
}

/// How well a rule fits the question, port of `_get_score`.
///
/// Half a point for matching on company, because a company match is worth
/// less than a domain match when both rules fit equally well; a full
/// point on top for the right business domain, and a flat refusal (-1)
/// for the wrong one — a rule about purchase orders must never win a
/// question about invoices, however specific it is otherwise.
pub fn applicability_score(
    rule_business_domain: &str,
    rule_company: Option<i64>,
    wanted_business_domain: Option<&str>,
    wanted_company: Option<i64>,
) -> f64 {
    let score = if rule_company.is_some() && wanted_company.is_some() {
        0.5
    } else {
        0.0
    };
    let Some(wanted) = wanted_business_domain else {
        return score;
    };
    if wanted == rule_business_domain {
        score + 1.0
    } else {
        -1.0
    }
}

/// The applicability of the best-fitting rule, or the plan's default.
///
/// A rule has to *beat* zero to win: a score of zero is what a rule that
/// matches nothing in particular gets, and the plan's own default is
/// already a better answer than that.
pub fn best_applicability(
    default_applicability: &str,
    rules: &[Rule],
    wanted_business_domain: Option<&str>,
    wanted_company: Option<i64>,
) -> String {
    let mut best = default_applicability.to_string();
    let mut best_score = 0.0;
    for rule in rules {
        if !rule_applies_to_company(rule.company_id, wanted_company) {
            continue;
        }
        let score = applicability_score(
            &rule.business_domain,
            rule.company_id,
            wanted_business_domain,
            wanted_company,
        );
        if score > best_score {
            best = rule.applicability.clone();
            best_score = score;
        }
    }
    best
}

/// What is being asked of the plans, port of `get_relevant_plans`' kwargs.
#[derive(Debug, Clone, Default)]
pub struct PlanQuery {
    pub business_domain: Option<String>,
    pub company_id: Option<i64>,
    /// Odoo's `applicability` kwarg: forces the answer for every plan. A
    /// distribution *model* wants every axis on screen whatever the rules
    /// say, because it is being written for documents that have not
    /// happened yet.
    pub forced_applicability: Option<String>,
    /// accounts the document already carries. Their axes stay on screen
    /// even when the rules turned against them, or the percentage
    /// somebody typed would vanish without a word.
    pub existing_account_ids: Vec<i64>,
}

impl PlanQuery {
    /// The query a client sent, out of the kwargs of `call_kw`.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> PlanQuery {
        PlanQuery {
            business_domain: kwargs
                .get("business_domain")
                .and_then(Value::as_str)
                .map(str::to_string),
            company_id: kwargs.get("company_id").and_then(Value::as_i64),
            forced_applicability: kwargs
                .get("applicability")
                .and_then(Value::as_str)
                .map(str::to_string),
            existing_account_ids: kwargs
                .get("existing_account_ids")
                .and_then(Value::as_array)
                .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default(),
        }
    }
}

/// One plan, as the screen that draws the analytic widget needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct RelevantPlan {
    pub id: i64,
    pub name: String,
    pub color: i64,
    pub sequence: i64,
    pub applicability: String,
    pub all_account_count: i64,
}

impl RelevantPlan {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "color": self.color,
            "applicability": self.applicability,
            "all_account_count": self.all_account_count,
        })
    }
}

/// Every root plan a document should be offered, port of
/// `get_relevant_plans`.
///
/// A plan with no accounts anywhere in its tree is not offered: an axis
/// with nothing to choose from is a field the user can only leave empty.
pub async fn relevant_plans(
    registry: &Registry,
    pool: &PgPool,
    query: &PlanQuery,
) -> Result<Vec<RelevantPlan>, RusdooError> {
    let root_ids = registry
        .search(
            pool,
            "account.analytic.plan",
            &parse_domain(&json!([["parent_id", "=", false]]))?,
            &SearchOptions::default(),
        )
        .await?;
    if root_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rules = rules_by_plan(registry, pool, &root_ids).await?;
    let rows = registry
        .read(
            pool,
            "account.analytic.plan",
            &root_ids,
            &[
                "name",
                "color",
                "sequence",
                "default_applicability",
                "all_account_count",
            ],
        )
        .await?;

    let mut offered: Vec<RelevantPlan> = Vec::new();
    for row in &rows {
        let Some(id) = row.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let default_applicability = row
            .get("default_applicability")
            .and_then(Value::as_str)
            .unwrap_or(OPTIONAL);
        let applicability = match &query.forced_applicability {
            Some(forced) => forced.clone(),
            None => best_applicability(
                default_applicability,
                rules.get(&id).map(Vec::as_slice).unwrap_or(&[]),
                query.business_domain.as_deref(),
                query.company_id,
            ),
        };
        let all_account_count = row
            .get("all_account_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if all_account_count == 0 || applicability == UNAVAILABLE {
            continue;
        }
        offered.push(RelevantPlan {
            id,
            name: row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            color: row.get("color").and_then(Value::as_i64).unwrap_or(0),
            sequence: row.get("sequence").and_then(Value::as_i64).unwrap_or(0),
            applicability,
            all_account_count,
        });
    }

    // the axes the document is already tagged on come back even when the
    // rules would hide them — as optional, because the percentage that is
    // already there may not be the one the rules would have imposed
    let forced = forced_plans(registry, pool, query, &offered).await?;
    offered.extend(forced);
    offered.sort_by_key(|plan| (plan.sequence, plan.id));
    Ok(offered)
}

/// The plans of the accounts a document already carries that the rules
/// left out.
async fn forced_plans(
    registry: &Registry,
    pool: &PgPool,
    query: &PlanQuery,
    offered: &[RelevantPlan],
) -> Result<Vec<RelevantPlan>, RusdooError> {
    if query.existing_account_ids.is_empty() {
        return Ok(Vec::new());
    }
    let roots = crate::account::root_plans_of(registry, pool, &query.existing_account_ids).await?;
    let mut wanted: Vec<i64> = roots
        .values()
        .copied()
        .filter(|root| !offered.iter().any(|plan| plan.id == *root))
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let rows = registry
        .read(
            pool,
            "account.analytic.plan",
            &wanted,
            &["name", "color", "sequence", "all_account_count"],
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(RelevantPlan {
                id: row.get("id").and_then(Value::as_i64)?,
                name: row
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                color: row.get("color").and_then(Value::as_i64).unwrap_or(0),
                sequence: row.get("sequence").and_then(Value::as_i64).unwrap_or(0),
                applicability: OPTIONAL.to_string(),
                all_account_count: row
                    .get("all_account_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            })
        })
        .collect())
}

/// The applicability rules of each plan, in one search and one read.
async fn rules_by_plan(
    registry: &Registry,
    pool: &PgPool,
    plan_ids: &[i64],
) -> Result<HashMap<i64, Vec<Rule>>, RusdooError> {
    let ids = registry
        .search(
            pool,
            "account.analytic.applicability",
            &parse_domain(&json!([["analytic_plan_id", "in", plan_ids]]))?,
            &SearchOptions::default(),
        )
        .await?;
    let mut by_plan: HashMap<i64, Vec<Rule>> = HashMap::new();
    if ids.is_empty() {
        return Ok(by_plan);
    }
    let rows = registry
        .read(
            pool,
            "account.analytic.applicability",
            &ids,
            &[
                "analytic_plan_id",
                "business_domain",
                "applicability",
                "company_id",
            ],
        )
        .await?;
    for row in &rows {
        let Some(plan) = row.get("analytic_plan_id").and_then(first_id) else {
            continue;
        };
        by_plan.entry(plan).or_default().push(Rule {
            business_domain: row
                .get("business_domain")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            applicability: row
                .get("applicability")
                .and_then(Value::as_str)
                .unwrap_or(OPTIONAL)
                .to_string(),
            company_id: row.get("company_id").and_then(first_id),
        });
    }
    Ok(by_plan)
}

/// `get_relevant_plans` — which axes this document should be tagged on.
///
/// A model-level call, like Odoo's `@api.model`: it answers about the
/// plans of the database, not about a recordset. Its arguments arrive as
/// kwargs, which is how the client sends them.
pub fn get_relevant_plans<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let query = PlanQuery::from_kwargs(kwargs);
        let plans = relevant_plans(&ctx.registry, ctx.pool, &query).await?;
        Ok(Value::Array(
            plans.iter().map(RelevantPlan::to_json).collect(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::MANDATORY;

    #[test]
    fn the_wrong_business_domain_loses_however_specific_the_rule_is() {
        // a company match is worth half a point ...
        assert_eq!(applicability_score("general", Some(1), None, Some(1)), 0.5);
        assert_eq!(applicability_score("general", None, None, Some(1)), 0.0);
        // ... the right domain a full one on top ...
        assert_eq!(
            applicability_score("general", Some(1), Some("general"), Some(1)),
            1.5
        );
        // ... and the wrong domain is a refusal, not a smaller score
        assert_eq!(
            applicability_score("general", Some(1), Some("purchase_order"), Some(1)),
            -1.0
        );
    }

    #[test]
    fn a_rule_with_no_company_speaks_for_every_company() {
        assert!(rule_applies_to_company(None, Some(7)));
        assert!(rule_applies_to_company(Some(7), None));
        assert!(rule_applies_to_company(Some(7), Some(7)));
        assert!(!rule_applies_to_company(Some(7), Some(9)));
    }

    #[test]
    fn the_plans_default_answers_when_no_rule_fits() {
        let rules = vec![Rule {
            business_domain: "general".into(),
            applicability: MANDATORY.into(),
            company_id: None,
        }];
        // the question names the domain the rule is about: the rule wins
        assert_eq!(
            best_applicability(OPTIONAL, &rules, Some("general"), None),
            MANDATORY
        );
        // it names another one: the rule is refused and the default holds
        assert_eq!(
            best_applicability(OPTIONAL, &rules, Some("purchase_order"), None),
            OPTIONAL
        );
        // it names none: the rule scores zero, which does not beat the
        // plan's own answer
        assert_eq!(best_applicability(OPTIONAL, &rules, None, None), OPTIONAL);
    }

    #[test]
    fn a_rule_of_another_company_does_not_answer_for_this_one() {
        let rules = vec![Rule {
            business_domain: "general".into(),
            applicability: MANDATORY.into(),
            company_id: Some(2),
        }];
        assert_eq!(
            best_applicability(OPTIONAL, &rules, Some("general"), Some(1)),
            OPTIONAL,
            "company 2's rule is not company 1's"
        );
        assert_eq!(
            best_applicability(OPTIONAL, &rules, Some("general"), Some(2)),
            MANDATORY
        );
        // asked without a company, the rule speaks: Odoo's own reading of
        // "no company in the question" is "every company"
        assert_eq!(
            best_applicability(OPTIONAL, &rules, Some("general"), None),
            MANDATORY
        );
    }

    #[test]
    fn a_query_is_read_out_of_the_kwargs_a_client_sends() {
        let kwargs: Map<String, Value> = json!({
            "business_domain": "general",
            "company_id": 3,
            "existing_account_ids": [7, 9],
        })
        .as_object()
        .unwrap()
        .clone();
        let query = PlanQuery::from_kwargs(&kwargs);
        assert_eq!(query.business_domain.as_deref(), Some("general"));
        assert_eq!(query.company_id, Some(3));
        assert_eq!(query.existing_account_ids, vec![7, 9]);
        assert_eq!(query.forced_applicability, None);

        // and a call with nothing in it asks about everything
        let empty = PlanQuery::from_kwargs(&Map::new());
        assert!(empty.business_domain.is_none());
        assert!(empty.existing_account_ids.is_empty());
    }
}

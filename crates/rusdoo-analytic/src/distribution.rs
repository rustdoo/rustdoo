//! The analytic distribution — port of
//! `odoo/addons/analytic/models/analytic_mixin.py` and
//! `analytic_distribution_model.py`.
//!
//! A distribution says how one document is shared out over the analytic
//! axes: `{"3,7": 50, "9": 50}` means half of it belongs to accounts 3
//! *and* 7 at once — one on each axis — and the other half to account 9.
//! It is a `jsonb` column and not a table on purpose: it travels with the
//! document it describes, and every module that carries analytics
//! (invoice lines, purchase lines, expenses) writes the same shape.
//!
//! The interesting part is [`merge_distribution`], which is what happens
//! when two standing rules both apply to one document and each one only
//! knows about its own axes. Odoo's answer is a cross product weighted by
//! the amounts, and it is ported here move for move.

use crate::{m2o, meta};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::defaults;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use rusdoo_orm::model::Model;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap, HashSet};

/// How many decimals a percentage keeps, Odoo's
/// `decimal.precision('Percentage Analytic')`.
///
/// A constant and not a row, because there is no `decimal.precision`
/// model in this port. Two digits is the value the addon ships.
pub const PERCENTAGE_DIGITS: i32 = 2;

/// A distribution: which accounts share a slice of the document, and how
/// much of it. The key is the *set* of accounts sharing that slice — one
/// per axis — kept sorted so that the same slice is always the same key.
pub type Distribution = BTreeMap<Vec<i64>, f64>;

/// A percentage, rounded the way the column stores it.
pub fn round_percentage(value: f64) -> f64 {
    let factor = 10f64.powi(PERCENTAGE_DIGITS);
    (value * factor).round() / factor
}

/// Read a distribution off the wire.
///
/// Odoo trusts the client and the JS widget here; this does not. A key
/// that is not a list of account ids, or a percentage that is not a
/// number, is a column that every later read has to guess about — and
/// the guess would be silent.
pub fn parse_distribution(value: &Value) -> Result<Distribution, String> {
    let mut distribution = Distribution::new();
    let map = match value {
        // Odoo's "no distribution" is a null column or `false`
        Value::Null | Value::Bool(false) => return Ok(distribution),
        Value::Object(map) => map,
        other => {
            return Err(format!(
                "an analytic distribution is an object of \"accounts\": percentage, got {other}"
            ))
        }
    };
    for (key, percentage) in map {
        let mut accounts = Vec::new();
        for part in key.split(',') {
            let id: i64 = part.trim().parse().map_err(|_| {
                format!("{key:?} is not a list of analytic account ids separated by commas")
            })?;
            if id <= 0 {
                return Err(format!("{key:?} names an impossible account id"));
            }
            accounts.push(id);
        }
        accounts.sort_unstable();
        accounts.dedup();
        let percentage = percentage
            .as_f64()
            .ok_or_else(|| format!("the share of {key:?} must be a number, got {percentage}"))?;
        *distribution.entry(accounts).or_insert(0.0) += percentage;
    }
    Ok(distribution)
}

/// Write a distribution back to the wire, rounded like the column.
pub fn distribution_json(distribution: &Distribution) -> Value {
    let mut map = Map::new();
    for (accounts, percentage) in distribution {
        let key = accounts
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        map.insert(key, json!(round_percentage(*percentage)));
    }
    Value::Object(map)
}

/// Every account a distribution mentions.
pub fn accounts_in(distribution: &Distribution) -> Vec<i64> {
    let mut accounts: Vec<i64> = distribution.keys().flatten().copied().collect();
    accounts.sort_unstable();
    accounts.dedup();
    accounts
}

/// How much of the document each *axis* got, port of the loop inside
/// `_validate_distribution`.
///
/// Accounts whose plan is unknown — a deleted account still named in an
/// old distribution — are left out rather than counted against some
/// axis they no longer belong to.
pub fn share_by_root_plan(
    distribution: &Distribution,
    root_of: &HashMap<i64, i64>,
) -> HashMap<i64, f64> {
    let mut share: HashMap<i64, f64> = HashMap::new();
    for (accounts, percentage) in distribution {
        for account in accounts {
            let Some(root) = root_of.get(account) else {
                continue;
            };
            *share.entry(*root).or_insert(0.0) += percentage;
        }
    }
    share
}

/// The mandatory axes this distribution does not fully cover, port of
/// `_validate_distribution`.
///
/// "Fully" is exactly 100 within the stored precision: 99.99 is a
/// document whose last cent belongs to nobody.
pub fn plans_short_of_full_distribution(
    distribution: &Distribution,
    root_of: &HashMap<i64, i64>,
    mandatory_plans: &[i64],
) -> Vec<i64> {
    let share = share_by_root_plan(distribution, root_of);
    mandatory_plans
        .iter()
        .copied()
        .filter(|plan| round_percentage(share.get(plan).copied().unwrap_or(0.0)) != 100.0)
        .collect()
}

/// Merge a new distribution into an old one, port of
/// `_merge_distribution` and `_modifiying_distribution_values`.
///
/// `changing_roots` is Odoo's `__update__` key: the axes the new
/// distribution is speaking about. `None` means it speaks about all of
/// them, and then it simply replaces what was there — which is what a
/// plain write does.
///
/// When it speaks about *some* axes, the two distributions have to be
/// crossed: the old one still decides the axes it kept, the new one
/// decides the ones it names, and every combination of the two gets the
/// product of their shares. Whichever side has the smaller total leaves
/// the remainder of the other side untouched, so that a rule about
/// departments cannot silently drop half of what a rule about projects
/// had already said.
///
/// Odoo takes the account ids of the two sides in the order it found
/// them; here the merged key is sorted, because the key is the identity
/// of a slice and two spellings of the same set would be two slices.
pub fn merge_distribution(
    old: &Distribution,
    new: &Distribution,
    changing_roots: Option<&HashSet<i64>>,
    root_of: &HashMap<i64, i64>,
) -> Distribution {
    let Some(changing) = changing_roots else {
        return new.clone();
    };
    // an account "stays" when its axis is one the new distribution says
    // nothing about — including, like Odoo, when its axis is unknown
    let stays = |account: &i64| -> bool {
        matches!(root_of.get(account), Some(root) if !changing.contains(root))
    };

    let (kept, kept_total) = project(old, |account| stays(account));
    let (incoming, incoming_total) = project(new, |account| !stays(account));

    // the side with less to say leaves the rest of the other side alone
    let (ratio, leftovers) = if kept_total > incoming_total {
        let covered = incoming_total / kept_total;
        (1.0, scaled(&kept, 1.0 - covered))
    } else if incoming_total > kept_total {
        let covered = kept_total / incoming_total;
        (covered, scaled(&incoming, 1.0 - covered))
    } else {
        (1.0, Distribution::new())
    };

    let mut merged = Distribution::new();
    // guarded, where Odoo divides straight away: with nothing kept there
    // is nothing to cross, and the division would be by zero
    if kept_total > 0.0 {
        for (old_accounts, old_share) in &kept {
            for (new_accounts, new_share) in &incoming {
                let mut accounts = old_accounts.clone();
                accounts.extend(new_accounts.iter().copied());
                accounts.sort_unstable();
                accounts.dedup();
                *merged.entry(accounts).or_insert(0.0) +=
                    ratio * old_share * new_share / kept_total;
            }
        }
    }
    // the untouched remainder wins over the cross product, as in Odoo's
    // `{...} | additional_vals`
    for (accounts, share) in leftovers {
        merged.insert(accounts, share);
    }
    merged
}

/// The distribution restricted to the accounts `keep` accepts, and how
/// much of the document that restriction covers.
fn project(distribution: &Distribution, keep: impl Fn(&i64) -> bool) -> (Distribution, f64) {
    let mut projected = Distribution::new();
    let mut total = 0.0;
    for (accounts, share) in distribution {
        let remaining: Vec<i64> = accounts.iter().copied().filter(|a| keep(a)).collect();
        if remaining.is_empty() {
            continue;
        }
        *projected.entry(remaining).or_insert(0.0) += share;
        total += share;
    }
    (projected, total)
}

fn scaled(distribution: &Distribution, factor: f64) -> Distribution {
    distribution
        .iter()
        .map(|(accounts, share)| (accounts.clone(), share * factor))
        .collect()
}

// ---------------------------------------------------------------------
// The mixin, and the model that stores the standing rules
// ---------------------------------------------------------------------

/// The fields `analytic.mixin` adds to whatever carries analytics.
///
/// Odoo makes this an `AbstractModel` and pulls it in with `_inherit`.
/// There are no abstract models in this registry — a model is a table —
/// so a mixin is what it really is: a list of fields another crate
/// splices into its own model. `account.move.line`, `purchase.order.line`
/// and the rest get their analytics by calling this.
pub fn analytic_distribution_fields() -> Vec<Field> {
    vec![Field::new("analytic_distribution", FieldType::Json)]
}

/// A distribution the shape of which nobody can read.
fn is_a_distribution(record: &Map<String, Value>) -> Result<(), String> {
    let Some(value) = record.get("analytic_distribution") else {
        return Ok(());
    };
    parse_distribution(value).map(|_| ())
}

/// `account.analytic.distribution.model` — a standing rule: "documents
/// of this partner, in this company, are shared out like this".
pub fn distribution_model() -> Model {
    let mut fields = vec![
        Field::new("sequence", FieldType::Integer).default_value(json!(10)),
        // cascade on all three: a rule whose condition names a partner or
        // a company that was deleted is a rule that can never fire again
        m2o("partner_id", "res.partner").ondelete(OnDelete::Cascade),
        m2o("company_id", "res.company")
            .ondelete(OnDelete::Cascade)
            .default_from(defaults::USER_COMPANY),
    ];
    fields.extend(analytic_distribution_fields());
    Model::new(
        meta(
            "account.analytic.distribution.model",
            "account_analytic_distribution_model",
        ),
        fields,
    )
    // most specific first: `sequence` is what an administrator drags,
    // and the newest rule wins a tie
    .ordered("sequence, id desc")
    .constrained(
        "analytic_distribution_shape",
        &["analytic_distribution"],
        is_a_distribution,
    )
}

// ---------------------------------------------------------------------
// Resolving the standing rules
// ---------------------------------------------------------------------

/// What a document knows about itself when it asks for a distribution.
#[derive(Debug, Clone, Default)]
pub struct DistributionQuery {
    pub partner_id: Option<i64>,
    pub company_id: Option<i64>,
}

impl DistributionQuery {
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> DistributionQuery {
        DistributionQuery {
            partner_id: kwargs.get("partner_id").and_then(Value::as_i64),
            company_id: kwargs.get("company_id").and_then(Value::as_i64),
        }
    }

    /// The domain that finds the rules bearing on this document.
    ///
    /// A rule matches when its condition is either the document's value
    /// or empty — empty meaning "any". Odoo writes it as
    /// `(fname, 'in', [value, False])`; spelled out here so that "unset"
    /// is `IS NULL` and never an id that happens to be false.
    fn domain(&self) -> Value {
        let mut terms: Vec<Value> = Vec::new();
        for (field, value) in [
            ("partner_id", self.partner_id),
            ("company_id", self.company_id),
        ] {
            match value {
                Some(id) => {
                    terms.push(json!("|"));
                    terms.push(json!([field, "=", id]));
                    terms.push(json!([field, "=", false]));
                }
                None => terms.push(json!([field, "=", false])),
            }
        }
        Value::Array(terms)
    }
}

/// The distribution to prefill a document with, port of
/// `_get_distribution`.
///
/// Every matching rule is applied in order, and a rule is skipped when
/// an earlier one already decided one of its axes: two rules disagreeing
/// about the same axis is a configuration mistake, and the more specific
/// one — the one that sorted first — is the one that meant it.
pub async fn distribution_for(
    registry: &Registry,
    pool: &PgPool,
    query: &DistributionQuery,
) -> Result<Distribution, RusdooError> {
    let ids = registry
        .search(
            pool,
            "account.analytic.distribution.model",
            &parse_domain(&query.domain())?,
            &SearchOptions {
                order: Some("sequence, id desc".into()),
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(Distribution::new());
    }
    let rows = registry
        .read(
            pool,
            "account.analytic.distribution.model",
            &ids,
            &["analytic_distribution"],
        )
        .await?;

    // every account of every rule, resolved to its axis in one read
    let mut rules: Vec<Distribution> = Vec::new();
    let mut every_account: Vec<i64> = Vec::new();
    for row in &rows {
        let distribution =
            parse_distribution(row.get("analytic_distribution").unwrap_or(&Value::Null))
                .map_err(RusdooError::Validation)?;
        every_account.extend(accounts_in(&distribution));
        rules.push(distribution);
    }
    let root_of = crate::account::root_plans_of(registry, pool, &every_account).await?;

    let mut applied: HashSet<i64> = HashSet::new();
    let mut result = Distribution::new();
    for rule in &rules {
        let axes: HashSet<i64> = accounts_in(rule)
            .iter()
            .filter_map(|account| root_of.get(account).copied())
            .collect();
        if !applied.is_disjoint(&axes) {
            continue;
        }
        applied.extend(axes.iter().copied());
        result = merge_distribution(&result, rule, Some(&axes), &root_of);
    }
    Ok(result)
}

/// Refuse a distribution that leaves a mandatory axis unaccounted for,
/// port of `_validate_distribution`.
pub async fn validate_distribution(
    registry: &Registry,
    pool: &PgPool,
    distribution: &Distribution,
    plans: &crate::applicability::PlanQuery,
) -> Result<(), RusdooError> {
    let mandatory: Vec<i64> = crate::applicability::relevant_plans(registry, pool, plans)
        .await?
        .into_iter()
        .filter(|plan| plan.applicability == crate::plan::MANDATORY)
        .map(|plan| plan.id)
        .collect();
    if mandatory.is_empty() {
        return Ok(());
    }
    let root_of = crate::account::root_plans_of(registry, pool, &accounts_in(distribution)).await?;
    let short = plans_short_of_full_distribution(distribution, &root_of, &mandatory);
    if short.is_empty() {
        return Ok(());
    }
    Err(RusdooError::Validation(
        "one or more lines require a 100% analytic distribution".into(),
    ))
}

// ---------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------

/// `get_distribution` — the distribution a new document should start
/// with, given who it is for.
///
/// Odoo's is `_get_distribution`, called from Python by the modules that
/// prefill a line. The equivalent for another crate here is
/// [`distribution_for`]; this is the same thing reachable from a client,
/// which is what an onchange on the partner needs.
pub fn get_distribution<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let query = DistributionQuery::from_kwargs(kwargs);
        let distribution = distribution_for(&ctx.registry, ctx.pool, &query).await?;
        Ok(distribution_json(&distribution))
    })
}

/// `validate_distribution` — is what this record says complete?
pub fn validate_distribution_method<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "account.analytic.distribution.model",
                &ctx.ids,
                &["analytic_distribution"],
            )
            .await?;
        let plans = crate::applicability::PlanQuery::from_kwargs(kwargs);
        for row in &rows {
            let distribution =
                parse_distribution(row.get("analytic_distribution").unwrap_or(&Value::Null))
                    .map_err(RusdooError::Validation)?;
            validate_distribution(&ctx.registry, ctx.pool, &distribution, &plans).await?;
        }
        Ok(json!(true))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distribution(pairs: &[(&[i64], f64)]) -> Distribution {
        pairs
            .iter()
            .map(|(accounts, share)| (accounts.to_vec(), *share))
            .collect()
    }

    /// Accounts 1 and 2 on the Projects axis (plan 10), account 3 on the
    /// Departments axis (plan 20) — the shape Odoo's own test uses.
    fn two_axes() -> HashMap<i64, i64> {
        HashMap::from([(1, 10), (2, 10), (3, 20)])
    }

    #[test]
    fn a_distribution_reads_and_writes_the_shape_odoo_stores() {
        let parsed = parse_distribution(&json!({"3,7": 50, "9": 50.0})).unwrap();
        assert_eq!(parsed, distribution(&[(&[3, 7], 50.0), (&[9], 50.0)]));
        assert_eq!(distribution_json(&parsed), json!({"3,7": 50.0, "9": 50.0}));
        // the same slice spelled two ways is one slice
        let unsorted = parse_distribution(&json!({"7,3": 100})).unwrap();
        assert_eq!(unsorted.keys().next().unwrap(), &vec![3, 7]);
        // and "no distribution" is not an error
        assert!(parse_distribution(&Value::Null).unwrap().is_empty());
        assert!(parse_distribution(&json!(false)).unwrap().is_empty());
    }

    #[test]
    fn a_distribution_nobody_can_read_is_refused() {
        for garbage in [
            json!({"not an id": 100}),
            json!({"3,": 100}),
            json!({"0": 100}),
            json!({"3": "half"}),
            json!([3, 100]),
        ] {
            assert!(
                parse_distribution(&garbage).is_err(),
                "{garbage} is not a distribution"
            );
        }
    }

    #[test]
    fn without_an_update_key_the_new_distribution_simply_replaces() {
        let old = distribution(&[(&[1], 100.0)]);
        let new = distribution(&[(&[3], 100.0)]);
        assert_eq!(merge_distribution(&old, &new, None, &two_axes()), new);
    }

    #[test]
    fn a_rule_about_one_axis_leaves_the_other_axis_alone() {
        // what `_get_distribution` does with two rules: projects first,
        // then departments. This is Odoo's own
        // `test_order_analytic_distribution_model`, without the partner
        // category this port has no model for.
        let root_of = two_axes();
        let projects: HashSet<i64> = HashSet::from([10]);
        let departments: HashSet<i64> = HashSet::from([20]);

        // the first rule shares the document between two projects
        let first = merge_distribution(
            &Distribution::new(),
            &distribution(&[(&[1], 100.0), (&[2], 100.0)]),
            Some(&projects),
            &root_of,
        );
        assert_eq!(first, distribution(&[(&[1], 100.0), (&[2], 100.0)]));

        // the second names a department, and says nothing about projects
        let merged = merge_distribution(
            &first,
            &distribution(&[(&[3], 100.0)]),
            Some(&departments),
            &root_of,
        );
        assert_eq!(
            merged,
            distribution(&[(&[1, 3], 50.0), (&[2, 3], 50.0), (&[1], 50.0), (&[2], 50.0),]),
            "half the document is on the department, and the projects keep the rest"
        );
    }

    #[test]
    fn two_rules_that_each_cover_the_whole_document_are_simply_crossed() {
        let root_of = two_axes();
        let old = distribution(&[(&[1], 100.0)]);
        let new = distribution(&[(&[3], 100.0)]);
        let merged = merge_distribution(&old, &new, Some(&HashSet::from([20])), &root_of);
        assert_eq!(merged, distribution(&[(&[1, 3], 100.0)]));
    }

    #[test]
    fn a_new_axis_on_an_empty_document_is_taken_whole() {
        // nothing kept means nothing to cross: Odoo would divide by the
        // kept total here, and there is none
        let merged = merge_distribution(
            &Distribution::new(),
            &distribution(&[(&[3], 100.0)]),
            Some(&HashSet::from([20])),
            &two_axes(),
        );
        assert_eq!(merged, distribution(&[(&[3], 100.0)]));
    }

    #[test]
    fn clearing_every_percentage_answers_nothing_at_all() {
        // the wizard that empties the boxes sends `__update__` naming
        // every axis and no values. The merge answers an empty
        // distribution, and Odoo's caller reads that as "leave the
        // record alone" — which is what its own regression test checks.
        let old = distribution(&[(&[1, 3], 100.0)]);
        let merged = merge_distribution(
            &old,
            &Distribution::new(),
            Some(&HashSet::from([10, 20])),
            &two_axes(),
        );
        assert!(
            merged.is_empty(),
            "nothing was kept and nothing came in: {merged:?}"
        );
    }

    #[test]
    fn an_axis_the_new_rule_does_not_cover_keeps_its_remainder() {
        // the new side says more than the old one: the old is stretched
        // over what it covers and the surplus stays on the new axis alone
        let root_of = two_axes();
        let old = distribution(&[(&[1], 50.0)]);
        let new = distribution(&[(&[3], 100.0)]);
        let merged = merge_distribution(&old, &new, Some(&HashSet::from([20])), &root_of);
        assert_eq!(merged, distribution(&[(&[1, 3], 50.0), (&[3], 50.0)]));
    }

    #[test]
    fn the_axes_of_a_document_add_up_per_plan() {
        let root_of = two_axes();
        let shared = distribution(&[(&[1, 3], 60.0), (&[2, 3], 40.0)]);
        let share = share_by_root_plan(&shared, &root_of);
        assert_eq!(share.get(&10), Some(&100.0));
        assert_eq!(share.get(&20), Some(&100.0));
        assert!(plans_short_of_full_distribution(&shared, &root_of, &[10, 20]).is_empty());
    }

    #[test]
    fn a_mandatory_axis_left_half_empty_is_named() {
        let root_of = two_axes();
        let half = distribution(&[(&[1], 50.0)]);
        assert_eq!(
            plans_short_of_full_distribution(&half, &root_of, &[10, 20]),
            vec![10, 20],
            "the half-covered axis and the untouched one are both short"
        );
    }

    #[test]
    fn a_deleted_account_does_not_break_the_validation() {
        // Odoo's `test_validate_deleted_account`: a distribution that
        // still names an account nobody can browse must go on answering
        let root_of = HashMap::from([(2, 20)]);
        let stale = distribution(&[(&[1, 2], 100.0)]);
        assert!(plans_short_of_full_distribution(&stale, &root_of, &[20]).is_empty());
    }

    #[test]
    fn a_percentage_is_kept_to_the_precision_the_column_stores() {
        assert_eq!(round_percentage(33.333_333), 33.33);
        assert_eq!(round_percentage(99.995), 100.0);
        // and a share of exactly a third of a document does not count as
        // a full one
        let root_of = HashMap::from([(1, 10)]);
        let third = distribution(&[(&[1], 33.333)]);
        assert_eq!(
            plans_short_of_full_distribution(&third, &root_of, &[10]),
            vec![10]
        );
    }

    #[test]
    fn the_domain_asks_for_the_rules_that_bear_on_this_document() {
        let query = DistributionQuery {
            partner_id: Some(7),
            company_id: None,
        };
        assert_eq!(
            query.domain(),
            json!([
                "|",
                ["partner_id", "=", 7],
                ["partner_id", "=", false],
                ["company_id", "=", false]
            ])
        );
        // and it parses: a domain nobody can parse is a search that
        // silently matches everything
        assert!(parse_domain(&query.domain()).is_ok());
        assert!(parse_domain(&DistributionQuery::default().domain()).is_ok());
    }
}

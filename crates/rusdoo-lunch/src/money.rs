//! The wallet: what somebody paid in, what their orders took out, and
//! what is left.
//!
//! Port of `lunch_cashmove.py` and of the SQL view in
//! `report/lunch_cashmove_report.py`.
//!
//! The view is not ported as a view. Odoo's `lunch.cashmove.report` is a
//! `UNION ALL` of the cashmoves and the negated orders, declared with
//! `_auto = False` and created by hand in `init()`; this ORM has no
//! model without a table of its own. What the view exists *for* — the
//! balance — is computed here from the same two sources, and any screen
//! that wants the statement can read the two models it is made of.

use crate::{char, m2o, meta};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use rusdoo_orm::model::Model;
use serde_json::{json, Map, Value};

/// The states in which an order has already taken money out of the
/// wallet (`lunch_cashmove_report.py`: the view's `WHERE`). A `new` line
/// is a cart, not a debt; a cancelled one never was.
pub const SPENDING_STATES: [&str; 2] = ["ordered", "confirmed"];

/// `lunch.cashmove` — money paid into somebody's lunch wallet.
///
/// Odoo calls it two types, payment and order, but only the payment is a
/// row: the order side of the ledger is the orders themselves.
pub fn cashmove() -> Model {
    Model::new(
        meta("lunch.cashmove", "lunch_cashmove"),
        vec![
            m2o("user_id", "res.users").default_from(rusdoo_orm::defaults::CURRENT_USER),
            Field::new("date", FieldType::Date)
                .required()
                .default_from(rusdoo_orm::defaults::TODAY),
            Field::new("amount", FieldType::Float { digits: Some((16, 2)) })
                .required()
                .default_value(json!(0.0)),
            Field::new("description", FieldType::Text),
            // Odoo has no `name` here and builds a display name in Python
            // ("Lunch Cashmove #7"). A many2one is read as `[id, name]`
            // by this ORM, and a ledger row with no name reads as `[7,
            // ""]` — so the description doubles as the name, which is
            // what the screen shows anyway.
            char("name"),
        ],
    )
    // the statement newest-first, like Odoo's `_order`
    .ordered("date desc, id desc")
}

/// Port of `get_wallet_balance`: the money paid in, less what the orders
/// took out, plus what the company lets people go under by.
///
/// Odoo rounds the ledger before adding the threshold, and the rounding
/// is the reason this is a function of its own: three floats added in
/// another order give a balance that is off by a cent, and a cent is the
/// difference between an order that goes through and one that is
/// refused.
pub fn balance(paid_in: f64, spent: f64, threshold: f64) -> f64 {
    round2(paid_in - spent) + threshold
}

/// Two decimal places, the precision a wallet is kept in.
pub fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `get_wallet_balance(user_id=...)` — what is left in somebody's wallet.
///
/// The default is the caller's own wallet. Naming another user is
/// allowed because the lunch manager's screens do exactly that, and the
/// method is registered as a read: who may look at whose ledger is a
/// question for `ir.rule`, not for this function.
pub fn get_wallet_balance<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let user = kwargs
            .get("user_id")
            .or_else(|| ctx.rest.first())
            .and_then(Value::as_i64)
            .unwrap_or(ctx.uid);
        Ok(json!(wallet_balance(&ctx, user).await?))
    })
}

/// The balance of `user`'s wallet, for the methods that have to know
/// before they write.
pub async fn wallet_balance(ctx: &MethodCtx<'_>, user: i64) -> Result<f64, RusdooError> {
    let paid_in = sum_of(
        ctx,
        "lunch.cashmove",
        json!([["user_id", "=", user]]),
        "amount",
    )
    .await?;
    let spent = sum_of(
        ctx,
        "lunch.order",
        json!([["user_id", "=", user], ["state", "in", SPENDING_STATES]]),
        "price",
    )
    .await?;
    Ok(balance(paid_in, spent, threshold_for(ctx, user).await?))
}

/// How far under zero this user's company lets a wallet go
/// (`res_company.lunch_minimum_threshold`).
///
/// A user with no company gets no allowance rather than an error: a
/// database that never filled the field is the common case, and
/// refusing every order there would be absurd.
async fn threshold_for(ctx: &MethodCtx<'_>, user: i64) -> Result<f64, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, "res.users", &[user], &["company_id"])
        .await?;
    let Some(company) = rows
        .first()
        .and_then(|row| row.get("company_id"))
        .and_then(crate::first_id)
    else {
        return Ok(0.0);
    };
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "res.company",
            &[company],
            &["lunch_minimum_threshold"],
        )
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("lunch_minimum_threshold"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0))
}

/// The sum of one numeric field over whatever a domain matches.
///
/// Two queries and an addition in Rust, where Odoo's view lets
/// PostgreSQL add. A wallet holds a person's own rows — tens, not
/// millions — and the alternative is a `read_group` whose result still
/// has to be unpacked here.
async fn sum_of(
    ctx: &MethodCtx<'_>,
    model: &str,
    domain: Value,
    field: &str,
) -> Result<f64, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            model,
            &parse_domain(&domain)?,
            &SearchOptions::default(),
        )
        .await?;
    if ids.is_empty() {
        return Ok(0.0);
    }
    let rows = ctx.registry.read(ctx.pool, model, &ids, &[field]).await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get(field).and_then(Value::as_f64))
        .sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wallet_is_what_went_in_less_what_was_eaten() {
        assert_eq!(balance(100.0, 12.5, 0.0), 87.5);
        // the company's threshold is an allowance, not money: it is added
        // after the ledger is rounded, exactly as Odoo does it
        assert_eq!(balance(0.0, 10.0, 25.0), 15.0);
    }

    #[test]
    fn the_ledger_is_rounded_before_the_allowance_is_added() {
        // three orders of 3.33 leave 0.01 in a wallet of 10, and a
        // balance of 0.009999999999999787 is a rejected order
        assert_eq!(balance(10.0, 3.33 * 3.0, 0.0), 0.01);
    }

    #[test]
    fn an_empty_wallet_with_no_allowance_is_exactly_zero() {
        assert_eq!(balance(0.0, 0.0, 0.0), 0.0);
    }
}

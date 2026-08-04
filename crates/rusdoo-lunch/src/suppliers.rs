//! The two buttons on a vendor's card: telling them what to cook, and
//! writing down that it arrived.
//!
//! Port of `lunch_supplier.py::action_send_orders` and
//! `action_confirm_orders`.
//!
//! What is not here is the email. Odoo's `_send_auto_email` renders
//! `lunch.lunch_order_mail_supplier` and hands it to the mail queue;
//! this port has `mail.message` (a chatter entry) and no outgoing mail
//! server, so a "sent" order is one the office marked as passed on. The
//! state machine is the same, and the day an SMTP layer lands this is
//! where the template goes.

use crate::orders;
use crate::schedule;
use rusdoo_core::RusdooError;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};

/// `action_send_orders` — pass today's orders to the vendors.
///
/// Odoo only sends for the vendors that are available today; so does
/// this, and for the same reason — an order that goes out on a day the
/// kitchen is closed is an order nobody receives.
pub fn action_send_orders<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { move_todays_orders(&ctx, "ordered", "sent", "The orders have been sent!").await })
}

/// `action_confirm_orders` — the food arrived.
pub fn action_confirm_orders<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        move_todays_orders(&ctx, "sent", "confirmed", "The orders have been confirmed!").await
    })
}

/// Move every order these vendors have today from one state to the next,
/// and answer the notification Odoo's buttons answer with.
async fn move_todays_orders(
    ctx: &MethodCtx<'_>,
    from: &str,
    to: &str,
    message: &str,
) -> Result<Value, RusdooError> {
    let open = available_today(ctx).await?;
    let ids = orders::current_orders(ctx, &open, from).await?;
    if !ids.is_empty() {
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "lunch.order",
                &ids,
                vec![("state", json!(to))],
            )
            .await?;
    }
    Ok(json!({
        "type": "ir.actions.client",
        "tag": "display_notification",
        "params": {
            "type": "success",
            "message": message,
            "next": {"type": "ir.actions.act_window_close"},
        }
    }))
}

/// The vendors among `ctx.ids` that deliver today.
async fn available_today(ctx: &MethodCtx<'_>) -> Result<Vec<i64>, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "choose at least one vendor".into(),
        ));
    }
    let today = chrono::Utc::now().date_naive();
    let mut open = Vec::new();
    for id in &ctx.ids {
        let vendor = crate::catalog::read_supplier(ctx, *id).await?;
        if schedule::available_on_date(&vendor, today) {
            open.push(*id);
        }
    }
    Ok(open)
}

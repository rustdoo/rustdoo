//! The dynamic defaults the framework itself provides, port of what
//! Odoo hands out as `fields.Datetime.now`, `fields.Date.context_today`
//! and `lambda self: self.env.company`.
//!
//! A module writing `Field::new("date_order", Datetime).default_from(now)`
//! reads like the Odoo line it came from, and nobody has to write the
//! same three lines of clock code in every crate.

use crate::fields::{DefaultCtx, DefaultFn};
use rusdoo_core::RusdooError;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

type Answer<'a> = Pin<Box<dyn Future<Output = Result<Value, RusdooError>> + Send + 'a>>;

/// Odoo's wire format for a datetime, which is what the ORM stores and
/// the client parses (`odoo/fields.py::Datetime.to_string`).
pub const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
pub const DATE_FORMAT: &str = "%Y-%m-%d";

/// `fields.Datetime.now` — the moment the record is created.
pub fn now(_ctx: DefaultCtx<'_>) -> Answer<'_> {
    Box::pin(async move {
        Ok(Value::from(
            chrono::Utc::now().format(DATETIME_FORMAT).to_string(),
        ))
    })
}

/// `fields.Date.context_today` — today's date.
///
/// Odoo reads the user's timezone to decide which day "today" is; the
/// port has no timezone on `res.users` yet, so this is UTC's today. The
/// difference is real for a user at UTC-3 creating a document at 21:30,
/// and it is the timezone field that has to land, not a guess here.
pub fn today(_ctx: DefaultCtx<'_>) -> Answer<'_> {
    Box::pin(async move {
        Ok(Value::from(
            chrono::Utc::now().format(DATE_FORMAT).to_string(),
        ))
    })
}

/// `lambda self: self.env.company` — the company of whoever is creating
/// the record.
///
/// A user with no company, or a uid that names nobody, gives no default
/// rather than an error: a single-company database that never filled the
/// field is the common case, and refusing every create there would be
/// absurd.
///
/// A failing *query*, though, is raised. This runs on the create's own
/// connection, so swallowing an error would leave the transaction
/// aborted and every statement after it failing with `current
/// transaction is aborted` — the real cause named nowhere.
pub fn user_company(ctx: DefaultCtx<'_>) -> Answer<'_> {
    Box::pin(async move {
        let found: Option<Option<i32>> =
            sqlx::query_scalar(r#"SELECT "company_id" FROM "res_users" WHERE "id" = $1"#)
                .bind(ctx.uid as i32)
                .fetch_optional(&mut *ctx.conn)
                .await
                .map_err(|error| {
                    RusdooError::Database(format!(
                        "default de empresa em {}: {error}",
                        ctx.model
                    ))
                })?;
        Ok(match found.flatten() {
            Some(company) => Value::from(i64::from(company)),
            None => Value::Null,
        })
    })
}

/// The acting user, for the `user_id` every document carries
/// (`default=lambda self: self.env.user`).
pub fn current_user(ctx: DefaultCtx<'_>) -> Answer<'_> {
    let uid = ctx.uid;
    Box::pin(async move { Ok(Value::from(uid)) })
}

/// Named so a module reads like the Odoo it came from.
pub const NOW: DefaultFn = now;
pub const TODAY: DefaultFn = today;
pub const USER_COMPANY: DefaultFn = user_company;
pub const CURRENT_USER: DefaultFn = current_user;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_formats_are_the_ones_odoo_uses() {
        let stamp = chrono::NaiveDate::from_ymd_opt(2026, 8, 3)
            .unwrap()
            .and_hms_opt(14, 5, 9)
            .unwrap();
        assert_eq!(stamp.format(DATETIME_FORMAT).to_string(), "2026-08-03 14:05:09");
        assert_eq!(stamp.format(DATE_FORMAT).to_string(), "2026-08-03");
    }
}

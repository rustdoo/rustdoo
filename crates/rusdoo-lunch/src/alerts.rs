//! The notice at the top of the ordering screen.
//!
//! Port of `odoo/addons/lunch/models/lunch_alert.py`, minus the cron.
//!
//! Odoo's alert owns an `ir.cron` row that it rewrites on every save so
//! that a `chat` alert is pushed at the minute it was set for. The
//! port's `ir.cron` runs a *named model method*, not a snippet of Python
//! kept in a column, so there is nothing for a per-record job to point
//! at. What survives is the part a user reads: the notice, the days it
//! shows on, and the day it stops.

use crate::schedule;
use crate::{char, meta};
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::Model;
use serde_json::{json, Map, Value};

/// `lunch.alert` — something the office wants everybody to know before
/// they order.
pub fn alert() -> Model {
    let mut fields = vec![
        char("name").required().translatable(),
        Field::new("message", FieldType::Html)
            .required()
            .translatable(),
        Field::new(
            "mode",
            FieldType::Selection(vec![
                ("alert".into(), "Alert in app".into()),
                ("chat".into(), "Chat notification".into()),
            ]),
        )
        .default_value(json!("alert")),
        Field::new(
            "recipients",
            FieldType::Selection(vec![
                ("everyone".into(), "Everyone".into()),
                ("last_week".into(), "Employee who ordered last week".into()),
                ("last_month".into(), "Employee who ordered last month".into()),
                ("last_year".into(), "Employee who ordered last year".into()),
            ]),
        )
        .default_value(json!("everyone")),
        Field::new("notification_time", FieldType::Float { digits: None })
            .required()
            .default_value(json!(10.0)),
        Field::new(
            "notification_moment",
            FieldType::Selection(vec![("am".into(), "AM".into()), ("pm".into(), "PM".into())]),
        )
        .required()
        .default_value(json!("am")),
        char("tz").required().default_value(json!("UTC")),
        Field::new("until", FieldType::Date),
        Field::new("active", FieldType::Boolean).default_value(json!(true)),
        Field::new(
            "location_ids",
            FieldType::Many2many {
                comodel: "lunch.location".into(),
                relation: "lunch_alert_location_rel".into(),
                column1: "alert_id".into(),
                column2: "location_id".into(),
            },
        ),
        Field::new("available_today", FieldType::Boolean)
            .computed(&["mon", "tue", "wed", "thu", "fri", "sat", "sun", "until"], available_today),
    ];
    // every day by default, unlike a vendor: an alert is about the
    // office, and the office is there on Saturday too if somebody is
    for day in schedule::WEEKDAY_TO_NAME {
        fields.push(Field::new(day, FieldType::Boolean).default_value(json!(true)));
    }
    Model::new(meta("lunch.alert", "lunch_alert"), fields)
        .sql_constrained(
            "lunch_alert_notification_time_range",
            "CHECK(notification_time >= 0 AND notification_time <= 12)",
            "the notification time must be between 0 and 12",
        )
        // Odoo's `_order`: whatever was touched last, first
        .ordered("write_date desc, id")
}

fn available_today(record: &Map<String, Value>) -> Value {
    json!(schedule::alert_available_today(
        record,
        chrono::Utc::now().date_naive()
    ))
}

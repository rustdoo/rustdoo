//! When a vendor takes orders, and until what time.
//!
//! Port of the module-level helpers in
//! `odoo/addons/lunch/models/lunch_supplier.py` — `WEEKDAY_TO_NAME`,
//! `float_to_time`, `time_to_float`, `_available_on_date` — plus the two
//! predicates built on them. They are here, apart from the models,
//! because they are the only part of this addon that is pure arithmetic
//! over a calendar, and the only part worth testing without a database.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde_json::{Map, Value};

/// Odoo's `WEEKDAY_TO_NAME`: the boolean field that says whether the
/// vendor delivers on that weekday. The order is Monday-first because
/// that is what `date.weekday()` answers.
pub const WEEKDAY_TO_NAME: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// The wire format a date travels in, and the one a `Date` column reads
/// back as (`odoo/fields.py::Date.to_string`).
pub const DATE_FORMAT: &str = "%Y-%m-%d";

/// The weekday field a date falls on.
pub fn weekday_field(date: NaiveDate) -> &'static str {
    WEEKDAY_TO_NAME[date.weekday().num_days_from_monday() as usize]
}

/// A date as a record read hands it over: `"2026-08-03"`, or nothing.
pub fn as_date(value: Option<&Value>) -> Option<NaiveDate> {
    let text = value.and_then(Value::as_str)?;
    NaiveDate::parse_from_str(text, DATE_FORMAT).ok()
}

/// Port of `float_to_time`: the hour a vendor's cut-off is written as on
/// screen (`11.5`) turned into a time of day (11:30).
///
/// `moment` is `"am"` or `"pm"`, and 12.0 PM is the end of the day —
/// Odoo returns `time.max` there so that "noon, PM" means "never today",
/// which is the only reading that keeps the field's 0..12 range usable.
///
/// One deviation: Odoo rounds the minutes on their own, so 11.999 gives
/// minute 60 and `time()` raises. Here the whole thing is rounded to
/// minutes first, which carries into the hour instead of failing.
pub fn float_to_time(hours: f64, moment: &str) -> NaiveTime {
    if hours == 12.0 && moment == "pm" {
        return NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).expect("a valid end of day");
    }
    let offset = if moment == "pm" { 12.0 } else { 0.0 };
    let minutes = ((hours + offset) * 60.0).round().max(0.0) as u32;
    let (hour, minute) = (minutes / 60, minutes % 60);
    NaiveTime::from_hms_opt(hour.min(23), minute, 0).expect("hour clamped to the day")
}

/// Port of `time_to_float`: the inverse, to two digits like Odoo's
/// `float_round(..., precision_digits=2)`.
pub fn time_to_float(time: NaiveTime) -> f64 {
    let hours = f64::from(time.hour()) + f64::from(time.minute()) / 60.0
        + f64::from(time.second()) / 3600.0;
    (hours * 100.0).round() / 100.0
}

/// Port of `_available_on_date`: the vendor delivers on that weekday, and
/// the recurrence has not run out yet.
///
/// `record` is a vendor as a read hands it over — the seven weekday
/// booleans and `recurrency_end_date`. Odoo compares with `>=`, so the
/// end date is the first day the vendor is *not* available.
pub fn available_on_date(record: &Map<String, Value>, date: NaiveDate) -> bool {
    let open = record
        .get(weekday_field(date))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let over = as_date(record.get("recurrency_end_date")).is_some_and(|end| date >= end);
    open && !over
}

/// Port of `_compute_order_deadline_passed`: whether it is too late to
/// order from this vendor today.
///
/// A vendor ordered from by email has a cut-off — the hour the automatic
/// mail goes out. One ordered from by phone has none, so the only thing
/// that can be late is the day itself.
pub fn order_deadline_passed(record: &Map<String, Value>, now: NaiveDateTime) -> bool {
    let available = available_on_date(record, now.date());
    if record.get("send_by").and_then(Value::as_str) != Some("mail") {
        return !available;
    }
    let hours = record
        .get("automatic_email_time")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let moment = record
        .get("moment")
        .and_then(Value::as_str)
        .unwrap_or("am");
    available && now.time() > float_to_time(hours, moment)
}

/// Port of `lunch_alert.py::_compute_available_today`: an alert shows on
/// the weekdays it was ticked for, until the day it was given.
///
/// Odoo compares `until` with `>` here and the vendor's end date with
/// `>=`. That is not a slip to tidy up: an alert is shown *through* its
/// last day, a recurrence stops *on* its end date.
pub fn alert_available_today(record: &Map<String, Value>, today: NaiveDate) -> bool {
    let shown = record
        .get(weekday_field(today))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let still = as_date(record.get("until")).is_none_or(|until| until > today);
    shown && still
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, DATE_FORMAT).expect("a date")
    }

    /// A vendor open on weekdays only, with no end to the arrangement.
    fn weekdays_only() -> Map<String, Value> {
        let mut record = Map::new();
        for name in ["mon", "tue", "wed", "thu", "fri"] {
            record.insert(name.into(), json!(true));
        }
        for name in ["sat", "sun"] {
            record.insert(name.into(), json!(false));
        }
        record
    }

    #[test]
    fn the_weekday_field_follows_the_calendar() {
        // 2026-08-03 is a Monday
        assert_eq!(weekday_field(day("2026-08-03")), "mon");
        assert_eq!(weekday_field(day("2026-08-08")), "sat");
        assert_eq!(weekday_field(day("2026-08-09")), "sun");
    }

    #[test]
    fn an_hour_written_as_a_number_becomes_a_time_of_day() {
        assert_eq!(float_to_time(11.5, "am"), NaiveTime::from_hms_opt(11, 30, 0).unwrap());
        assert_eq!(float_to_time(10.0, "pm"), NaiveTime::from_hms_opt(22, 0, 0).unwrap());
        // noon PM is the end of the day: "there is no cut-off today"
        assert_eq!(
            float_to_time(12.0, "pm"),
            NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).unwrap()
        );
        // and the round trip lands where it started
        assert_eq!(time_to_float(float_to_time(9.25, "am")), 9.25);
    }

    #[test]
    fn a_vendor_is_available_on_the_days_it_was_ticked_for() {
        let vendor = weekdays_only();
        assert!(available_on_date(&vendor, day("2026-08-03")), "Monday");
        assert!(!available_on_date(&vendor, day("2026-08-08")), "Saturday");
    }

    #[test]
    fn the_end_date_is_the_first_day_the_vendor_is_gone() {
        let mut vendor = weekdays_only();
        vendor.insert("recurrency_end_date".into(), json!("2026-08-05"));
        assert!(available_on_date(&vendor, day("2026-08-04")));
        // on the end date itself the arrangement is over, like Odoo's `>=`
        assert!(!available_on_date(&vendor, day("2026-08-05")));
    }

    #[test]
    fn a_vendor_ordered_from_by_phone_has_no_cut_off_but_still_has_days() {
        let mut vendor = weekdays_only();
        vendor.insert("send_by".into(), json!("phone"));
        let monday_late = day("2026-08-03").and_hms_opt(23, 0, 0).unwrap();
        assert!(!order_deadline_passed(&vendor, monday_late), "still Monday");
        let saturday = day("2026-08-08").and_hms_opt(9, 0, 0).unwrap();
        assert!(order_deadline_passed(&vendor, saturday), "closed on Saturday");
    }

    #[test]
    fn a_vendor_ordered_from_by_email_closes_at_the_hour_the_mail_goes_out() {
        let mut vendor = weekdays_only();
        vendor.insert("send_by".into(), json!("mail"));
        vendor.insert("automatic_email_time".into(), json!(11.0));
        vendor.insert("moment".into(), json!("am"));
        let before = day("2026-08-03").and_hms_opt(10, 59, 0).unwrap();
        let after = day("2026-08-03").and_hms_opt(11, 1, 0).unwrap();
        assert!(!order_deadline_passed(&vendor, before));
        assert!(order_deadline_passed(&vendor, after));
        // a day the vendor does not deliver has nothing to be late for:
        // the order is refused by availability, not by the clock
        let saturday = day("2026-08-08").and_hms_opt(23, 0, 0).unwrap();
        assert!(!order_deadline_passed(&vendor, saturday));
    }

    #[test]
    fn an_alert_is_shown_through_its_last_day() {
        let mut alert = weekdays_only();
        alert.insert("until".into(), json!("2026-08-05"));
        assert!(alert_available_today(&alert, day("2026-08-04")));
        // Odoo's `>`: the alert stops the day it was given, not before
        assert!(!alert_available_today(&alert, day("2026-08-05")));
        // and an alert with no end date runs on
        alert.remove("until");
        assert!(alert_available_today(&alert, day("2026-08-05")));
    }
}

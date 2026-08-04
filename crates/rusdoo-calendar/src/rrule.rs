//! The recurrence rule, port of `odoo/addons/calendar/models/calendar_recurrence.py`.
//!
//! Odoo hands the hard part to `python-dateutil`: a `calendar.recurrence`
//! holds the rule in pieces (every N weeks, on Monday and Wednesday,
//! until December) and `dateutil.rrule` turns those pieces into the list
//! of moments the meeting happens at. There is no dateutil here, so the
//! expansion is written out: the four frequencies Odoo offers, the two
//! ways a monthly rule can name its day, and the three ways a series can
//! end.
//!
//! It is all pure functions over dates on purpose. Recurrence is where a
//! calendar is wrong in ways nobody notices for a month — the fifth
//! Tuesday, the 31st of February, the occurrence that lands *before* the
//! event it came from — and those are answers a test can pin down without
//! a database.
//!
//! ## Where this deviates from Odoo, and why
//!
//! * **Occurrences are computed in UTC.** Odoo localizes each occurrence
//!   into the recurrence's timezone before storing it, so that a 6am
//!   meeting stays at 6am across a daylight-saving change. That needs a
//!   timezone database; the port has none yet, and guessing is worse than
//!   saying so. A recurrence that spans a DST boundary will drift by an
//!   hour. See the report: this is the framework's gap, not the addon's.
//! * **The `rrule` string is the RRULE value alone** (`FREQ=WEEKLY;...`),
//!   not dateutil's `DTSTART:...\nRRULE:...`. The field is computed from
//!   the rule's own pieces, and the start date belongs to the event, not
//!   to the rule.
//! * **The week starts on Monday.** Odoo reads `res.lang.week_start`;
//!   `res.lang` here has no such field yet.

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime};
use serde_json::{Map, Value};

/// `MAX_RECURRENT_EVENT` — the hard cap on how many events one rule may
/// ever produce. A rule that repeats forever still has to stop somewhere,
/// and it is better that it stops at a number somebody chose.
pub const MAX_RECURRENT_EVENT: usize = 720;

/// `calendar.max_recurrence_years` — how far ahead a series with no end
/// is materialized. Odoo's default, and Odoo's parameter name.
pub const DEFAULT_MAX_RECURRENCE_YEARS: i64 = 15;

/// A period that yields nothing (the 31st in a 30-day month) still costs
/// a turn of the loop. The cap keeps a rule that can never match — which
/// the checks below should already have refused — from spinning forever.
const MAX_PERIODS: usize = 20_000;

/// The seven booleans a weekly rule is spelled with, in the order the
/// screen draws them (`mon` .. `sun`).
pub const WEEKDAY_FIELDS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// The same seven, as `calendar.recurrence.weekday` spells them.
pub const WEEKDAY_CODES: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

/// The same seven, as iCalendar spells them (RFC 5545 `BYDAY`).
pub const WEEKDAY_ICAL: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

/// The same seven, as a person reads them.
pub const WEEKDAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// `BYDAY_SELECTION`: which occurrence of the weekday in the month.
pub const BYDAY_LABELS: [(&str, &str); 5] = [
    ("1", "First"),
    ("2", "Second"),
    ("3", "Third"),
    ("4", "Fourth"),
    ("-1", "Last"),
];

/// `RRULE_TYPE_SELECTION` — how often the meeting comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    pub fn parse(value: &str) -> Option<Freq> {
        Some(match value {
            "daily" => Freq::Daily,
            "weekly" => Freq::Weekly,
            "monthly" => Freq::Monthly,
            "yearly" => Freq::Yearly,
            _ => return None,
        })
    }

    /// The value stored in `rrule_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            Freq::Daily => "daily",
            Freq::Weekly => "weekly",
            Freq::Monthly => "monthly",
            Freq::Yearly => "yearly",
        }
    }

    /// The `FREQ=` of an iCalendar rule.
    pub fn ical(self) -> &'static str {
        match self {
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        }
    }

    fn ical_parse(value: &str) -> Option<Freq> {
        Some(match value {
            "DAILY" => Freq::Daily,
            "WEEKLY" => Freq::Weekly,
            "MONTHLY" => Freq::Monthly,
            "YEARLY" => Freq::Yearly,
            _ => return None,
        })
    }
}

/// `END_TYPE_SELECTION` — what stops the series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndType {
    /// after a given number of occurrences
    Count,
    /// on a given date
    EndDate,
    /// never — bounded only by `calendar.max_recurrence_years`
    Forever,
}

impl EndType {
    pub fn parse(value: &str) -> Option<EndType> {
        Some(match value {
            "count" => EndType::Count,
            "end_date" => EndType::EndDate,
            "forever" => EndType::Forever,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EndType::Count => "count",
            EndType::EndDate => "end_date",
            EndType::Forever => "forever",
        }
    }
}

/// `MONTH_BY_SELECTION` — how a monthly rule names its day: "the 15th",
/// or "the second Monday".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonthBy {
    Date,
    Day,
}

impl MonthBy {
    pub fn parse(value: &str) -> Option<MonthBy> {
        Some(match value {
            "date" => MonthBy::Date,
            "day" => MonthBy::Day,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MonthBy::Date => "date",
            MonthBy::Day => "day",
        }
    }
}

/// One recurrence rule, as `calendar.recurrence` stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub freq: Freq,
    pub interval: i64,
    pub end_type: EndType,
    pub count: i64,
    pub until: Option<NaiveDate>,
    /// `mon` .. `sun`, in that order
    pub weekdays: [bool; 7],
    pub month_by: MonthBy,
    /// day of the month, for `month_by = date`
    pub day: i64,
    /// which weekday, for `month_by = day`
    pub weekday: Option<usize>,
    /// which occurrence of that weekday: 1..4, or -1 for the last
    pub byday: i64,
}

impl Default for Rule {
    /// The defaults `calendar.recurrence` declares: every week, once.
    fn default() -> Rule {
        Rule {
            freq: Freq::Weekly,
            interval: 1,
            end_type: EndType::Count,
            count: 1,
            until: None,
            weekdays: [false; 7],
            month_by: MonthBy::Date,
            day: 1,
            weekday: None,
            byday: 1,
        }
    }
}

fn as_i64(record: &Map<String, Value>, name: &str, fallback: i64) -> i64 {
    record.get(name).and_then(Value::as_i64).unwrap_or(fallback)
}

fn as_text<'a>(record: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    record
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

impl Rule {
    /// The rule a `calendar.recurrence` row spells out.
    ///
    /// A missing piece falls back to the field's declared default rather
    /// than failing: a compute runs on rows that are half written, and a
    /// rule that cannot be read is reported by the model's constraint,
    /// with a sentence, and not by a blank screen.
    pub fn from_record(record: &Map<String, Value>) -> Result<Rule, String> {
        let freq = match as_text(record, "rrule_type") {
            Some(value) => Freq::parse(value)
                .ok_or_else(|| format!("{value:?} is not a recurrence frequency"))?,
            None => Rule::default().freq,
        };
        let end_type = match as_text(record, "end_type") {
            Some(value) => {
                EndType::parse(value).ok_or_else(|| format!("{value:?} is not an end type"))?
            }
            None => Rule::default().end_type,
        };
        let month_by = match as_text(record, "month_by") {
            Some(value) => MonthBy::parse(value)
                .ok_or_else(|| format!("{value:?} is not a way to name a day of the month"))?,
            None => Rule::default().month_by,
        };
        let weekday = match as_text(record, "weekday") {
            Some(value) => Some(
                WEEKDAY_CODES
                    .iter()
                    .position(|code| *code == value)
                    .ok_or_else(|| format!("{value:?} is not a weekday"))?,
            ),
            None => None,
        };
        let byday = match as_text(record, "byday") {
            Some(value) => value
                .parse::<i64>()
                .map_err(|_| format!("{value:?} is not a position in the month"))?,
            None => Rule::default().byday,
        };
        let until = match as_text(record, "until") {
            Some(value) => Some(parse_date(value)?),
            None => None,
        };
        let mut weekdays = [false; 7];
        for (slot, name) in weekdays.iter_mut().zip(WEEKDAY_FIELDS) {
            *slot = record.get(name).and_then(Value::as_bool).unwrap_or(false);
        }
        Ok(Rule {
            freq,
            interval: as_i64(record, "interval", 1),
            end_type,
            count: as_i64(record, "count", 1),
            until,
            weekdays,
            month_by,
            day: as_i64(record, "day", 1),
            weekday,
            byday,
        })
    }

    /// The rule as field values, for writing it back onto a record.
    pub fn to_values(&self) -> Vec<(&'static str, Value)> {
        let mut values: Vec<(&'static str, Value)> = vec![
            ("rrule_type", Value::from(self.freq.as_str())),
            ("interval", Value::from(self.interval)),
            ("end_type", Value::from(self.end_type.as_str())),
            ("count", Value::from(self.count)),
            ("month_by", Value::from(self.month_by.as_str())),
            ("day", Value::from(self.day)),
            ("byday", Value::from(self.byday.to_string())),
            (
                "until",
                match self.until {
                    Some(date) => Value::from(date.to_string()),
                    None => Value::Null,
                },
            ),
            (
                "weekday",
                match self.weekday {
                    Some(index) => Value::from(WEEKDAY_CODES[index]),
                    None => Value::Null,
                },
            ),
        ];
        for (name, on) in WEEKDAY_FIELDS.iter().zip(self.weekdays) {
            values.push((name, Value::from(on)));
        }
        values
    }

    /// What makes this rule impossible to expand, port of the errors
    /// `_rrule_serialize` and `_get_rrule` raise.
    ///
    /// It is a separate step from the expansion so the model can refuse
    /// the *write* — a rule saved unusable is a series that silently
    /// produces nothing, and the person who saved it finds out weeks
    /// later.
    pub fn check(&self) -> Result<(), String> {
        if self.interval <= 0 {
            return Err("the interval cannot be negative: a rule repeats every 1 period or more"
                .to_string());
        }
        if self.end_type == EndType::Count && self.count <= 0 {
            return Err("the number of repetitions cannot be negative".to_string());
        }
        if self.end_type == EndType::EndDate && self.until.is_none() {
            return Err("a series that ends on a date needs the date".to_string());
        }
        if self.freq == Freq::Weekly && !self.weekdays.iter().any(|on| *on) {
            return Err("you have to choose at least one day in the week".to_string());
        }
        if self.freq == Freq::Monthly {
            match self.month_by {
                MonthBy::Date if !(1..=31).contains(&self.day) => {
                    return Err("the day of the month must be between 1 and 31".to_string());
                }
                MonthBy::Day if self.weekday.is_none() => {
                    return Err("say which weekday of the month the meeting falls on".to_string());
                }
                MonthBy::Day if !BYDAY_LABELS.iter().any(|(v, _)| *v == self.byday.to_string()) => {
                    return Err(
                        "say which occurrence of the weekday: the first, second, third, \
                         fourth or last"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The weekdays this rule fires on, as a person reads them.
    fn weekday_names(&self) -> Vec<&'static str> {
        WEEKDAY_NAMES
            .iter()
            .zip(self.weekdays)
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect()
    }

    /// The rule in one line, port of `get_recurrence_name`.
    ///
    /// This is the `name` of a `calendar.recurrence`, and it is the only
    /// place a user ever sees the rule as a whole: the form shows the
    /// pieces, the list shows this.
    pub fn name(&self) -> String {
        let interval = self.interval;
        let what = match self.freq {
            Freq::Daily => format!("Every {interval} Days"),
            Freq::Weekly => {
                let days = self.weekday_names().join(", ");
                format!("Every {interval} Weeks on {days}")
            }
            Freq::Monthly => match self.month_by {
                MonthBy::Day => {
                    let position = BYDAY_LABELS
                        .iter()
                        .find(|(value, _)| *value == self.byday.to_string())
                        .map_or("", |(_, label)| *label);
                    let weekday = self.weekday.map_or("", |index| WEEKDAY_NAMES[index]);
                    format!("Every {interval} Months on the {position} {weekday}")
                }
                MonthBy::Date => format!("Every {interval} Months day {}", self.day),
            },
            Freq::Yearly => format!("Every {interval} Years"),
        };
        match (self.end_type, self.until) {
            (EndType::Count, _) => format!("{what} for {} events", self.count),
            (EndType::EndDate, Some(until)) => format!("{what} until {until}"),
            _ => what,
        }
    }

    /// The rule as an iCalendar RRULE value (RFC 5545), port of
    /// `_rrule_serialize`.
    pub fn to_rrule(&self) -> Result<String, String> {
        self.check()?;
        let mut parts = vec![
            format!("FREQ={}", self.freq.ical()),
            format!("INTERVAL={}", self.interval),
        ];
        match self.freq {
            Freq::Weekly => {
                let days: Vec<&str> = WEEKDAY_ICAL
                    .iter()
                    .zip(self.weekdays)
                    .filter(|(_, on)| *on)
                    .map(|(code, _)| *code)
                    .collect();
                parts.push(format!("BYDAY={}", days.join(",")));
            }
            Freq::Monthly => match self.month_by {
                MonthBy::Date => parts.push(format!("BYMONTHDAY={}", self.day)),
                MonthBy::Day => {
                    let weekday = self.weekday.map_or("MO", |index| WEEKDAY_ICAL[index]);
                    parts.push(format!("BYDAY={}{weekday}", self.byday));
                }
            },
            Freq::Daily | Freq::Yearly => {}
        }
        match (self.end_type, self.until) {
            (EndType::Count, _) => parts.push(format!("COUNT={}", self.count)),
            // the whole of the last day belongs to the series, which is
            // what Odoo means by `datetime.combine(until, time.max)`
            (EndType::EndDate, Some(until)) => {
                parts.push(format!("UNTIL={}T235959", until.format("%Y%m%d")));
            }
            _ => {}
        }
        Ok(parts.join(";"))
    }

    /// Read a rule back out of an RRULE value, port of `_rrule_parse`.
    ///
    /// What arrives is not always what this module wrote: an event
    /// imported from another calendar carries that calendar's rule, and
    /// Odoo strips the `X-` extensions some of them add before parsing.
    pub fn from_rrule(text: &str) -> Result<Rule, String> {
        let mut rule = Rule {
            // an imported rule that names no end has none
            end_type: EndType::Forever,
            ..Rule::default()
        };
        let body = text
            .lines()
            .find_map(|line| line.strip_prefix("RRULE:").or({
                // a bare `FREQ=...` is what this module writes
                if line.contains("FREQ=") && !line.starts_with("DTSTART") {
                    Some(line)
                } else {
                    None
                }
            }))
            .ok_or_else(|| format!("{text:?} carries no recurrence rule"))?;
        let mut saw_freq = false;
        for part in body.split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_uppercase();
            let value = value.trim();
            // the `X-` extensions other calendars add mean nothing here
            if key.starts_with("X-") {
                continue;
            }
            match key.as_str() {
                "FREQ" => {
                    rule.freq = Freq::ical_parse(&value.to_ascii_uppercase())
                        .ok_or_else(|| format!("{value:?} is not a recurrence frequency"))?;
                    saw_freq = true;
                }
                "INTERVAL" => {
                    rule.interval = value
                        .parse()
                        .map_err(|_| format!("{value:?} is not an interval"))?;
                }
                "COUNT" => {
                    rule.count = value
                        .parse()
                        .map_err(|_| format!("{value:?} is not a number of repetitions"))?;
                    rule.end_type = EndType::Count;
                }
                "UNTIL" => {
                    rule.until = Some(parse_ical_date(value)?);
                    rule.end_type = EndType::EndDate;
                }
                "BYMONTHDAY" => {
                    rule.day = value
                        .parse()
                        .map_err(|_| format!("{value:?} is not a day of the month"))?;
                    rule.month_by = MonthBy::Date;
                }
                "BYDAY" => parse_byday(&mut rule, value)?,
                _ => {}
            }
        }
        if !saw_freq {
            return Err(format!("{text:?} carries no FREQ"));
        }
        rule.check()?;
        Ok(rule)
    }

    /// How many occurrences this rule may produce at most.
    ///
    /// A series with an end date is bounded by the date, so it takes the
    /// hard cap; a series with no end at all is bounded by
    /// `calendar.max_recurrence_years` turned into a number of
    /// occurrences, which is Odoo's own arithmetic in `_get_rrule`.
    fn horizon(&self, max_years: i64) -> usize {
        let bounded = |n: i64| usize::try_from(n.max(1)).unwrap_or(1).min(MAX_RECURRENT_EVENT);
        match self.end_type {
            EndType::Count => bounded(self.count),
            EndType::EndDate => MAX_RECURRENT_EVENT,
            EndType::Forever => match self.freq {
                Freq::Yearly => bounded(max_years),
                Freq::Monthly => bounded(max_years * 12),
                Freq::Weekly => {
                    let chosen = self.weekdays.iter().filter(|on| **on).count() as i64;
                    let weeks = max_years * 365 / 7;
                    bounded(weeks * chosen.max(1) / self.interval.max(1))
                }
                Freq::Daily => bounded(max_years * 365 / self.interval.max(1)),
            },
        }
    }

    /// Every moment this rule fires at, starting from `dtstart`.
    ///
    /// Port of `_get_occurrences` plus `_range_calculation`. Two things
    /// in there matter and are easy to get wrong:
    ///
    /// * a weekly or monthly rule is expanded from the start of the
    ///   *period* — the Monday of the week, the 1st of the month — so
    ///   that "every week on Monday and Wednesday" fires on the Monday of
    ///   the week its Wednesday event was created in, not on the next
    ///   one;
    /// * which means the first candidates can land before the event
    ///   itself, and those are dropped.
    ///
    /// Odoo drops them *after* generating, then generates a second time
    /// with a padded count to make up the shortfall. This produces the
    /// count the user asked for in one pass instead: "repeat 5 times"
    /// should mean five meetings, and Odoo's padding makes it five only
    /// by coincidence.
    pub fn occurrences(
        &self,
        dtstart: NaiveDateTime,
        max_years: i64,
    ) -> Result<Vec<NaiveDateTime>, String> {
        self.check()?;
        let wanted = self.horizon(max_years);
        let time = dtstart.time();
        let floor = dtstart.date();
        let stop = match self.end_type {
            EndType::EndDate => self.until,
            _ => None,
        };
        let mut out: Vec<NaiveDateTime> = Vec::new();
        for index in 0..MAX_PERIODS as i64 {
            if out.len() >= wanted {
                break;
            }
            // a period that cannot even be addressed is the end of the
            // calendar, not a period that yields nothing
            let Some(marker) = self.period_marker(floor, index) else {
                break;
            };
            if stop.is_some_and(|until| marker > until) {
                break;
            }
            for date in self.dates_in(floor, index) {
                if stop.is_some_and(|until| date > until) {
                    return Ok(out);
                }
                // the period a weekly or monthly rule starts in began
                // before the event did: those candidates are real
                // occurrences of the rule and still must not become
                // meetings in the past
                if date < floor {
                    continue;
                }
                out.push(date.and_time(time));
                if out.len() >= wanted {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The first day of the `index`-th period, port of
    /// `_get_start_of_period` advanced by the interval.
    ///
    /// It is what decides when a series is over, and it exists apart from
    /// the occurrences because a period may contribute none — there is no
    /// 31st in November — and a series bounded by a date would otherwise
    /// have nothing to compare against.
    fn period_marker(&self, anchor: NaiveDate, index: i64) -> Option<NaiveDate> {
        let step = self.interval.max(1);
        let months = |n: i64| u32::try_from(n).ok().map(Months::new);
        match self.freq {
            Freq::Daily => {
                anchor.checked_add_days(Days::new(u64::try_from(step * index).ok()?))
            }
            // the Monday of the event's week, then whole weeks from there
            Freq::Weekly => {
                let monday =
                    anchor.checked_sub_days(Days::new(u64::from(anchor.weekday().num_days_from_monday())))?;
                monday.checked_add_days(Days::new(u64::try_from(step * index * 7).ok()?))
            }
            // the 1st of the event's month, then whole months from there
            Freq::Monthly => anchor
                .with_day(1)?
                .checked_add_months(months(step * index)?),
            // the year itself: a yearly rule keeps the event's own month
            // and day, which is not a date that exists in every year
            Freq::Yearly => {
                let year = i32::try_from(i64::from(anchor.year()) + step * index).ok()?;
                NaiveDate::from_ymd_opt(year, 1, 1)
            }
        }
    }

    /// The dates the `index`-th period contributes, in order.
    ///
    /// Most periods contribute exactly one; a weekly period contributes
    /// one per chosen weekday, and a monthly or yearly one may contribute
    /// none — there is no 31st in November and no 29th of February in
    /// 2027. dateutil skips those rather than sliding to the nearest day,
    /// and so does this: a rule about the 31st that quietly fired on the
    /// 30th would be a meeting nobody scheduled.
    fn dates_in(&self, anchor: NaiveDate, index: i64) -> Vec<NaiveDate> {
        let Some(marker) = self.period_marker(anchor, index) else {
            return Vec::new();
        };
        match self.freq {
            Freq::Daily => vec![marker],
            Freq::Weekly => (0..7)
                .filter(|day| self.weekdays[*day])
                .filter_map(|day| marker.checked_add_days(Days::new(day as u64)))
                .collect(),
            Freq::Monthly => match self.month_by {
                MonthBy::Date => u32::try_from(self.day)
                    .ok()
                    .and_then(|day| NaiveDate::from_ymd_opt(marker.year(), marker.month(), day))
                    .into_iter()
                    .collect(),
                MonthBy::Day => self
                    .weekday
                    .and_then(|weekday| {
                        nth_weekday(marker.year(), marker.month(), weekday, self.byday)
                    })
                    .into_iter()
                    .collect(),
            },
            Freq::Yearly => NaiveDate::from_ymd_opt(marker.year(), anchor.month(), anchor.day())
                .into_iter()
                .collect(),
        }
    }
}

/// The `nth` occurrence of `weekday` (0 = Monday) in a month, with -1
/// meaning the last one.
fn nth_weekday(year: i32, month: u32, weekday: usize, nth: i64) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    if nth > 0 {
        let offset = i64::from(weekday as u32)
            - i64::from(first.weekday().num_days_from_monday());
        let day = offset.rem_euclid(7) + (nth - 1) * 7 + 1;
        return NaiveDate::from_ymd_opt(year, month, u32::try_from(day).ok()?);
    }
    if nth == 0 {
        return None;
    }
    let last = first.checked_add_months(Months::new(1))?.pred_opt()?;
    let back = (i64::from(last.weekday().num_days_from_monday()) - i64::from(weekday as u32))
        .rem_euclid(7)
        + (-nth - 1) * 7;
    last.checked_sub_days(Days::new(u64::try_from(back).ok()?))
}

/// `BYDAY=MO,WE` for a weekly rule, `BYDAY=2MO` or `BYDAY=-1FR` for a
/// monthly one.
fn parse_byday(rule: &mut Rule, value: &str) -> Result<(), String> {
    let mut weekly = [false; 7];
    let mut any_weekly = false;
    for token in value.split(',') {
        let token = token.trim().to_ascii_uppercase();
        let split = token
            .char_indices()
            .find(|(_, c)| c.is_ascii_alphabetic())
            .map(|(at, _)| at)
            .ok_or_else(|| format!("{token:?} is not a weekday"))?;
        let (position, code) = token.split_at(split);
        let index = WEEKDAY_ICAL
            .iter()
            .position(|known| *known == code)
            .ok_or_else(|| format!("{code:?} is not a weekday"))?;
        if position.is_empty() {
            weekly[index] = true;
            any_weekly = true;
            continue;
        }
        // a position turns the rule monthly: "the second Monday" is not
        // something a weekly rule can say
        rule.byday = position
            .trim_start_matches('+')
            .parse()
            .map_err(|_| format!("{position:?} is not a position in the month"))?;
        rule.weekday = Some(index);
        rule.month_by = MonthBy::Day;
        rule.freq = Freq::Monthly;
    }
    if any_weekly {
        rule.weekdays = weekly;
        if rule.freq != Freq::Monthly {
            rule.freq = Freq::Weekly;
        }
    }
    Ok(())
}

/// A date as it travels in JSON (`YYYY-MM-DD`), which is also how a
/// datetime starts.
pub fn parse_date(value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(&value[..value.len().min(10)], "%Y-%m-%d")
        .map_err(|_| format!("{value:?} is not a date"))
}

/// A datetime as it travels in JSON (`YYYY-MM-DD HH:MM:SS`), tolerating
/// the `T` an ISO client sends and the fractional seconds PostgreSQL adds.
pub fn parse_datetime(value: &str) -> Result<NaiveDateTime, String> {
    let text = value.trim().replace('T', " ");
    let text = text.split('.').next().unwrap_or(&text);
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| {
            NaiveDateTime::parse_from_str(&format!("{text} 00:00:00"), "%Y-%m-%d %H:%M:%S")
        })
        .map_err(|_| format!("{value:?} is not a datetime"))
}

/// The wire format a datetime goes back out in.
pub fn format_datetime(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn parse_ical_date(value: &str) -> Result<NaiveDate, String> {
    let day = value.split(['T', 't']).next().unwrap_or(value);
    NaiveDate::parse_from_str(day, "%Y%m%d").map_err(|_| format!("{value:?} is not a date"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(text: &str) -> NaiveDateTime {
        parse_datetime(text).expect("a datetime the tests wrote")
    }

    fn weekly(days: &[&str]) -> Rule {
        let mut rule = Rule {
            freq: Freq::Weekly,
            ..Rule::default()
        };
        for day in days {
            let index = WEEKDAY_FIELDS
                .iter()
                .position(|name| name == day)
                .expect("a weekday the tests named");
            rule.weekdays[index] = true;
        }
        rule
    }

    #[test]
    fn a_daily_rule_counts_days_from_the_event() {
        let rule = Rule {
            freq: Freq::Daily,
            interval: 2,
            count: 3,
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2026-03-02 09:00:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![
                at("2026-03-02 09:00:00"),
                at("2026-03-04 09:00:00"),
                at("2026-03-06 09:00:00"),
            ]
        );
    }

    #[test]
    fn a_weekly_rule_starts_at_the_monday_of_the_events_week() {
        // the event is a Wednesday; the Monday of that same week belongs
        // to the series, but it is in the past and must not be created
        let rule = Rule {
            count: 4,
            ..weekly(&["mon", "wed"])
        };
        let dates = rule.occurrences(at("2026-03-04 09:00:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![
                at("2026-03-04 09:00:00"),
                at("2026-03-09 09:00:00"),
                at("2026-03-11 09:00:00"),
                at("2026-03-16 09:00:00"),
            ],
            "no occurrence lands before the event it came from"
        );
    }

    #[test]
    fn a_weekly_rule_skips_the_weeks_its_interval_says_to_skip() {
        let rule = Rule {
            interval: 2,
            count: 3,
            ..weekly(&["fri"])
        };
        let dates = rule.occurrences(at("2026-03-06 15:30:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![
                at("2026-03-06 15:30:00"),
                at("2026-03-20 15:30:00"),
                at("2026-04-03 15:30:00"),
            ]
        );
    }

    #[test]
    fn a_monthly_rule_by_date_skips_the_months_that_have_no_such_day() {
        // there is no 31st of April or of June: dateutil drops those
        // months rather than sliding to the 30th, and so does this
        let rule = Rule {
            freq: Freq::Monthly,
            month_by: MonthBy::Date,
            day: 31,
            count: 4,
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2026-03-31 08:00:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![
                at("2026-03-31 08:00:00"),
                at("2026-05-31 08:00:00"),
                at("2026-07-31 08:00:00"),
                at("2026-08-31 08:00:00"),
            ]
        );
    }

    #[test]
    fn a_monthly_rule_by_weekday_finds_the_second_tuesday() {
        let rule = Rule {
            freq: Freq::Monthly,
            month_by: MonthBy::Day,
            weekday: Some(1),
            byday: 2,
            count: 3,
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2026-03-10 10:00:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![
                at("2026-03-10 10:00:00"),
                at("2026-04-14 10:00:00"),
                at("2026-05-12 10:00:00"),
            ]
        );
    }

    #[test]
    fn the_last_friday_is_the_last_one_and_not_the_fifth() {
        // May 2026 has five Fridays, July has four: "last" has to mean
        // the last one in each, which is what -1 is for
        let rule = Rule {
            freq: Freq::Monthly,
            month_by: MonthBy::Day,
            weekday: Some(4),
            byday: -1,
            count: 3,
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2026-05-29 17:00:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![
                at("2026-05-29 17:00:00"),
                at("2026-06-26 17:00:00"),
                at("2026-07-31 17:00:00"),
            ]
        );
    }

    #[test]
    fn a_yearly_rule_on_the_29th_of_february_only_fires_on_leap_years() {
        let rule = Rule {
            freq: Freq::Yearly,
            count: 2,
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2028-02-29 12:00:00"), 15).unwrap();
        assert_eq!(
            dates,
            vec![at("2028-02-29 12:00:00"), at("2032-02-29 12:00:00")],
            "the day does not exist in between, and is not moved to the 28th"
        );
    }

    #[test]
    fn a_series_that_ends_on_a_date_includes_the_whole_of_that_day() {
        let rule = Rule {
            freq: Freq::Daily,
            end_type: EndType::EndDate,
            until: Some(NaiveDate::from_ymd_opt(2026, 3, 5).unwrap()),
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2026-03-03 23:30:00"), 15).unwrap();
        assert_eq!(dates.len(), 3, "the 3rd, the 4th and the 5th");
        assert_eq!(*dates.last().unwrap(), at("2026-03-05 23:30:00"));
    }

    #[test]
    fn a_series_with_no_end_stops_at_the_horizon_it_was_given() {
        let rule = Rule {
            freq: Freq::Yearly,
            end_type: EndType::Forever,
            ..Rule::default()
        };
        let dates = rule.occurrences(at("2026-01-01 09:00:00"), 15).unwrap();
        assert_eq!(dates.len(), 15);

        // and never past the hard cap, whatever the horizon says
        let daily = Rule {
            freq: Freq::Daily,
            end_type: EndType::Forever,
            ..Rule::default()
        };
        let dates = daily.occurrences(at("2026-01-01 09:00:00"), 15).unwrap();
        assert_eq!(dates.len(), MAX_RECURRENT_EVENT);
    }

    #[test]
    fn a_weekly_rule_with_no_day_chosen_is_refused() {
        let rule = Rule {
            freq: Freq::Weekly,
            ..Rule::default()
        };
        let error = rule.check().expect_err("a weekly rule needs a weekday");
        assert!(error.contains("at least one day"), "{error}");
    }

    #[test]
    fn an_interval_of_zero_is_refused() {
        let rule = Rule {
            interval: 0,
            ..weekly(&["mon"])
        };
        assert!(rule.check().is_err(), "a rule that never advances");
        // and the expansion refuses it too, rather than spinning
        assert!(rule.occurrences(at("2026-03-02 09:00:00"), 15).is_err());
    }

    #[test]
    fn a_rule_survives_a_round_trip_through_its_ical_form() {
        for rule in [
            Rule {
                count: 5,
                ..weekly(&["mon", "wed"])
            },
            Rule {
                freq: Freq::Monthly,
                month_by: MonthBy::Date,
                day: 15,
                count: 3,
                ..Rule::default()
            },
            Rule {
                freq: Freq::Monthly,
                month_by: MonthBy::Day,
                weekday: Some(4),
                byday: -1,
                end_type: EndType::EndDate,
                until: Some(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()),
                ..Rule::default()
            },
            Rule {
                freq: Freq::Daily,
                interval: 3,
                end_type: EndType::Forever,
                ..Rule::default()
            },
        ] {
            let text = rule.to_rrule().expect("a rule the tests wrote");
            let back = Rule::from_rrule(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(
                back.occurrences(at("2026-03-04 09:00:00"), 15).unwrap(),
                rule.occurrences(at("2026-03-04 09:00:00"), 15).unwrap(),
                "{text} must mean the same thing after a round trip"
            );
        }
    }

    #[test]
    fn the_ical_form_is_the_one_other_calendars_write() {
        let rule = Rule {
            count: 5,
            ..weekly(&["mon", "wed"])
        };
        assert_eq!(
            rule.to_rrule().unwrap(),
            "FREQ=WEEKLY;INTERVAL=1;BYDAY=MO,WE;COUNT=5"
        );
        let monthly = Rule {
            freq: Freq::Monthly,
            month_by: MonthBy::Day,
            weekday: Some(0),
            byday: 2,
            count: 3,
            ..Rule::default()
        };
        assert_eq!(
            monthly.to_rrule().unwrap(),
            "FREQ=MONTHLY;INTERVAL=1;BYDAY=2MO;COUNT=3"
        );
    }

    #[test]
    fn an_imported_rule_keeps_its_meaning_through_the_x_extensions() {
        // Evolution and friends put their own parameters in the string;
        // Odoo strips them before parsing, and so does this
        let rule = Rule::from_rrule("RRULE:FREQ=WEEKLY;X-EVOLUTION-ENDDATE=20200120;COUNT=3;BYDAY=MO")
            .expect("the X- parameter is not part of the rule");
        assert_eq!(rule.freq, Freq::Weekly);
        assert_eq!(rule.count, 3);
        assert_eq!(rule.end_type, EndType::Count);
        assert_eq!(rule.weekdays, [true, false, false, false, false, false, false]);
    }

    #[test]
    fn a_rule_names_itself_the_way_the_list_shows_it() {
        let rule = Rule {
            count: 5,
            ..weekly(&["mon", "wed"])
        };
        assert_eq!(
            rule.name(),
            "Every 1 Weeks on Monday, Wednesday for 5 events"
        );
        let monthly = Rule {
            freq: Freq::Monthly,
            month_by: MonthBy::Day,
            weekday: Some(1),
            byday: -1,
            end_type: EndType::Forever,
            ..Rule::default()
        };
        assert_eq!(monthly.name(), "Every 1 Months on the Last Tuesday");
        let until = Rule {
            freq: Freq::Daily,
            end_type: EndType::EndDate,
            until: Some(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()),
            ..Rule::default()
        };
        assert_eq!(until.name(), "Every 1 Days until 2026-12-31");
    }

    #[test]
    fn a_rule_is_read_out_of_the_row_that_stores_it() {
        let mut record = Map::new();
        record.insert("rrule_type".into(), json!("weekly"));
        record.insert("interval".into(), json!(2));
        record.insert("end_type".into(), json!("count"));
        record.insert("count".into(), json!(4));
        record.insert("mon".into(), json!(true));
        record.insert("fri".into(), json!(true));
        let rule = Rule::from_record(&record).expect("the row spells a rule");
        assert_eq!(rule.freq, Freq::Weekly);
        assert_eq!(rule.interval, 2);
        assert_eq!(rule.weekdays, [true, false, false, false, true, false, false]);

        // a row that has nothing in it yet falls back to the declared
        // defaults instead of failing: a compute runs on half-written rows
        assert_eq!(Rule::from_record(&Map::new()).unwrap(), Rule::default());

        // but a value that is not one of the choices is a mistake, not a
        // default
        record.insert("rrule_type".into(), json!("fortnightly"));
        assert!(Rule::from_record(&record).is_err());
    }

    #[test]
    fn the_values_a_rule_writes_back_read_as_the_same_rule() {
        let rule = Rule {
            freq: Freq::Monthly,
            month_by: MonthBy::Day,
            weekday: Some(3),
            byday: -1,
            end_type: EndType::EndDate,
            until: Some(NaiveDate::from_ymd_opt(2027, 1, 31).unwrap()),
            ..Rule::default()
        };
        let record: Map<String, Value> = rule
            .to_values()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
        assert_eq!(Rule::from_record(&record).unwrap(), rule);
    }

    #[test]
    fn datetimes_survive_the_shapes_the_wire_and_the_database_use() {
        assert_eq!(
            parse_datetime("2026-03-04 09:00:00").unwrap(),
            at("2026-03-04 09:00:00")
        );
        assert_eq!(
            parse_datetime("2026-03-04T09:00:00").unwrap(),
            at("2026-03-04 09:00:00")
        );
        assert_eq!(
            parse_datetime("2026-03-04 09:00:00.123456").unwrap(),
            at("2026-03-04 09:00:00")
        );
        assert!(parse_datetime("not a date").is_err());
    }
}

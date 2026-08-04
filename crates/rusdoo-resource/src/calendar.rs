//! Port of `resource_calendar.py`'s computation API: turning a weekly
//! schedule plus a list of time off into "when is this person actually
//! working", and answering the four questions every planning module asks
//! of it — how many hours, how many days, when will N hours be done, and
//! when will N days be.
//!
//! Everything here is pure: it takes a schedule and gives an answer.
//! What loads the schedule out of the database is [`crate::load`], and
//! what exposes the answers over RPC is [`crate::lib`]'s methods. The
//! split is deliberate — this is the part with the arithmetic in it, and
//! arithmetic that needs a database to be tested does not get tested.
//!
//! **Time zones.** Odoo does every step of this in the calendar's own
//! `tz` and converts at the edges. The port has no timezone-aware
//! datetime layer (see the crate docs), so every datetime here is the
//! calendar's own wall clock, naive. For a UTC calendar — the default —
//! the two are the same thing. For any other, the caller is responsible
//! for the shift, and the day a `chrono-tz` lands in the framework this
//! module gets a conversion at its two edges and nothing else changes.

use crate::intervals::{AttendanceRef, Attendances, Intervals};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::collections::BTreeMap;

/// `resource.calendar.attendance` as the arithmetic needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Attendance {
    pub id: i64,
    /// 0 = Monday, like Odoo's `dayofweek` selection
    pub dayofweek: u32,
    pub hour_from: f64,
    pub hour_to: f64,
    /// `morning` | `lunch` | `afternoon` | `full_day`
    pub day_period: String,
    /// `Some(0)` first week, `Some(1)` second — only on a two-week calendar
    pub week_type: Option<u8>,
    /// `Some("line_section")` for the header rows of a two-week calendar
    pub display_type: Option<String>,
    pub sequence: i32,
}

impl Attendance {
    /// `_is_work_period`: a break is not worked, and a section header is
    /// not a period at all.
    pub fn is_work_period(&self) -> bool {
        self.day_period != "lunch" && self.display_type.is_none()
    }

    /// `_compute_duration_hours`: a break lasts no working time.
    pub fn duration_hours(&self) -> f64 {
        if self.day_period == "lunch" {
            0.0
        } else {
            self.hour_to - self.hour_from
        }
    }

    /// `_compute_duration_days`: a full day is one, a break is none, and
    /// a half day only counts as half while it stays short of three
    /// quarters of the calendar's ordinary day.
    pub fn duration_days(&self, hours_per_day: f64) -> f64 {
        match self.day_period.as_str() {
            "lunch" => 0.0,
            "full_day" => 1.0,
            _ if self.duration_hours() <= hours_per_day * 3.0 / 4.0 => 0.5,
            _ => 1.0,
        }
    }

    fn as_ref(&self, hours_per_day: f64) -> AttendanceRef {
        AttendanceRef::new(self.id, self.duration_hours(), self.duration_days(hours_per_day))
    }
}

/// `resource.calendar.leaves` as the arithmetic needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Leave {
    pub id: i64,
    pub date_from: NaiveDateTime,
    pub date_to: NaiveDateTime,
}

/// A working schedule: what `resource.calendar` says, without the ORM.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub two_weeks_calendar: bool,
    /// `flexible_hours`: no fixed periods, only a weekly total
    pub flexible_hours: bool,
    /// `duration_based`: the hours are a length centred on midday, not a
    /// pair of clock times
    pub duration_based: bool,
    pub attendances: Vec<Attendance>,
    /// `hours_per_day`/`hours_per_week` as *written on the record*. Odoo
    /// computes them from the attendances and lets them be overridden,
    /// which is the only way a flexible calendar — which has no
    /// attendances — can have them at all.
    pub hours_per_day: f64,
    pub hours_per_week: f64,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            two_weeks_calendar: false,
            flexible_hours: false,
            duration_based: false,
            attendances: Vec::new(),
            hours_per_day: 0.0,
            hours_per_week: 0.0,
        }
    }
}

/// Odoo's default when a calendar cannot say (`resource/models/utils.py`).
pub const HOURS_PER_DAY: f64 = 8.0;

/// `get_week_type` — which of the two weeks a date falls in.
///
/// Counted in whole weeks since day one of the proleptic Gregorian
/// calendar rather than by ISO week number, and Odoo's comment says why:
/// some years have 53 weeks, so with ISO numbering two odd weeks would
/// follow each other and a fortnightly schedule would skip a beat.
pub fn week_type(date: NaiveDate) -> u8 {
    let ordinal = i64::from(date.num_days_from_ce());
    (((ordinal - 1).div_euclid(7)) % 2) as u8
}

/// `float_to_time` — 8.5 is half past eight.
///
/// 24.0 is the end of the day and not the start of the next one, which
/// is what lets an attendance run to midnight without spilling into
/// tomorrow.
pub fn float_to_time(hours: f64) -> NaiveTime {
    if hours == 24.0 {
        return NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).expect("a valid time");
    }
    let hour = hours.trunc().clamp(0.0, 23.0) as u32;
    // rounded, not truncated: 8.999 is nine o'clock and not 8:59:56
    let minute = (hours.fract() * 60.0).round().clamp(0.0, 59.0) as u32;
    NaiveTime::from_hms_opt(hour, minute, 0).expect("hour and minute are clamped")
}

/// `time_to_float` — the inverse, for the hour a datetime falls on.
pub fn time_to_float(time: NaiveTime) -> f64 {
    f64::from(time.hour()) + f64::from(time.minute()) / 60.0 + f64::from(time.second()) / 3600.0
}

/// Hours between two moments, the way every duration in this module is
/// counted.
pub fn hours_between(start: NaiveDateTime, stop: NaiveDateTime) -> f64 {
    (stop - start).num_milliseconds() as f64 / 3_600_000.0
}

fn round_to(value: f64, step: f64) -> f64 {
    (value / step).round() * step
}

impl Schedule {
    /// `_get_global_attendances`: the periods that are actually worked.
    pub fn worked_attendances(&self) -> impl Iterator<Item = &Attendance> {
        self.attendances.iter().filter(|a| a.is_work_period())
    }

    /// `_get_hours_per_week` — the average, which on a two-week calendar
    /// is half of what the fortnight holds.
    pub fn computed_hours_per_week(&self) -> f64 {
        let total: f64 = self
            .worked_attendances()
            .map(|a| {
                if self.duration_based {
                    a.duration_hours()
                } else {
                    a.hour_to - a.hour_from
                }
            })
            .sum();
        if self.two_weeks_calendar {
            total / 2.0
        } else {
            total
        }
    }

    /// `_get_days_per_week`. A day somebody works at all is a day worked:
    /// three mornings a week is three days, not one and a half.
    pub fn computed_days_per_week(&self) -> f64 {
        let count = |week: Option<u8>| -> usize {
            let mut days: Vec<u32> = self
                .worked_attendances()
                .filter(|a| week.is_none() || a.week_type == week)
                .map(|a| a.dayofweek)
                .collect();
            days.sort_unstable();
            days.dedup();
            days.len()
        };
        if self.two_weeks_calendar {
            (count(Some(0)) + count(Some(1))) as f64 / 2.0
        } else {
            count(None) as f64
        }
    }

    /// `_get_hours_per_day` — the average over the days that are worked,
    /// not over seven.
    pub fn computed_hours_per_day(&self) -> f64 {
        let days = self.computed_days_per_week();
        if days == 0.0 {
            0.0
        } else {
            self.computed_hours_per_week() / days
        }
    }

    /// `_works_on_date`.
    pub fn works_on(&self, date: NaiveDate) -> bool {
        let wanted = date.weekday().num_days_from_monday();
        let week = self.two_weeks_calendar.then(|| week_type(date));
        self.attendances
            .iter()
            .any(|a| a.dayofweek == wanted && (week.is_none() || a.week_type == week))
    }

    /// `_get_hours_for_date` — when the working day starts and ends on a
    /// given date, optionally for one half of it.
    ///
    /// Odoo asks the database for this with a `read_group`; here the
    /// attendances are already loaded, so it is the same aggregation done
    /// in memory. When the date is not a working day the answer falls
    /// back to the calendar's widest hours — which is Odoo's behaviour
    /// and the reason a leave taken on a Sunday still has a length.
    pub fn hours_for_date(&self, date: NaiveDate, day_period: Option<&str>) -> (f64, f64) {
        if self.flexible_hours {
            // centred on midday, since a flexible day has no clock times
            let half = self.hours_per_day / 2.0;
            return match day_period {
                Some("morning") => (12.0 - half, 12.0),
                Some(_) => (12.0, 12.0 + half),
                None => (12.0 - half, 12.0 + half),
            };
        }
        let usable: Vec<&Attendance> = self
            .attendances
            .iter()
            .filter(|a| a.display_type.is_none() && a.day_period != "lunch")
            .collect();
        // a full-day attendance is split at its midpoint when only one
        // half of the day is asked for
        let mut periods: Vec<(Option<u8>, u32, f64, f64)> = Vec::new();
        for attendance in &usable {
            let (from, to) = (attendance.hour_from, attendance.hour_to);
            match (day_period, attendance.day_period.as_str()) {
                (None, _) => periods.push((attendance.week_type, attendance.dayofweek, from, to)),
                (Some(wanted), period) if period == wanted => {
                    periods.push((attendance.week_type, attendance.dayofweek, from, to))
                }
                (Some(wanted), "full_day") => {
                    let half = (from + to) / 2.0;
                    let (from, to) = if wanted == "morning" {
                        (from, half)
                    } else {
                        (half, to)
                    };
                    periods.push((attendance.week_type, attendance.dayofweek, from, to));
                }
                _ => {}
            }
        }
        let default_start = periods.iter().map(|p| p.2).fold(f64::INFINITY, f64::min);
        let default_end = periods.iter().map(|p| p.3).fold(f64::NEG_INFINITY, f64::max);
        let default_start = if default_start.is_finite() { default_start } else { 0.0 };
        let default_end = if default_end.is_finite() { default_end } else { 0.0 };

        let week = self.two_weeks_calendar.then(|| week_type(date));
        let wanted_day = date.weekday().num_days_from_monday();
        let today: Vec<&(Option<u8>, u32, f64, f64)> = periods
            .iter()
            .filter(|(week_type, dayofweek, _, _)| *week_type == week && *dayofweek == wanted_day)
            .collect();
        if today.is_empty() {
            return (default_start, default_end);
        }
        (
            today.iter().map(|p| p.2).fold(f64::INFINITY, f64::min),
            today.iter().map(|p| p.3).fold(f64::NEG_INFINITY, f64::max),
        )
    }
}

/// `_attendance_intervals_batch` for a calendar on its own — the working
/// periods it declares, clipped to the window asked about.
pub fn attendance_intervals(
    schedule: &Schedule,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Intervals<NaiveDateTime> {
    if schedule.flexible_hours {
        return flexible_attendance_intervals(schedule, start, end);
    }
    let hours_per_day = effective_hours_per_day(schedule);
    // indexed by weekday, twice over: the first week's seven days and
    // the second's, so a fortnightly schedule is one lookup like any
    // other
    let mut per_day: Vec<Vec<&Attendance>> = vec![Vec::new(); 14];
    for attendance in &schedule.attendances {
        if attendance.display_type.is_some() || attendance.day_period == "lunch" {
            continue;
        }
        let day = attendance.dayofweek as usize % 7;
        if schedule.two_weeks_calendar {
            let week = attendance.week_type.unwrap_or(0) as usize;
            per_day[day + 7 * week].push(attendance);
        } else {
            per_day[day].push(attendance);
            per_day[day + 7].push(attendance);
        }
    }

    let mut items = Vec::new();
    let mut day = start.date();
    let last = end.date();
    while day <= last {
        let week = week_type(day) as usize;
        let index = day.weekday().num_days_from_monday() as usize + 7 * week;
        for attendance in &per_day[index] {
            let from = day.and_time(float_to_time(attendance.hour_from));
            let to = day.and_time(float_to_time(attendance.hour_to));
            items.push((
                from.max(start),
                to.min(end),
                Attendances::one(attendance.as_ref(hours_per_day)),
            ));
        }
        day += Duration::days(1);
    }
    // distinct: a morning and an afternoon that touch are two
    // attendances with two durations, and merging them would make a
    // half day look like a whole one
    Intervals::distinct(items)
}

/// The day length the durations are measured against: what the record
/// says, or what the attendances imply when it says nothing.
fn effective_hours_per_day(schedule: &Schedule) -> f64 {
    if schedule.hours_per_day > 0.0 {
        schedule.hours_per_day
    } else {
        schedule.computed_hours_per_day()
    }
}

/// The flexible calendar's branch of `_attendance_intervals_batch`.
///
/// A flexible schedule says only "so many hours a week"; the intervals
/// are invented, one per day, centred on midday and capped by the day's
/// and the week's totals. Odoo's comment gives the reason: it is the
/// closest approximation that still answers a daily *and* a weekly
/// question correctly, which is what overtime and time-off accrual need.
fn flexible_attendance_intervals(
    schedule: &Schedule,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Intervals<NaiveDateTime> {
    let window_hours = hours_between(start, end);
    let max_per_week = schedule.hours_per_week;
    let max_per_day = schedule.hours_per_day;
    if max_per_week <= 0.0 || max_per_day <= 0.0 {
        return Intervals::empty(true);
    }
    // the last instant that still belongs to the window, so a window
    // ending at midnight does not open one more day
    let end_inclusive = end - Duration::seconds(1);

    let mut items = Vec::new();
    let mut week_start = start;
    while week_start <= end_inclusive {
        let week_end = (week_start + Duration::days(6)).min(end_inclusive);
        let mut remaining = max_per_week.min(window_hours);
        let mut day = week_start.max(start);
        while day <= week_end {
            if remaining <= 0.0 {
                break;
            }
            let day_start = day.date().and_time(NaiveTime::MIN);
            let day_end = day
                .date()
                .and_time(NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).expect("end of day"));
            let from = day_start.max(start);
            let to = day_end.min(end);
            let allocated = max_per_day.min(remaining).min(hours_between(from, to));
            if allocated > 0.0 {
                remaining -= allocated;
                let midpoint = day.date().and_hms_opt(12, 0, 0).expect("midday");
                let half = Duration::milliseconds((allocated * 1_800_000.0) as i64);
                let mut interval_start = midpoint - half;
                let mut interval_end = midpoint + half;
                let span = Duration::milliseconds((allocated * 3_600_000.0) as i64);
                if interval_start < from {
                    interval_start = from;
                    interval_end = interval_start + span;
                } else if interval_end > to {
                    interval_end = to;
                    interval_start = interval_end - span;
                }
                items.push((
                    interval_start,
                    interval_end,
                    Attendances::one(AttendanceRef::synthetic(allocated, 1.0)),
                ));
            }
            day += Duration::days(1);
        }
        week_start += Duration::days(7);
    }
    Intervals::distinct(items)
}

/// `_leave_intervals_batch` — time off, clipped to the window.
pub fn leave_intervals(
    leaves: &[Leave],
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Intervals<NaiveDateTime> {
    Intervals::new(leaves.iter().map(|leave| {
        (
            leave.date_from.max(start),
            leave.date_to.min(end),
            Attendances::none(),
        )
    }))
}

/// `_work_intervals_batch` — what is left of the schedule once the time
/// off is taken out.
pub fn work_intervals(
    schedule: &Schedule,
    leaves: &[Leave],
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Intervals<NaiveDateTime> {
    let attendance = attendance_intervals(schedule, start, end);
    if leaves.is_empty() {
        return attendance;
    }
    attendance.difference(&leave_intervals(leaves, start, end))
}

/// `get_work_hours_count` — how many hours of work the window holds.
pub fn work_hours_count(
    schedule: &Schedule,
    leaves: &[Leave],
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> f64 {
    work_intervals(schedule, leaves, start, end)
        .iter()
        .map(|(from, to, _)| hours_between(*from, *to))
        .sum()
}

/// What a duration looks like once both units are wanted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Duration2 {
    pub days: f64,
    pub hours: f64,
}

/// `_get_attendance_intervals_days_data` — the same stretch of time in
/// days and in hours.
///
/// The hours are just added up. The days cannot be: a day is whatever
/// the attendance says it is, so an interval contributes the *fraction*
/// of its attendance that it covers. Four hours of an eight-hour full
/// day is half a day; four hours of a four-hour morning is a whole one,
/// because that morning is what the calendar calls a day's work.
pub fn attendance_days_data(
    schedule: &Schedule,
    intervals: &Intervals<NaiveDateTime>,
) -> Duration2 {
    let mut day_hours: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    let mut day_days: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for (start, stop, payload) in intervals.iter() {
        let hours = hours_between(*start, *stop);
        *day_hours.entry(start.date()).or_default() += hours;
        let days = if schedule.flexible_hours {
            if schedule.hours_per_day > 0.0 {
                hours / schedule.hours_per_day
            } else {
                0.0
            }
        } else {
            let declared_hours = payload.total_hours();
            if declared_hours == 0.0 {
                0.0
            } else {
                payload.total_days() * hours / declared_hours
            }
        };
        *day_days.entry(start.date()).or_default() += days;
    }
    Duration2 {
        // Odoo rounds to the closest sixteenth of a day; the comment in
        // the source says so and the number it uses is 0.001
        days: round_to(day_days.values().sum::<f64>(), 0.001),
        hours: day_hours.values().sum(),
    }
}

/// How far `plan_hours` and `plan_days` will look before giving up:
/// a hundred fortnights, as in Odoo. A schedule with no working time at
/// all would otherwise be an endless loop.
const SEARCH_WINDOWS: i64 = 100;
const WINDOW: i64 = 14;

/// `plan_hours` — when will `hours` of work be done, counting from
/// `from`. A negative number counts backwards.
///
/// `None` is Odoo's `False`: there is no such moment within the search
/// horizon, which for an empty calendar is every moment.
pub fn plan_hours(
    schedule: &Schedule,
    leaves: &[Leave],
    hours: f64,
    from: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let intervals = |start: NaiveDateTime, end: NaiveDateTime| work_intervals(schedule, leaves, start, end);
    if hours >= 0.0 {
        let mut left = hours;
        for step in 0..SEARCH_WINDOWS {
            let start = from + Duration::days(WINDOW * step);
            let end = start + Duration::days(WINDOW);
            for (interval_start, interval_stop, _) in intervals(start, end).iter() {
                let span = hours_between(*interval_start, *interval_stop);
                if left <= span {
                    return Some(*interval_start + minutes_of(left));
                }
                left -= span;
            }
        }
        return None;
    }
    let mut left = hours.abs();
    for step in 0..SEARCH_WINDOWS {
        let end = from - Duration::days(WINDOW * step);
        let start = end - Duration::days(WINDOW);
        for (interval_start, interval_stop, _) in intervals(start, end).iter().rev() {
            let span = hours_between(*interval_start, *interval_stop);
            if left <= span {
                return Some(*interval_stop - minutes_of(left));
            }
            left -= span;
        }
    }
    None
}

/// `plan_days` — the end of the `days`-th working day from `from`.
///
/// A day counts as soon as any of it is worked, which is why the search
/// is over the days the intervals *start* on and not over their length.
pub fn plan_days(
    schedule: &Schedule,
    leaves: &[Leave],
    days: i64,
    from: NaiveDateTime,
) -> Option<NaiveDateTime> {
    if days == 0 {
        return Some(from);
    }
    let intervals = |start: NaiveDateTime, end: NaiveDateTime| work_intervals(schedule, leaves, start, end);
    let mut found: Vec<NaiveDate> = Vec::new();
    if days > 0 {
        for step in 0..SEARCH_WINDOWS {
            let start = from + Duration::days(WINDOW * step);
            let end = start + Duration::days(WINDOW);
            for (interval_start, interval_stop, _) in intervals(start, end).iter() {
                if !found.contains(&interval_start.date()) {
                    found.push(interval_start.date());
                }
                if found.len() as i64 == days {
                    return Some(*interval_stop);
                }
            }
        }
        return None;
    }
    let wanted = days.abs();
    for step in 0..SEARCH_WINDOWS {
        let end = from - Duration::days(WINDOW * step);
        let start = end - Duration::days(WINDOW);
        for (interval_start, _, _) in intervals(start, end).iter().rev() {
            if !found.contains(&interval_start.date()) {
                found.push(interval_start.date());
            }
            if found.len() as i64 == wanted {
                return Some(*interval_start);
            }
        }
    }
    None
}

fn minutes_of(hours: f64) -> Duration {
    Duration::milliseconds((hours * 3_600_000.0).round() as i64)
}

/// `_get_closest_work_time` — the nearest moment work starts (or ends,
/// with `match_end`) inside a window.
pub fn closest_work_time(
    schedule: &Schedule,
    leaves: &[Leave],
    at: NaiveDateTime,
    range: (NaiveDateTime, NaiveDateTime),
    match_end: bool,
) -> Option<NaiveDateTime> {
    if at < range.0 || at > range.1 {
        return None;
    }
    let intervals = work_intervals(schedule, leaves, range.0, range.1);
    intervals
        .iter()
        .map(|(start, stop, _)| if match_end { *stop } else { *start })
        .min_by_key(|moment| (*moment - at).num_milliseconds().abs())
}

/// `_unavailable_intervals` — the gaps between the working periods,
/// which is what a gantt paints grey.
pub fn unavailable_intervals(
    schedule: &Schedule,
    leaves: &[Leave],
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Vec<(NaiveDateTime, NaiveDateTime)> {
    let work = work_intervals(schedule, leaves, start, end);
    let mut edges: Vec<NaiveDateTime> = Vec::with_capacity(work.len() * 2 + 2);
    edges.push(start);
    for (from, to, _) in work.iter() {
        edges.push(*from);
        edges.push(*to);
    }
    edges.push(end);
    edges
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|(from, to)| from < to)
        .collect()
}

/// `_get_unusual_days` — for each day in the window, is it a day this
/// calendar does *not* work? What a date picker greys out.
pub fn unusual_days(
    schedule: &Schedule,
    leaves: &[Leave],
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> BTreeMap<NaiveDate, bool> {
    let intervals = work_intervals(schedule, leaves, start, end);
    let worked: Vec<NaiveDate> = intervals.iter().map(|(from, _, _)| from.date()).collect();
    let mut answer = BTreeMap::new();
    let mut day = start.date();
    while day <= end.date() {
        // a flexible calendar has no unusual days of its own: what is
        // unusual there is the time off, and Odoo inverts the question
        let unusual = if schedule.flexible_hours {
            leaves
                .iter()
                .any(|leave| leave.date_from.date() <= day && day <= leave.date_to.date())
        } else {
            !worked.contains(&day)
        };
        answer.insert(day, unusual);
        day += Duration::days(1);
    }
    answer
}

/// `_check_overlap` — two working periods on the same weekday that share
/// time.
///
/// The check is Odoo's: lay every attendance out on a single line seven
/// days long, normalize, and see whether anything merged. The
/// millionth-of-an-hour added to each start is Odoo's too, and its
/// comment says why — without it two periods that merely touch would
/// merge and read as an overlap.
pub fn overlapping(attendances: &[&Attendance]) -> bool {
    let laid_out: Vec<(f64, f64, Attendances)> = attendances
        .iter()
        .map(|a| {
            let day = f64::from(a.dayofweek) * 24.0;
            (
                day + a.hour_from + 0.000_001,
                day + a.hour_to,
                Attendances::none(),
            )
        })
        .collect();
    let expected = laid_out.len();
    Intervals::new(laid_out).len() != expected
}

/// `_check_attendance_ids` — every rule a calendar's working time has to
/// satisfy, and the sentence to show when it does not.
pub fn check_attendances(schedule: &Schedule) -> Result<(), String> {
    let sections: Vec<&Attendance> = schedule
        .attendances
        .iter()
        .filter(|a| a.display_type.as_deref() == Some("line_section"))
        .collect();
    if schedule.two_weeks_calendar && !sections.is_empty() {
        let first = schedule
            .attendances
            .iter()
            .min_by_key(|a| (a.sequence, a.id))
            .expect("there is at least one section");
        if first.display_type.is_none() {
            return Err(
                "in a two-week calendar every period belongs to one of the two week sections"
                    .into(),
            );
        }
    }
    let worked: Vec<&Attendance> = schedule
        .attendances
        .iter()
        .filter(|a| a.display_type.is_none())
        .collect();
    let clashes = if schedule.two_weeks_calendar {
        [Some(0), Some(1)].into_iter().any(|week| {
            let week_attendances: Vec<&Attendance> = worked
                .iter()
                .copied()
                .filter(|a| a.week_type == week)
                .collect();
            overlapping(&week_attendances)
        })
    } else {
        overlapping(&worked)
    };
    if clashes {
        return Err("working periods cannot overlap".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn attendance(id: i64, dayofweek: u32, from: f64, to: f64, period: &str) -> Attendance {
        Attendance {
            id,
            dayofweek,
            hour_from: from,
            hour_to: to,
            day_period: period.into(),
            week_type: None,
            display_type: None,
            sequence: 10,
        }
    }

    /// The calendar every Odoo starts with: 8–12, lunch, 13–17, Monday
    /// to Friday.
    fn standard() -> Schedule {
        let mut attendances = Vec::new();
        for day in 0..5u32 {
            let base = i64::from(day) * 3;
            attendances.push(attendance(base + 1, day, 8.0, 12.0, "morning"));
            attendances.push(attendance(base + 2, day, 12.0, 13.0, "lunch"));
            attendances.push(attendance(base + 3, day, 13.0, 17.0, "afternoon"));
        }
        Schedule {
            attendances,
            hours_per_day: 8.0,
            hours_per_week: 40.0,
            ..Schedule::default()
        }
    }

    #[test]
    fn a_standard_calendar_is_forty_hours_over_five_days() {
        let calendar = standard();
        assert_eq!(calendar.computed_hours_per_week(), 40.0);
        assert_eq!(calendar.computed_days_per_week(), 5.0);
        assert_eq!(calendar.computed_hours_per_day(), 8.0);
    }

    #[test]
    fn a_break_is_not_working_time() {
        let calendar = standard();
        // nine periods a week are worked; the five lunches are not
        assert_eq!(calendar.worked_attendances().count(), 10);
    }

    #[test]
    fn a_two_week_calendar_averages_the_fortnight() {
        let mut calendar = standard();
        calendar.two_weeks_calendar = true;
        for (index, attendance) in calendar.attendances.iter_mut().enumerate() {
            attendance.week_type = Some(if index < 6 { 0 } else { 1 });
        }
        // the same periods, now spread over two weeks: half the average
        assert_eq!(calendar.computed_hours_per_week(), 20.0);
    }

    #[test]
    fn half_past_eight_is_half_past_eight_and_midnight_is_the_end_of_the_day() {
        assert_eq!(float_to_time(8.5), NaiveTime::from_hms_opt(8, 30, 0).unwrap());
        assert_eq!(float_to_time(0.0), NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        // 24.0 is 23:59:59.999999, so an attendance running to midnight
        // stays inside its own day
        assert_eq!(float_to_time(24.0).hour(), 23);
        assert_eq!(float_to_time(24.0).minute(), 59);
    }

    #[test]
    fn the_two_weeks_alternate_without_ever_repeating() {
        // whatever ISO week numbering does at a year's end, one week
        // always follows the other
        let mut day = NaiveDate::from_ymd_opt(2025, 12, 1).unwrap();
        let mut seen = week_type(day);
        for _ in 0..10 {
            day += Duration::days(7);
            let next = week_type(day);
            assert_ne!(next, seen, "{day} repeats the week before it");
            seen = next;
        }
    }

    #[test]
    fn a_working_day_yields_the_morning_and_the_afternoon_and_not_the_break() {
        let calendar = standard();
        // Wednesday 4 June 2025
        let intervals = attendance_intervals(&calendar, at(2025, 6, 4, 0, 0), at(2025, 6, 4, 23, 59));
        let bounds: Vec<(NaiveDateTime, NaiveDateTime)> =
            intervals.iter().map(|(a, b, _)| (*a, *b)).collect();
        assert_eq!(
            bounds,
            vec![
                (at(2025, 6, 4, 8, 0), at(2025, 6, 4, 12, 0)),
                (at(2025, 6, 4, 13, 0), at(2025, 6, 4, 17, 0)),
            ]
        );
    }

    #[test]
    fn a_weekend_has_no_working_time() {
        let calendar = standard();
        // Saturday 7 June 2025
        let intervals = attendance_intervals(&calendar, at(2025, 6, 7, 0, 0), at(2025, 6, 8, 23, 59));
        assert!(intervals.is_empty());
    }

    #[test]
    fn a_window_that_starts_mid_morning_is_clipped_and_not_extended() {
        let calendar = standard();
        let intervals = attendance_intervals(&calendar, at(2025, 6, 4, 10, 0), at(2025, 6, 4, 14, 0));
        let bounds: Vec<(NaiveDateTime, NaiveDateTime)> =
            intervals.iter().map(|(a, b, _)| (*a, *b)).collect();
        assert_eq!(
            bounds,
            vec![
                (at(2025, 6, 4, 10, 0), at(2025, 6, 4, 12, 0)),
                (at(2025, 6, 4, 13, 0), at(2025, 6, 4, 14, 0)),
            ]
        );
    }

    #[test]
    fn a_working_week_counts_forty_hours() {
        let calendar = standard();
        let hours = work_hours_count(
            &calendar,
            &[],
            at(2025, 6, 2, 0, 0),
            at(2025, 6, 8, 23, 59),
        );
        assert_eq!(hours, 40.0);
    }

    #[test]
    fn a_public_holiday_takes_its_day_out_of_the_week() {
        let calendar = standard();
        let holiday = Leave {
            id: 1,
            date_from: at(2025, 6, 4, 0, 0),
            date_to: at(2025, 6, 4, 23, 59),
        };
        let hours = work_hours_count(
            &calendar,
            &[holiday],
            at(2025, 6, 2, 0, 0),
            at(2025, 6, 8, 23, 59),
        );
        assert_eq!(hours, 32.0, "one working day fewer");
    }

    #[test]
    fn an_afternoon_off_takes_only_the_afternoon() {
        let calendar = standard();
        let leave = Leave {
            id: 1,
            date_from: at(2025, 6, 4, 13, 0),
            date_to: at(2025, 6, 4, 17, 0),
        };
        let intervals = work_intervals(
            &calendar,
            &[leave],
            at(2025, 6, 4, 0, 0),
            at(2025, 6, 4, 23, 59),
        );
        let bounds: Vec<(NaiveDateTime, NaiveDateTime)> =
            intervals.iter().map(|(a, b, _)| (*a, *b)).collect();
        assert_eq!(bounds, vec![(at(2025, 6, 4, 8, 0), at(2025, 6, 4, 12, 0))]);
    }

    #[test]
    fn a_full_day_off_is_one_day_and_eight_hours() {
        let calendar = standard();
        let intervals = work_intervals(
            &calendar,
            &[],
            at(2025, 6, 4, 0, 0),
            at(2025, 6, 4, 23, 59),
        );
        let data = attendance_days_data(&calendar, &intervals);
        assert_eq!(data.hours, 8.0);
        // a morning and an afternoon of a standard calendar are half a
        // day each
        assert_eq!(data.days, 1.0);
    }

    #[test]
    fn half_of_a_morning_is_a_quarter_of_a_day() {
        let calendar = standard();
        let intervals = work_intervals(
            &calendar,
            &[],
            at(2025, 6, 4, 8, 0),
            at(2025, 6, 4, 10, 0),
        );
        let data = attendance_days_data(&calendar, &intervals);
        assert_eq!(data.hours, 2.0);
        // the morning is half a day and only half of it was worked
        assert_eq!(data.days, 0.25);
    }

    #[test]
    fn planning_four_hours_from_monday_morning_lands_at_noon() {
        let calendar = standard();
        let planned = plan_hours(&calendar, &[], 4.0, at(2025, 6, 2, 8, 0));
        assert_eq!(planned, Some(at(2025, 6, 2, 12, 0)));
    }

    #[test]
    fn planning_across_the_break_skips_it() {
        let calendar = standard();
        // five hours from eight is four in the morning and one after the
        // break, which is two in the afternoon and not one
        let planned = plan_hours(&calendar, &[], 5.0, at(2025, 6, 2, 8, 0));
        assert_eq!(planned, Some(at(2025, 6, 2, 14, 0)));
    }

    #[test]
    fn planning_across_the_weekend_lands_on_monday() {
        let calendar = standard();
        // one hour left on Friday afternoon plus two more
        let planned = plan_hours(&calendar, &[], 3.0, at(2025, 6, 6, 16, 0));
        assert_eq!(planned, Some(at(2025, 6, 9, 10, 0)));
    }

    #[test]
    fn planning_backwards_walks_back_through_the_schedule() {
        let calendar = standard();
        let planned = plan_hours(&calendar, &[], -4.0, at(2025, 6, 4, 17, 0));
        assert_eq!(planned, Some(at(2025, 6, 4, 13, 0)));
    }

    #[test]
    fn planning_on_an_empty_calendar_answers_nothing_rather_than_looping() {
        let empty = Schedule::default();
        assert_eq!(plan_hours(&empty, &[], 1.0, at(2025, 6, 2, 8, 0)), None);
        assert_eq!(plan_days(&empty, &[], 1, at(2025, 6, 2, 8, 0)), None);
    }

    #[test]
    fn planning_three_days_ends_when_the_third_day_is_reached() {
        let calendar = standard();
        let planned = plan_days(&calendar, &[], 3, at(2025, 6, 2, 0, 0));
        // Wednesday noon, not Wednesday evening: Odoo answers with the
        // end of the *interval* that brought the count up, and the first
        // interval of Wednesday is its morning. Reproduced rather than
        // corrected — a caller who wants the end of the day asks
        // `plan_hours`, and quietly disagreeing with Odoo here would
        // move every date another module computed from it.
        assert_eq!(planned, Some(at(2025, 6, 4, 12, 0)));
    }

    #[test]
    fn planning_days_backwards_lands_on_the_start_of_the_day_reached() {
        let calendar = standard();
        let planned = plan_days(&calendar, &[], -2, at(2025, 6, 6, 23, 0));
        // Friday is the first day back, Thursday the second, and the
        // last interval of Thursday is its afternoon
        assert_eq!(planned, Some(at(2025, 6, 5, 13, 0)));
    }

    #[test]
    fn planning_zero_days_stays_put() {
        let calendar = standard();
        let start = at(2025, 6, 2, 9, 30);
        assert_eq!(plan_days(&calendar, &[], 0, start), Some(start));
    }

    #[test]
    fn the_closest_start_of_work_is_the_start_of_the_morning() {
        let calendar = standard();
        let closest = closest_work_time(
            &calendar,
            &[],
            at(2025, 6, 4, 9, 0),
            (at(2025, 6, 4, 0, 0), at(2025, 6, 4, 23, 59)),
            false,
        );
        assert_eq!(closest, Some(at(2025, 6, 4, 8, 0)));
    }

    #[test]
    fn the_closest_end_of_work_is_the_end_of_the_afternoon() {
        let calendar = standard();
        let closest = closest_work_time(
            &calendar,
            &[],
            at(2025, 6, 4, 18, 0),
            (at(2025, 6, 4, 0, 0), at(2025, 6, 4, 23, 59)),
            true,
        );
        assert_eq!(closest, Some(at(2025, 6, 4, 17, 0)));
    }

    #[test]
    fn a_moment_outside_the_search_range_has_no_closest_work_time() {
        let calendar = standard();
        let closest = closest_work_time(
            &calendar,
            &[],
            at(2025, 6, 10, 9, 0),
            (at(2025, 6, 4, 0, 0), at(2025, 6, 4, 23, 59)),
            false,
        );
        assert_eq!(closest, None);
    }

    #[test]
    fn the_unavailable_intervals_are_the_gaps_around_the_work() {
        let calendar = standard();
        let gaps = unavailable_intervals(
            &calendar,
            &[],
            at(2025, 6, 4, 0, 0),
            at(2025, 6, 4, 23, 59),
        );
        assert_eq!(
            gaps,
            vec![
                (at(2025, 6, 4, 0, 0), at(2025, 6, 4, 8, 0)),
                (at(2025, 6, 4, 12, 0), at(2025, 6, 4, 13, 0)),
                (at(2025, 6, 4, 17, 0), at(2025, 6, 4, 23, 59)),
            ]
        );
    }

    #[test]
    fn the_weekend_is_unusual_and_the_working_days_are_not() {
        let calendar = standard();
        let days = unusual_days(&calendar, &[], at(2025, 6, 2, 0, 0), at(2025, 6, 8, 23, 59));
        assert!(!days[&NaiveDate::from_ymd_opt(2025, 6, 4).unwrap()]);
        assert!(days[&NaiveDate::from_ymd_opt(2025, 6, 7).unwrap()]);
        assert!(days[&NaiveDate::from_ymd_opt(2025, 6, 8).unwrap()]);
    }

    #[test]
    fn a_public_holiday_makes_a_working_day_unusual() {
        let calendar = standard();
        let holiday = Leave {
            id: 1,
            date_from: at(2025, 6, 4, 0, 0),
            date_to: at(2025, 6, 4, 23, 59),
        };
        let days = unusual_days(
            &calendar,
            &[holiday],
            at(2025, 6, 2, 0, 0),
            at(2025, 6, 6, 23, 59),
        );
        assert!(days[&NaiveDate::from_ymd_opt(2025, 6, 4).unwrap()]);
    }

    #[test]
    fn two_periods_that_only_touch_do_not_overlap() {
        let morning = attendance(1, 0, 8.0, 12.0, "morning");
        let afternoon = attendance(2, 0, 12.0, 17.0, "afternoon");
        assert!(!overlapping(&[&morning, &afternoon]));
    }

    #[test]
    fn two_periods_that_share_an_hour_overlap() {
        let morning = attendance(1, 0, 8.0, 13.0, "morning");
        let afternoon = attendance(2, 0, 12.0, 17.0, "afternoon");
        assert!(overlapping(&[&morning, &afternoon]));
    }

    #[test]
    fn the_same_hours_on_different_days_do_not_overlap() {
        let monday = attendance(1, 0, 8.0, 12.0, "morning");
        let tuesday = attendance(2, 1, 8.0, 12.0, "morning");
        assert!(!overlapping(&[&monday, &tuesday]));
    }

    #[test]
    fn a_calendar_whose_periods_clash_is_refused_with_a_reason() {
        let mut calendar = standard();
        calendar
            .attendances
            .push(attendance(99, 0, 11.0, 14.0, "afternoon"));
        let error = check_attendances(&calendar).expect_err("11–14 sits on top of the morning");
        assert!(error.contains("overlap"), "{error}");
    }

    #[test]
    fn a_two_week_calendar_may_repeat_the_same_hours_in_each_week() {
        let mut calendar = standard();
        calendar.two_weeks_calendar = true;
        let mut second_week = calendar.attendances.clone();
        for attendance in &mut calendar.attendances {
            attendance.week_type = Some(0);
        }
        for (index, attendance) in second_week.iter_mut().enumerate() {
            attendance.week_type = Some(1);
            attendance.id = 1000 + index as i64;
        }
        calendar.attendances.extend(second_week);
        // the very same Monday morning in both weeks is not an overlap:
        // the two never happen in the same week
        assert!(check_attendances(&calendar).is_ok());
    }

    #[test]
    fn a_flexible_calendar_spreads_its_weekly_hours_over_the_days() {
        // the case Odoo's own test uses: 30 hours a week, 7 a day
        let flexible = Schedule {
            flexible_hours: true,
            hours_per_day: 7.0,
            hours_per_week: 30.0,
            ..Schedule::default()
        };
        let intervals =
            attendance_intervals(&flexible, at(2025, 6, 2, 0, 0), at(2025, 6, 7, 23, 59));
        let hours: Vec<f64> = intervals
            .iter()
            .map(|(from, to, _)| (hours_between(*from, *to) * 100.0).round() / 100.0)
            .collect();
        assert_eq!(hours, vec![7.0, 7.0, 7.0, 7.0, 2.0]);
    }

    #[test]
    fn a_flexible_calendars_days_are_counted_against_its_own_day_length() {
        let flexible = Schedule {
            flexible_hours: true,
            hours_per_day: 7.0,
            hours_per_week: 30.0,
            ..Schedule::default()
        };
        let intervals =
            attendance_intervals(&flexible, at(2025, 6, 2, 0, 0), at(2025, 6, 3, 23, 59));
        let data = attendance_days_data(&flexible, &intervals);
        assert_eq!(data.hours, 14.0);
        assert_eq!(data.days, 2.0);
    }

    #[test]
    fn a_flexible_calendars_intervals_stay_inside_the_window_asked_about() {
        let flexible = Schedule {
            flexible_hours: true,
            hours_per_day: 7.0,
            hours_per_week: 30.0,
            ..Schedule::default()
        };
        let start = at(2025, 6, 2, 11, 0);
        let end = at(2025, 6, 7, 13, 0);
        let intervals = attendance_intervals(&flexible, start, end);
        for (from, to, _) in intervals.iter() {
            assert!(*from >= start, "{from} starts before the window");
            assert!(*to <= end, "{to} ends after the window");
        }
    }

    #[test]
    fn the_working_hours_of_a_date_are_the_widest_of_that_days_periods() {
        let calendar = standard();
        let wednesday = NaiveDate::from_ymd_opt(2025, 6, 4).unwrap();
        assert_eq!(calendar.hours_for_date(wednesday, None), (8.0, 17.0));
        assert_eq!(calendar.hours_for_date(wednesday, Some("morning")), (8.0, 12.0));
        assert_eq!(
            calendar.hours_for_date(wednesday, Some("afternoon")),
            (13.0, 17.0)
        );
    }

    #[test]
    fn a_day_the_calendar_does_not_work_falls_back_to_its_widest_hours() {
        let calendar = standard();
        let sunday = NaiveDate::from_ymd_opt(2025, 6, 8).unwrap();
        assert_eq!(calendar.hours_for_date(sunday, None), (8.0, 17.0));
    }

    #[test]
    fn a_calendar_knows_which_dates_it_works_on() {
        let calendar = standard();
        assert!(calendar.works_on(NaiveDate::from_ymd_opt(2025, 6, 4).unwrap()));
        assert!(!calendar.works_on(NaiveDate::from_ymd_opt(2025, 6, 7).unwrap()));
    }

    #[test]
    fn a_full_day_period_is_a_whole_day_and_a_short_morning_is_half_of_one() {
        let full = attendance(1, 0, 8.0, 16.0, "full_day");
        assert_eq!(full.duration_days(8.0), 1.0);
        let morning = attendance(2, 0, 8.0, 12.0, "morning");
        assert_eq!(morning.duration_days(8.0), 0.5);
        // a "morning" that runs most of the day is a day's work
        let long = attendance(3, 0, 8.0, 15.0, "morning");
        assert_eq!(long.duration_days(8.0), 1.0);
        let lunch = attendance(4, 0, 12.0, 13.0, "lunch");
        assert_eq!(lunch.duration_days(8.0), 0.0);
        assert_eq!(lunch.duration_hours(), 0.0);
    }
}

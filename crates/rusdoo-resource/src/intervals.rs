//! Port of `odoo/tools/intervals.py` — the algebra every working-time
//! question in Odoo is answered with.
//!
//! An interval is `(start, stop, payload)`; a set of them is kept ordered
//! and disjoint. Working time is "the attendances", time off is "the
//! leaves", and what somebody actually works is the difference of the
//! two. Writing that as set operations instead of loops is what keeps a
//! half-day of leave in the middle of an afternoon from needing a
//! special case.
//!
//! The payload is the reason this is not a plain `Vec<(T, T)>`: an
//! interval remembers which attendance rows produced it, and
//! [`crate::calendar::attendance_days_data`] needs their declared
//! durations to answer "how many *days* is that" — four hours of a
//! full-day attendance is half a day, four hours of a morning is a whole
//! one.

use std::cmp::Ordering;

/// One attendance row as an interval remembers it.
///
/// Odoo carries the `resource.calendar.attendance` recordset here. What
/// is read off it is only ever `duration_hours` and `duration_days`, and
/// carrying those two numbers instead of an id means the flexible
/// calendar's synthesized attendances — which have no row of their own,
/// exactly as in Odoo where they are `.new({...})` — fit the same type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttendanceRef {
    /// the row's id, or `None` for one the calendar made up on the spot
    pub id: Option<i64>,
    pub duration_hours: f64,
    pub duration_days: f64,
}

impl AttendanceRef {
    pub fn new(id: i64, duration_hours: f64, duration_days: f64) -> Self {
        AttendanceRef {
            id: Some(id),
            duration_hours,
            duration_days,
        }
    }

    /// The attendance a flexible calendar invents for a stretch of time
    /// nobody encoded (`self.env['resource.calendar.attendance'].new`).
    pub fn synthetic(duration_hours: f64, duration_days: f64) -> Self {
        AttendanceRef {
            id: None,
            duration_hours,
            duration_days,
        }
    }
}

/// The recordset an interval carries, and the union Odoo writes as `|`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attendances(Vec<AttendanceRef>);

impl Attendances {
    pub fn none() -> Self {
        Attendances(Vec::new())
    }

    pub fn one(attendance: AttendanceRef) -> Self {
        Attendances(vec![attendance])
    }

    pub fn iter(&self) -> impl Iterator<Item = &AttendanceRef> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn total_hours(&self) -> f64 {
        self.0.iter().map(|a| a.duration_hours).sum()
    }

    pub fn total_days(&self) -> f64 {
        self.0.iter().map(|a| a.duration_days).sum()
    }

    /// A recordset union: the same row twice is one row. A synthesized
    /// attendance has no identity to compare, so it is always kept —
    /// which is right, since two of them are two distinct stretches of
    /// invented time.
    fn union(&self, other: &Attendances) -> Attendances {
        let mut merged = self.0.clone();
        for attendance in &other.0 {
            let known = attendance
                .id
                .is_some_and(|id| merged.iter().any(|kept| kept.id == Some(id)));
            if !known {
                merged.push(*attendance);
            }
        }
        Attendances(merged)
    }
}

impl FromIterator<AttendanceRef> for Attendances {
    fn from_iter<I: IntoIterator<Item = AttendanceRef>>(iter: I) -> Self {
        Attendances(iter.into_iter().collect())
    }
}

/// Which end of an interval a boundary is. The order of the variants is
/// the order Python's `sorted` puts the flag strings in — `'start'` <
/// `'stop'` < `'switch'` — and that ordering is what makes two adjacent
/// intervals merge instead of closing and reopening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Flag {
    Start,
    Stop,
    Switch,
}

/// An ordered set of disjoint intervals with the attendances that made
/// them.
///
/// `keep_distinct` is Odoo's flag of the same name: with it off, two
/// intervals that merely touch (`8–12` and `12–13`) become one, which is
/// what you want when counting hours and wrong when the two are
/// different attendances whose durations must stay apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Intervals<T> {
    items: Vec<(T, T, Attendances)>,
    keep_distinct: bool,
}

impl<T: Copy + PartialOrd> Default for Intervals<T> {
    fn default() -> Self {
        Intervals {
            items: Vec::new(),
            keep_distinct: false,
        }
    }
}

/// A total order over values that only promise a partial one.
///
/// The times and hour floats that reach here are finite by construction;
/// treating an incomparable pair as equal keeps a sort from panicking
/// instead of turning a NaN into a crash in the middle of a payroll run.
fn order<T: PartialOrd>(left: &T, right: &T) -> Ordering {
    left.partial_cmp(right).unwrap_or(Ordering::Equal)
}

impl<T: Copy + PartialOrd> Intervals<T> {
    /// Normalize a bag of intervals into an ordered, disjoint set.
    pub fn new(intervals: impl IntoIterator<Item = (T, T, Attendances)>) -> Self {
        Self::build(intervals, false)
    }

    /// The same, keeping intervals that only touch apart.
    pub fn distinct(intervals: impl IntoIterator<Item = (T, T, Attendances)>) -> Self {
        Self::build(intervals, true)
    }

    pub fn empty(keep_distinct: bool) -> Self {
        Intervals {
            items: Vec::new(),
            keep_distinct,
        }
    }

    fn build(intervals: impl IntoIterator<Item = (T, T, Attendances)>, keep_distinct: bool) -> Self {
        let mut input: Vec<(T, T, Attendances)> = intervals.into_iter().collect();
        if keep_distinct {
            // `sorted(intervals)` before the boundaries are taken: with
            // the flags no longer breaking ties, the input's own order
            // is what decides which of two boundaries at the same
            // instant comes first
            input.sort_by(|a, b| order(&a.0, &b.0).then_with(|| order(&a.1, &b.1)));
        }
        let mut boundaries: Vec<(T, Flag, Attendances)> = Vec::with_capacity(input.len() * 2);
        for (start, stop, payload) in input {
            // a zero-length interval is not a boundary pair, it is
            // nothing at all
            if start < stop {
                boundaries.push((start, Flag::Start, payload.clone()));
                boundaries.push((stop, Flag::Stop, payload));
            }
        }
        sort_boundaries(&mut boundaries, keep_distinct);

        let mut items = Vec::new();
        let mut starts: Vec<T> = Vec::new();
        let mut gathered: Option<Attendances> = None;
        for (value, flag, payload) in boundaries {
            if flag == Flag::Start {
                starts.push(value);
                gathered = Some(match gathered {
                    None => payload,
                    Some(current) => current.union(&payload),
                });
            } else {
                // an unbalanced boundary cannot happen: every start was
                // pushed with its stop
                let start = starts.pop().expect("a stop always follows its start");
                if starts.is_empty() {
                    items.push((start, value, gathered.take().unwrap_or_default()));
                }
            }
        }
        Intervals {
            items,
            keep_distinct,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &(T, T, Attendances)> {
        self.items.iter()
    }

    /// The union, Odoo's `|`.
    pub fn union(&self, other: &Intervals<T>) -> Intervals<T> {
        let both = self.items.iter().chain(other.items.iter()).cloned();
        Self::build(both, self.keep_distinct)
    }

    /// The intersection, Odoo's `&`.
    pub fn intersect(&self, other: &Intervals<T>) -> Intervals<T> {
        self.merge(other, false)
    }

    /// The difference, Odoo's `-` — working time minus time off.
    pub fn difference(&self, other: &Intervals<T>) -> Intervals<T> {
        self.merge(other, true)
    }

    /// Odoo's `_merge`: one sweep over both sets' boundaries, where the
    /// other set's ends are neither opening nor closing but a switch of
    /// whether the current stretch counts.
    fn merge(&self, other: &Intervals<T>, difference: bool) -> Intervals<T> {
        let mut bounds: Vec<(T, Flag, Attendances)> = Vec::new();
        for (start, stop, payload) in &self.items {
            if start < stop {
                bounds.push((*start, Flag::Start, payload.clone()));
                bounds.push((*stop, Flag::Stop, payload.clone()));
            }
        }
        let normalized = Self::build(other.items.iter().cloned(), self.keep_distinct);
        for (start, stop, _) in &normalized.items {
            if start < stop {
                bounds.push((*start, Flag::Switch, Attendances::none()));
                bounds.push((*stop, Flag::Switch, Attendances::none()));
            }
        }
        sort_boundaries(&mut bounds, self.keep_distinct);

        let mut items = Vec::new();
        let mut start: Option<T> = None;
        let mut payload = Attendances::none();
        let mut enabled = difference;
        for (value, flag, recs) in bounds {
            match flag {
                Flag::Start => {
                    start = Some(value);
                    payload = recs;
                }
                Flag::Stop => {
                    if let Some(open) = start {
                        if enabled && open < value {
                            items.push((open, value, payload.clone()));
                        }
                    }
                    start = None;
                }
                Flag::Switch => {
                    if !enabled && start.is_some() {
                        start = Some(value);
                    }
                    if enabled {
                        if let Some(open) = start {
                            if open < value {
                                items.push((open, value, payload.clone()));
                            }
                        }
                    }
                    enabled = !enabled;
                }
            }
        }
        Intervals {
            items,
            keep_distinct: self.keep_distinct,
        }
    }
}

/// Boundaries in the order the sweep needs them.
///
/// Without `keep_distinct` the flag breaks ties, so at one instant a
/// start is seen before a stop and two touching intervals never close.
/// With it, only the value orders — and a stable sort leaves the input's
/// own order to decide, which is what keeps them apart.
fn sort_boundaries<T: PartialOrd>(boundaries: &mut [(T, Flag, Attendances)], keep_distinct: bool) {
    if keep_distinct {
        boundaries.sort_by(|a, b| order(&a.0, &b.0));
    } else {
        boundaries.sort_by(|a, b| order(&a.0, &b.0).then_with(|| a.1.cmp(&b.1)));
    }
}

/// Do two intervals share more than an endpoint? Odoo's
/// `intervals_overlap`.
pub fn intervals_overlap<T: PartialOrd>(a: (T, T), b: (T, T)) -> bool {
    a.0 < b.1 && a.1 > b.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(start: i64, stop: i64) -> (i64, i64, Attendances) {
        (start, stop, Attendances::none())
    }

    fn tagged(start: i64, stop: i64, id: i64) -> (i64, i64, Attendances) {
        (
            start,
            stop,
            Attendances::one(AttendanceRef::new(id, (stop - start) as f64, 1.0)),
        )
    }

    fn bounds<T: Copy + PartialOrd>(intervals: &Intervals<T>) -> Vec<(T, T)> {
        intervals.iter().map(|(a, b, _)| (*a, *b)).collect()
    }

    #[test]
    fn touching_intervals_become_one() {
        let merged = Intervals::new([plain(1, 3), plain(3, 5)]);
        assert_eq!(bounds(&merged), vec![(1, 5)]);
    }

    #[test]
    fn keeping_them_distinct_keeps_the_two_attendances_apart() {
        // 8–12 and 12–13 are a morning and a break: adding their hours up
        // is right, and merging them would lose that the second one is
        // not worked
        let kept = Intervals::distinct([tagged(8, 12, 1), tagged(12, 13, 2)]);
        assert_eq!(bounds(&kept), vec![(8, 12), (12, 13)]);
        assert_eq!(kept.iter().next().unwrap().2.len(), 1);
    }

    #[test]
    fn overlapping_intervals_are_normalized_into_one_carrying_both() {
        let merged = Intervals::new([tagged(1, 6, 1), tagged(4, 9, 2)]);
        assert_eq!(bounds(&merged), vec![(1, 9)]);
        let payload = &merged.iter().next().unwrap().2;
        assert_eq!(payload.len(), 2, "the interval remembers both rows");
    }

    #[test]
    fn the_same_row_twice_is_one_row() {
        let merged = Intervals::new([tagged(1, 6, 7), tagged(4, 9, 7)]);
        assert_eq!(merged.iter().next().unwrap().2.len(), 1);
    }

    #[test]
    fn a_zero_length_interval_is_dropped() {
        let merged = Intervals::new([plain(3, 3), plain(5, 8)]);
        assert_eq!(bounds(&merged), vec![(5, 8)]);
    }

    #[test]
    fn out_of_order_input_comes_back_ordered() {
        let merged = Intervals::new([plain(10, 12), plain(1, 3), plain(5, 6)]);
        assert_eq!(bounds(&merged), vec![(1, 3), (5, 6), (10, 12)]);
    }

    #[test]
    fn a_leave_in_the_middle_of_the_day_splits_the_work() {
        let work = Intervals::new([plain(8, 17)]);
        let leave = Intervals::new([plain(12, 13)]);
        assert_eq!(bounds(&work.difference(&leave)), vec![(8, 12), (13, 17)]);
    }

    #[test]
    fn a_leave_covering_the_whole_day_leaves_nothing() {
        let work = Intervals::new([plain(8, 17)]);
        let leave = Intervals::new([plain(0, 24)]);
        assert!(work.difference(&leave).is_empty());
    }

    #[test]
    fn a_leave_outside_the_working_hours_takes_nothing_away() {
        let work = Intervals::new([plain(8, 12), plain(13, 17)]);
        let leave = Intervals::new([plain(18, 20)]);
        assert_eq!(bounds(&work.difference(&leave)), vec![(8, 12), (13, 17)]);
    }

    #[test]
    fn the_difference_keeps_the_attendance_the_remaining_time_came_from() {
        let work = Intervals::new([tagged(8, 17, 4)]);
        let leave = Intervals::new([plain(12, 13)]);
        let left = work.difference(&leave);
        for (_, _, payload) in left.iter() {
            assert_eq!(payload.iter().next().unwrap().id, Some(4));
        }
    }

    #[test]
    fn the_intersection_is_the_time_both_sets_cover() {
        let work = Intervals::new([plain(8, 12), plain(13, 17)]);
        let window = Intervals::new([plain(10, 14)]);
        assert_eq!(bounds(&work.intersect(&window)), vec![(10, 12), (13, 14)]);
    }

    #[test]
    fn the_union_merges_what_overlaps_and_keeps_what_does_not() {
        let morning = Intervals::new([plain(8, 12)]);
        let rest = Intervals::new([plain(11, 13), plain(20, 22)]);
        assert_eq!(bounds(&morning.union(&rest)), vec![(8, 13), (20, 22)]);
    }

    #[test]
    fn overlap_is_about_more_than_a_shared_endpoint() {
        assert!(intervals_overlap((1, 5), (4, 8)));
        assert!(!intervals_overlap((1, 5), (5, 8)), "touching is not overlapping");
        assert!(!intervals_overlap((1, 5), (6, 8)));
    }

    #[test]
    fn durations_add_up_across_a_merged_payload() {
        let merged = Intervals::new([tagged(1, 5, 1), tagged(3, 9, 2)]);
        let payload = &merged.iter().next().unwrap().2;
        assert_eq!(payload.total_hours(), 4.0 + 6.0);
        assert_eq!(payload.total_days(), 2.0);
    }
}

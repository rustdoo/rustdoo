//! Port of `odoo/addons/rating/models/rating_data.py`: what a number
//! between 0 and 5 *means*.
//!
//! Everything here is pure arithmetic over a rating value, and it is the
//! part of the addon every other module reads: "happy" is not an opinion,
//! it is `rating >= 4`. Keeping the thresholds in one place — and under
//! test — is what stops two screens disagreeing about the same customer.

use std::cmp::Ordering;

/// The averages that separate a happy record from an unhappy one
/// (`RATING_AVG_*`). They are not the same cuts as the individual
/// thresholds: an average of 3.66 is "happy" while a single rating of 3.66
/// is only "neutral", because one number summarises many opinions and the
/// other is one opinion.
pub const RATING_AVG_TOP: f64 = 3.66;
pub const RATING_AVG_OK: f64 = 2.33;
pub const RATING_AVG_MIN: f64 = 1.0;

/// The cuts a single rating falls into (`RATING_LIMIT_*`).
pub const RATING_LIMIT_SATISFIED: f64 = 4.0;
pub const RATING_LIMIT_OK: f64 = 3.0;
pub const RATING_LIMIT_MIN: f64 = 1.0;

/// The three faces a customer is offered, plus "not rated"
/// (`RATING_*_VALUE`).
pub const RATING_HAPPY_VALUE: f64 = 5.0;
pub const RATING_NEUTRAL_VALUE: f64 = 3.0;
pub const RATING_UNHAPPY_VALUE: f64 = 1.0;
pub const RATING_NONE_VALUE: f64 = 0.0;

/// `RATING_TEXT` — the selection stored in `rating_text`.
pub const RATING_TEXT: [(&str, &str); 4] = [
    ("top", "Happy"),
    ("ok", "Neutral"),
    ("ko", "Unhappy"),
    ("none", "Not Rated yet"),
];

/// The grades `rating_get_grades` counts by.
pub const GRADE_GREAT: &str = "great";
pub const GRADE_OKAY: &str = "okay";
pub const GRADE_BAD: &str = "bad";

/// How many decimals the averages are compared on, Odoo's
/// `float_compare(..., 2)`.
const AVG_PRECISION: i32 = 2;

/// Round half away from zero, port of
/// `odoo.tools.float_utils.float_round` in its HALF-UP form.
///
/// The epsilon is not decoration and it is the whole difference from a
/// plain `(value * factor).round() / factor`. A decimal that looks like
/// a tie usually is not one: IEEE-754 stores `1.005` a hair *below* the
/// real value, so scaling and rounding gives 1.00 where a person — and
/// Odoo — reads 1.01. Odoo nudges the scaled number by
/// `2^(log2(|v|) - 50)` before rounding, which is far below the
/// magnitude of any real difference and just above the representation
/// error. Without it the port disagreed with Odoo on exactly the values
/// a user is most likely to notice.
fn float_round(value: f64, digits: i32) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let factor = 10f64.powi(digits);
    let scaled = value * factor;
    let epsilon = 2f64.powf(scaled.abs().log2() - 50.0);
    (scaled + epsilon.copysign(scaled)).round() / factor
}

/// Port of `float_compare(value1, value2, precision_digits)`: both sides
/// are rounded first, and a difference smaller than half a step is no
/// difference at all.
///
/// Comparing the raw floats instead would put an average that prints as
/// `3.66` on the wrong side of `RATING_AVG_TOP` whenever the sum of the
/// ratings lands a few ULPs short — a record that reads "3.66" on screen
/// and is filed as neutral.
fn float_compare(value1: f64, value2: f64, digits: i32) -> Ordering {
    let step = 10f64.powi(-digits);
    let delta = float_round(value1, digits) - float_round(value2, digits);
    if delta.abs() < step / 2.0 {
        return Ordering::Equal;
    }
    if delta < 0.0 {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// `_rating_assert_value` — Odoo asserts, this answers.
///
/// The assertion is kept as a question because the port's caller is the
/// RPC layer: a client sending 7 must read a sentence, not meet a panic.
/// The database says the same thing a second time through the model's
/// `CHECK`, for the writers that never come through here.
pub fn is_rating_value(value: f64) -> bool {
    (RATING_NONE_VALUE..=RATING_HAPPY_VALUE).contains(&value)
}

/// `_rating_avg_to_text` — what an *average* of ratings is called.
pub fn rating_avg_to_text(rating_avg: f64) -> &'static str {
    if float_compare(rating_avg, RATING_AVG_TOP, AVG_PRECISION) != Ordering::Less {
        return "top";
    }
    if float_compare(rating_avg, RATING_AVG_OK, AVG_PRECISION) != Ordering::Less {
        return "ok";
    }
    if float_compare(rating_avg, RATING_AVG_MIN, AVG_PRECISION) != Ordering::Less {
        return "ko";
    }
    "none"
}

/// `_rating_to_text` — what a single rating is called.
///
/// Deviation from Odoo: no assertion on the range. A value outside 0..5
/// cannot reach here through the ORM (the model carries Odoo's own
/// `CHECK`), and a compute in this framework has no way to fail — so a
/// hypothetical 7 is read as the happiest thing it could be rather than
/// bringing the read down.
pub fn rating_to_text(value: f64) -> &'static str {
    if value >= RATING_LIMIT_SATISFIED {
        return "top";
    }
    if value >= RATING_LIMIT_OK {
        return "ok";
    }
    if value >= RATING_LIMIT_MIN {
        return "ko";
    }
    "none"
}

/// `_rating_to_grade` — the three buckets the statistics count in.
pub fn rating_to_grade(value: f64) -> &'static str {
    if value >= RATING_LIMIT_SATISFIED {
        return GRADE_GREAT;
    }
    if value >= RATING_LIMIT_OK {
        return GRADE_OKAY;
    }
    GRADE_BAD
}

/// `_rating_to_threshold` — the 0/1/3/5 the images are named after.
pub fn rating_to_threshold(value: f64) -> i64 {
    if value >= RATING_LIMIT_SATISFIED {
        return RATING_HAPPY_VALUE as i64;
    }
    if value >= RATING_LIMIT_OK {
        return RATING_NEUTRAL_VALUE as i64;
    }
    if value >= RATING_LIMIT_MIN {
        return RATING_UNHAPPY_VALUE as i64;
    }
    RATING_NONE_VALUE as i64
}

/// `_get_rating_image_filename` — `rating_5.png` and friends.
pub fn rating_image_filename(value: f64) -> String {
    format!("rating_{}.png", rating_to_threshold(value))
}

/// The URL the client draws the face from, as Odoo builds it in
/// `_compute_rating_image`.
pub fn rating_image_url(value: f64) -> String {
    format!("/rating/static/src/img/{}", rating_image_filename(value))
}

/// What a set of ratings adds up to, port of `_rating_get_repartition`
/// with `add_stats=True` plus the two figures `rating.mixin` exposes as
/// fields (`rating_avg_text`, `rating_percentage_satisfaction`).
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    /// how many ratings were counted
    pub total: i64,
    pub avg: f64,
    /// `rating value -> how many`, always carrying the five whole values
    pub repartition: Vec<(f64, i64)>,
    pub great: i64,
    pub okay: i64,
    pub bad: i64,
}

impl Stats {
    /// `rating_get_stats`'s `percent`: what share of the ratings each
    /// value took, as a percentage.
    pub fn percent(&self) -> Vec<(f64, f64)> {
        self.repartition
            .iter()
            .map(|(value, count)| {
                let share = if self.total > 0 {
                    (*count as f64) * 100.0 / (self.total as f64)
                } else {
                    0.0
                };
                (*value, share)
            })
            .collect()
    }

    /// `rating_percentage_satisfaction`: the share of happy ratings.
    ///
    /// `-1` when there is nothing to be satisfied about — Odoo's own
    /// sentinel, and the reason a record with no rating draws no bar
    /// instead of drawing an empty one.
    pub fn percentage_satisfaction(&self) -> f64 {
        let counted = self.great + self.okay + self.bad;
        if counted == 0 {
            return -1.0;
        }
        (self.great as f64) * 100.0 / (counted as f64)
    }

    /// `rating_avg_text`.
    pub fn avg_text(&self) -> &'static str {
        rating_avg_to_text(self.avg)
    }
}

/// Fold `(rating value, how many)` pairs — what a grouped read gives —
/// into the statistics every screen of this addon shows.
///
/// The five whole values are always present, at zero when nobody chose
/// them: a bar chart missing its empty bars is a chart that lies about
/// the shape of the answers.
pub fn stats_of(pairs: &[(f64, i64)]) -> Stats {
    let mut repartition: Vec<(f64, i64)> = (1..=5).map(|value| (f64::from(value), 0)).collect();
    let (mut total, mut weighted) = (0i64, 0f64);
    let (mut great, mut okay, mut bad) = (0i64, 0i64, 0i64);
    for (value, count) in pairs {
        // Odoo rounds the grouped value to one decimal before using it as
        // a key: 4.05 and 4.049999 are the same bar
        let value = float_round(*value, 1);
        match repartition.iter_mut().find(|(known, _)| *known == value) {
            Some((_, known_count)) => *known_count += count,
            None => repartition.push((value, *count)),
        }
        total += count;
        weighted += value * (*count as f64);
        match rating_to_grade(value) {
            GRADE_GREAT => great += count,
            GRADE_OKAY => okay += count,
            _ => bad += count,
        }
    }
    repartition.sort_by(|(left, _), (right, _)| left.total_cmp(right));
    let avg = if total > 0 {
        weighted / (total as f64)
    } else {
        0.0
    };
    Stats {
        total,
        avg,
        repartition,
        great,
        okay,
        bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_rating_is_named_by_its_cut() {
        // the boundaries themselves, which is where a port goes wrong
        assert_eq!(rating_to_text(5.0), "top");
        assert_eq!(rating_to_text(4.0), "top");
        assert_eq!(rating_to_text(3.99), "ok");
        assert_eq!(rating_to_text(3.0), "ok");
        assert_eq!(rating_to_text(2.99), "ko");
        assert_eq!(rating_to_text(1.0), "ko");
        // below 1 is not an unhappy customer: it is no customer
        assert_eq!(rating_to_text(0.99), "none");
        assert_eq!(rating_to_text(0.0), "none");
    }

    #[test]
    fn an_average_is_named_by_wider_cuts_than_a_single_rating() {
        // 3.66 as an average is happy; as one rating it is only neutral
        assert_eq!(rating_avg_to_text(3.66), "top");
        assert_eq!(rating_to_text(3.66), "ok");
        assert_eq!(rating_avg_to_text(3.65), "ok");
        assert_eq!(rating_avg_to_text(2.33), "ok");
        assert_eq!(rating_avg_to_text(2.32), "ko");
        assert_eq!(rating_avg_to_text(1.0), "ko");
        assert_eq!(rating_avg_to_text(0.99), "none");
    }

    #[test]
    fn an_average_is_compared_on_two_decimals_like_odoo() {
        // three ratings of 3, 3.97 and 4 average to 3.6566...: rounded to
        // two decimals that is 3.66, and the record is happy
        let avg = (3.0 + 3.97 + 4.0) / 3.0;
        assert!(avg < RATING_AVG_TOP, "the raw average is below the cut");
        assert_eq!(rating_avg_to_text(avg), "top");
        // and a hair below still rounds down, instead of drifting up
        assert_eq!(rating_avg_to_text(3.654), "ok");
    }

    #[test]
    fn a_grade_is_one_of_three_and_never_a_fourth() {
        assert_eq!(rating_to_grade(5.0), GRADE_GREAT);
        assert_eq!(rating_to_grade(4.0), GRADE_GREAT);
        assert_eq!(rating_to_grade(3.0), GRADE_OKAY);
        assert_eq!(rating_to_grade(1.0), GRADE_BAD);
        // Odoo has no grade for "not rated": zero is a bad grade
        assert_eq!(rating_to_grade(0.0), GRADE_BAD);
    }

    #[test]
    fn the_face_is_named_after_the_threshold_it_falls_in() {
        assert_eq!(rating_to_threshold(4.5), 5);
        assert_eq!(rating_to_threshold(3.5), 3);
        assert_eq!(rating_to_threshold(1.5), 1);
        assert_eq!(rating_to_threshold(0.5), 0);
        assert_eq!(rating_image_filename(5.0), "rating_5.png");
        assert_eq!(rating_image_url(2.0), "/rating/static/src/img/rating_1.png");
    }

    #[test]
    fn what_is_out_of_range_is_refused_before_it_is_stored() {
        assert!(is_rating_value(0.0));
        assert!(is_rating_value(5.0));
        assert!(!is_rating_value(-0.5));
        assert!(!is_rating_value(5.5));
    }

    #[test]
    fn the_statistics_keep_the_bars_nobody_chose() {
        let stats = stats_of(&[(5.0, 3), (1.0, 1)]);
        assert_eq!(stats.total, 4);
        assert_eq!(stats.avg, 4.0);
        assert_eq!(stats.avg_text(), "top");
        assert_eq!(stats.great, 3);
        assert_eq!(stats.okay, 0);
        assert_eq!(stats.bad, 1);
        // five bars, in order, including the three nobody chose
        assert_eq!(
            stats.repartition,
            vec![(1.0, 1), (2.0, 0), (3.0, 0), (4.0, 0), (5.0, 3)]
        );
        assert_eq!(stats.percentage_satisfaction(), 75.0);
        let percent = stats.percent();
        assert_eq!(percent[0], (1.0, 25.0));
        assert_eq!(percent[4], (5.0, 75.0));
    }

    #[test]
    fn a_record_nobody_rated_is_not_a_record_with_zero_satisfaction() {
        let stats = stats_of(&[]);
        assert_eq!(stats.total, 0);
        assert_eq!(stats.avg, 0.0);
        // -1, not 0: nothing to show is not the same as everybody unhappy
        assert_eq!(stats.percentage_satisfaction(), -1.0);
        assert!(stats.percent().iter().all(|(_, share)| *share == 0.0));
    }

    #[test]
    fn a_rating_between_two_bars_gets_a_bar_of_its_own() {
        // half a star is not what the faces send, but an imported rating
        // can carry one — and it must not vanish from the repartition
        let stats = stats_of(&[(4.5, 2), (5.0, 2)]);
        assert_eq!(stats.total, 4);
        assert_eq!(stats.great, 4);
        assert!(stats.repartition.contains(&(4.5, 2)), "{stats:?}");
        assert_eq!(stats.avg, 4.75);
    }
}

#[cfg(test)]
mod rounding_tests {
    use super::float_round;

    /// The values Odoo answers, taken from running its own
    /// `float_round` against the vendored source. Every one of these is
    /// a case where dropping the epsilon changes the answer a user sees.
    #[test]
    fn a_tie_that_is_not_really_a_tie_rounds_the_way_odoo_rounds_it() {
        for (value, digits, expected) in [
            (1.005_f64, 2, 1.01_f64),
            (2.675, 2, 2.68),
            (2.345, 2, 2.35),
            (-1.005, 2, -1.01),
            (0.5, 0, 1.0),
            (1.5, 0, 2.0),
            // and something below the tie still rounds down: the
            // epsilon is small enough not to move a real difference
            (1.0049, 2, 1.00),
        ] {
            assert_eq!(
                float_round(value, digits),
                expected,
                "float_round({value}, {digits})"
            );
        }
    }

    #[test]
    fn zero_and_the_unrepresentable_come_back_untouched() {
        assert_eq!(float_round(0.0, 2), 0.0);
        assert!(float_round(f64::NAN, 2).is_nan());
        assert_eq!(float_round(f64::INFINITY, 2), f64::INFINITY);
    }
}

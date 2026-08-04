//! The amount a check is written for, in words.
//!
//! Odoo gets this from `res.currency.amount_to_text`, which calls
//! `num2words` and appends the currency's unit and subunit labels
//! ("Dollars", "Cents"). This port has no `res.currency` at all, so
//! there is nowhere for those labels to come from — and inventing a
//! currency model inside a check-printing addon would be worse than
//! saying so.
//!
//! What is written instead is the form a check actually carries: the
//! integer part spelled out, then the hundredths as a fraction, which is
//! how the legal amount line reads on a printed cheque. The spelling
//! itself is `num2words(..., lang='en').title()`, exactly what Odoo
//! produces for the words half.

/// 0..19, the numbers English does not build out of anything smaller.
const SMALL: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const TENS: [&str; 10] = [
    "", "ten", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// The groups of three digits English names, largest first.
const SCALES: [(u64, &str); 5] = [
    (1_000_000_000_000_000, "quadrillion"),
    (1_000_000_000_000, "trillion"),
    (1_000_000_000, "billion"),
    (1_000_000, "million"),
    (1_000, "thousand"),
];

/// 0..99 — `num2words` hyphenates the compound tens ("twenty-three").
fn under_hundred(number: u64) -> String {
    if number < 20 {
        return SMALL[number as usize].to_string();
    }
    let tens = TENS[(number / 10) as usize];
    match number % 10 {
        0 => tens.to_string(),
        unit => format!("{tens}-{}", SMALL[unit as usize]),
    }
}

/// 0..999. The "and" is `num2words`' English one: "one hundred and two",
/// not the American "one hundred two".
fn under_thousand(number: u64) -> String {
    if number < 100 {
        return under_hundred(number);
    }
    let hundreds = SMALL[(number / 100) as usize];
    match number % 100 {
        0 => format!("{hundreds} hundred"),
        rest => format!("{hundreds} hundred and {}", under_hundred(rest)),
    }
}

/// A whole number in English words, lowercase — the `num2words` half of
/// `amount_to_text`.
///
/// Beyond a quadrillion there is no scale name left, and a check for that
/// amount is not a case worth guessing at: the digits come back as
/// themselves rather than as a wrong word.
pub fn to_words(number: u64) -> String {
    if number == 0 {
        return SMALL[0].to_string();
    }
    if number >= 1_000_000_000_000_000_000 {
        return number.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut rest = number;
    for (scale, name) in SCALES {
        let group = rest / scale;
        if group > 0 {
            parts.push(format!("{} {name}", under_thousand(group)));
            rest %= scale;
        }
    }
    if rest > 0 {
        // "one million and twenty-three", but "one thousand two hundred
        // and thirty-four": the leading "and" only appears when the last
        // group is smaller than a hundred and something came before it
        if !parts.is_empty() && rest < 100 {
            parts.push(format!("and {}", under_hundred(rest)));
        } else {
            parts.push(under_thousand(rest));
        }
    }
    parts.join(" ")
}

/// Python's `str.title()`: a capital after anything that is not a letter,
/// which is why "twenty-three" becomes "Twenty-Three" and not
/// "Twenty-three".
pub fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start_of_word = true;
    for ch in text.chars() {
        if ch.is_alphabetic() {
            if start_of_word {
                out.extend(ch.to_uppercase());
            } else {
                out.extend(ch.to_lowercase());
            }
            start_of_word = false;
        } else {
            out.push(ch);
            start_of_word = true;
        }
    }
    out
}

/// The legal amount line of a check: `"One Hundred And Twenty-Three and
/// 45/100"`.
///
/// The cents are a fraction rather than words because that is what the
/// line means — a bank reads `45/100` and cannot misread it, which is the
/// whole reason the amount is written twice on a check.
pub fn amount_in_words(amount: f64) -> String {
    // rounded to the cent first: the words must agree with the figures,
    // and 0.145 printed as "0.15" must not be spelled "fourteen"
    let cents_total = (amount.abs() * 100.0).round() as u64;
    let whole = cents_total / 100;
    let cents = cents_total % 100;
    let words = format!("{} and {cents:02}/100", title_case(&to_words(whole)));
    if amount < 0.0 {
        // a check is never written for a negative amount; a payment that
        // somehow is says so instead of printing its absolute value
        return format!("Minus {words}");
    }
    words
}

/// Port of `_check_fill_line`: the amount line padded with stars, so that
/// nobody can add a word after it.
pub fn fill_line(amount_str: &str) -> String {
    if amount_str.is_empty() {
        return String::new();
    }
    let mut line = format!("{amount_str} ");
    while line.chars().count() < 200 {
        line.push('*');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_small_numbers_are_spelled_out() {
        assert_eq!(to_words(0), "zero");
        assert_eq!(to_words(7), "seven");
        assert_eq!(to_words(13), "thirteen");
        assert_eq!(to_words(20), "twenty");
        assert_eq!(to_words(23), "twenty-three");
        assert_eq!(to_words(99), "ninety-nine");
    }

    #[test]
    fn the_hundreds_carry_the_english_and() {
        assert_eq!(to_words(100), "one hundred");
        assert_eq!(to_words(123), "one hundred and twenty-three");
        assert_eq!(to_words(905), "nine hundred and five");
    }

    #[test]
    fn the_scales_are_named_largest_first() {
        assert_eq!(to_words(1_000), "one thousand");
        assert_eq!(to_words(1_234), "one thousand two hundred and thirty-four");
        // the leading "and" belongs to the last group only when it is
        // smaller than a hundred
        assert_eq!(to_words(1_000_023), "one million and twenty-three");
        assert_eq!(to_words(1_000_100), "one million one hundred");
        assert_eq!(
            to_words(2_147_483_647),
            "two billion one hundred and forty-seven million four hundred and eighty-three \
             thousand six hundred and forty-seven"
        );
    }

    #[test]
    fn a_number_too_big_to_name_comes_back_as_digits() {
        // no scale name past a quadrillion: better the digits than a word
        // that means something else
        let huge = 1_000_000_000_000_000_000u64;
        assert_eq!(to_words(huge), huge.to_string());
    }

    #[test]
    fn the_title_case_is_pythons() {
        assert_eq!(title_case("twenty-three"), "Twenty-Three");
        assert_eq!(
            title_case("one hundred and two"),
            "One Hundred And Two"
        );
        // and it lowercases what it did not capitalize, like str.title()
        assert_eq!(title_case("ONE HUNDRED"), "One Hundred");
    }

    #[test]
    fn the_legal_amount_line_carries_the_cents_as_a_fraction() {
        assert_eq!(
            amount_in_words(123.45),
            "One Hundred And Twenty-Three and 45/100"
        );
        assert_eq!(amount_in_words(0.0), "Zero and 00/100");
        assert_eq!(amount_in_words(1000.0), "One Thousand and 00/100");
        // the rounding happens before the words, so the two halves agree
        assert_eq!(amount_in_words(9.999), "Ten and 00/100");
    }

    #[test]
    fn a_negative_amount_says_so_instead_of_hiding_its_sign() {
        assert_eq!(amount_in_words(-5.0), "Minus Five and 00/100");
    }

    #[test]
    fn the_amount_line_is_padded_so_nothing_can_be_added_to_it() {
        let line = fill_line("One and 00/100");
        assert_eq!(line.chars().count(), 200);
        assert!(line.starts_with("One and 00/100 *"));
        assert!(line.ends_with('*'));
        // an empty amount stays empty: a line of 200 stars means nothing
        assert_eq!(fill_line(""), "");
    }
}

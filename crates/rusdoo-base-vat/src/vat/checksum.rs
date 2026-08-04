//! The check digits several countries share.
//!
//! Port of `stdnum.luhn`, `stdnum.iso7064.mod_11_10` and
//! `stdnum.iso7064.mod_97_10`. A VAT number's last digit is almost always
//! a checksum over the rest: it is what makes a typo in the middle of a
//! number visible instead of silently addressing a different company.

use super::text::{digits, is_digits};

/// Luhn checksum over a digits-only number. Valid numbers sum to 0.
pub fn luhn(number: &str) -> Option<u32> {
    let values = digits(number)?;
    let mut total = 0;
    // the doubling starts on the second digit from the right
    for (position, value) in values.iter().rev().enumerate() {
        if position % 2 == 0 {
            total += value;
        } else {
            let doubled = value * 2;
            total += doubled / 10 + doubled % 10;
        }
    }
    Some(total % 10)
}

pub fn luhn_is_valid(number: &str) -> bool {
    !number.is_empty() && luhn(number) == Some(0)
}

/// The digit that makes `number` pass Luhn, `stdnum.luhn.calc_check_digit`.
pub fn luhn_check_digit(number: &str) -> Option<char> {
    let checksum = luhn(&format!("{number}0"))?;
    char::from_digit((10 - checksum) % 10, 10)
}

/// ISO 7064 MOD 11,10. A valid number has a checksum of 1.
pub fn mod_11_10(number: &str) -> Option<u32> {
    let values = digits(number)?;
    let mut check = 5;
    for value in values {
        check = ((if check == 0 { 10 } else { check } * 2) % 11 + value) % 10;
    }
    Some(check)
}

pub fn mod_11_10_is_valid(number: &str) -> bool {
    mod_11_10(number) == Some(1)
}

/// ISO 7064 MOD 97,10 over a base-36 string. A valid number leaves 1.
///
/// The number is first spelled out in decimal — `'B'` becomes `11` — and
/// the result can be far past `u64`, so the remainder is carried digit by
/// digit instead of building the integer.
pub fn mod_97_10_is_valid(number: &str) -> bool {
    let mut remainder: u64 = 0;
    for c in number.chars() {
        let Some(value) = c.to_digit(36) else {
            return false;
        };
        for decimal in value.to_string().chars() {
            remainder = (remainder * 10 + decimal.to_digit(10).expect("decimal digit") as u64) % 97;
        }
    }
    remainder == 1
}

/// `sum(w * int(n) for w, n in zip(weights, number))`, the shape most of
/// the national checksums have. Pairs stop at the shorter of the two,
/// like Python's `zip`.
pub fn weighted(number: &str, weights: &[i64]) -> Option<i64> {
    let values = digits(number)?;
    Some(
        values
            .iter()
            .zip(weights)
            .map(|(value, weight)| weight * i64::from(*value))
            .sum(),
    )
}

/// The same, pairing the weights against the number read right to left.
pub fn weighted_reversed(number: &str, weights: &[i64]) -> Option<i64> {
    let mut values = digits(number)?;
    values.reverse();
    Some(
        values
            .iter()
            .zip(weights)
            .map(|(value, weight)| weight * i64::from(*value))
            .sum(),
    )
}

/// Does `number` end in `expected`, written as a digit?
///
/// The comparison is on the character rather than on a parsed value so
/// that a check digit of 10 or 11 — which several algorithms can produce
/// and no single position can hold — simply fails to match, exactly as
/// comparing `str(check)` to a one-character string does in Python.
pub fn ends_with_digit(number: &str, expected: i64) -> bool {
    if !is_digits(number) {
        return false;
    }
    number.chars().next_back().map(String::from) == Some(expected.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_a_number_whose_last_digit_was_computed_for_it() {
        // the SIREN inside FR23334175221
        assert!(luhn_is_valid("334175221"));
        assert!(!luhn_is_valid("334175222"));
        assert_eq!(luhn_check_digit("33417522"), Some('1'));
        assert!(!luhn_is_valid(""), "an empty number checks nothing");
    }

    #[test]
    fn mod_11_10_accepts_the_german_vat_number() {
        assert!(mod_11_10_is_valid("123456788"));
        assert!(!mod_11_10_is_valid("136695978"));
    }

    #[test]
    fn mod_97_10_reads_letters_as_base_36() {
        // `'B'` is read as 11 and the string is spelled out in decimal
        // before the remainder is taken, so a letter costs two digits
        assert!(mod_97_10_is_valid("B1234567835"));
        assert!(!mod_97_10_is_valid("B1234567836"));
        // the Dutch reference number does NOT pass this check — it is
        // valid through the other half of `stdnum.nl.btw`, the BSN one,
        // and asserting otherwise here was what hid that
        assert!(!mod_97_10_is_valid("NL123456782B90"));
        // anything outside base 36 is not a number to check
        assert!(!mod_97_10_is_valid("B123456783!"));
    }

    #[test]
    fn a_check_digit_that_cannot_be_written_never_matches() {
        assert!(ends_with_digit("129", 9));
        assert!(!ends_with_digit("129", 10));
        assert!(!ends_with_digit("12A", 1));
    }
}

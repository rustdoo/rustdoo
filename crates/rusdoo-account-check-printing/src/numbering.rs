//! The numbers on the checks: what a valid one looks like, how the next
//! one is derived from the last, and the ceiling the journal's sequence
//! cannot go past.

use rusdoo_core::RusdooError;

/// `MAX_INT32` in `account_journal.py`: the sequence's `number_next` is
/// an `int4` column, and a number above this is refused with a sentence
/// rather than by PostgreSQL with a stack trace.
pub const MAX_INT32: i64 = 2_147_483_647;

/// Port of `_constrains_check_number` and of the journal's
/// `^[0-9]+$`: a check number is digits and nothing else.
///
/// Not `char::is_numeric`: that is true of the Devanagari digits and of
/// superscript two, none of which a bank reads. Odoo's `isdecimal()` is
/// wider than ASCII too, but a check number that is not ASCII is a
/// mistake in every case anyone will meet.
pub fn is_check_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The number as it is printed: zero-padded to `padding`, never
/// truncated — a number wider than its padding keeps every digit.
pub fn padded(number: i64, padding: i64) -> String {
    if padding <= 0 {
        return number.to_string();
    }
    format!("{number:0width$}", width = padding as usize)
}

/// Port of `print_checks`' "number of the first pre-printed check":
/// the last number used in the journal, plus one, kept at the same width.
///
/// No previous check means `"1"` — Odoo arrives at the same place through
/// `int(False) + 1` with a zero-width format.
pub fn next_after(last: Option<&str>) -> String {
    let Some(last) = last.filter(|text| is_check_number(text)) else {
        return "1".to_string();
    };
    let width = last.len();
    // the parse cannot overflow for anything a printer produced, but a
    // number that does not fit an i64 falls back to starting over rather
    // than panicking
    match last.parse::<i64>() {
        Ok(number) => padded(number + 1, width as i64),
        Err(_) => "1".to_string(),
    }
}

/// The numeric value a check number stands for, for comparing `'0042'`
/// with `'42'` — which Odoo does by casting to `BIGINT` in its
/// uniqueness query.
pub fn numeric(text: &str) -> Option<i64> {
    if !is_check_number(text) {
        return None;
    }
    text.parse().ok()
}

/// Port of `_inverse_check_next_number`: what the journal accepts as the
/// next number to print.
///
/// `current` is where the sequence stands now. Going backwards is
/// refused for the reason Odoo gives — a number the bank has already seen
/// comes back rejected, and the person setting it would have no way of
/// knowing why.
pub fn accept_next_number(entered: &str, current: i64) -> Result<i64, RusdooError> {
    if !is_check_number(entered) {
        return Err(RusdooError::Validation(
            "Next Check Number should only contains numbers.".into(),
        ));
    }
    let Some(next) = entered.parse::<i64>().ok().filter(|n| *n <= MAX_INT32) else {
        return Err(RusdooError::Validation(format!(
            "The check number you entered ({entered}) exceeds the maximum allowed value of \
             {MAX_INT32}. Please enter a smaller number."
        )));
    };
    if next < current {
        return Err(RusdooError::Validation(format!(
            "The last check number was {current}. In order to avoid a check being rejected by \
             the bank, you can only use a greater number."
        )));
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_number_is_digits_and_nothing_else() {
        assert!(is_check_number("00042"));
        assert!(is_check_number("2147483648"));
        assert!(!is_check_number("F1234"));
        assert!(!is_check_number("42 "));
        assert!(!is_check_number(""));
        // digits a bank does not read are not digits here
        assert!(!is_check_number("４２"));
    }

    #[test]
    fn a_number_keeps_its_width_and_never_loses_a_digit() {
        assert_eq!(padded(42, 5), "00042");
        assert_eq!(padded(123456, 5), "123456");
        assert_eq!(padded(7, 0), "7");
    }

    #[test]
    fn the_next_check_follows_the_last_one_at_the_same_width() {
        assert_eq!(next_after(Some("00042")), "00043");
        assert_eq!(next_after(Some("9")), "10");
        // the int32 ceiling is the journal sequence's, not the payment's:
        // a pre-printed pad may well go past it
        assert_eq!(next_after(Some("2147483648")), "2147483649");
        // an empty journal starts at one
        assert_eq!(next_after(None), "1");
        assert_eq!(next_after(Some("")), "1");
    }

    #[test]
    fn two_writings_of_the_same_number_compare_equal() {
        assert_eq!(numeric("0042"), numeric("42"));
        assert_eq!(numeric("F1"), None);
    }

    #[test]
    fn the_journal_refuses_a_number_that_is_not_one() {
        let error = accept_next_number("F1234", 1).expect_err("letters are not a number");
        assert!(error.to_string().contains("should only contains numbers"));
    }

    #[test]
    fn the_journal_refuses_a_number_past_the_int32_ceiling() {
        assert_eq!(accept_next_number("2147483647", 1).unwrap(), MAX_INT32);
        let error =
            accept_next_number("2147483648", 1).expect_err("the column does not hold it");
        assert!(error.to_string().contains("exceeds the maximum allowed value"));
    }

    #[test]
    fn the_journal_refuses_to_go_backwards() {
        assert_eq!(accept_next_number("100", 100).unwrap(), 100);
        let error = accept_next_number("99", 100).expect_err("the bank has seen 99 already");
        assert!(error.to_string().contains("The last check number was 100"));
    }
}

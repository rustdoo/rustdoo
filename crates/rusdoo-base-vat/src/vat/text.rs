//! The string surgery every country's rules start with.
//!
//! Port of `stdnum.util.clean` and of the `compact()` each country module
//! opens with. A VAT number is typed by a human: it arrives with the
//! spaces, dots, slashes and dashes of whatever form it was copied from,
//! and every algorithm below expects it without them.
//!
//! Deviation from `stdnum.util.clean`: it also folds the Unicode dashes
//! (en dash, em dash, minus sign) onto ASCII `-` before deleting. That
//! matters for numbers pasted out of a word processor; it is not ported
//! here because nothing else in this port normalizes Unicode yet, and a
//! half-done normalization is worse than an absent one.

/// `clean(number, deletechars).upper().strip()`.
///
/// Uppercasing is unconditional here while `stdnum` does it in most but
/// not all modules — the exceptions are the digits-only ones, where it
/// cannot make a difference.
pub fn clean(number: &str, delete: &str) -> String {
    number
        .chars()
        .filter(|c| !delete.contains(*c))
        .collect::<String>()
        .to_uppercase()
        .trim()
        .to_string()
}

/// `clean(...)` and then drop a leading country prefix, the shape almost
/// every `compact()` has.
pub fn clean_prefixed(number: &str, delete: &str, prefix: &str) -> String {
    let cleaned = clean(number, delete);
    match cleaned.strip_prefix(prefix) {
        Some(rest) => rest.to_string(),
        None => cleaned,
    }
}

/// Left-pad with zeros to `width`, Python's `zfill`.
pub fn zfill(number: &str, width: usize) -> String {
    let missing = width.saturating_sub(number.chars().count());
    format!("{}{number}", "0".repeat(missing))
}

/// The digits of `number`, or `None` if anything else is in it.
///
/// `stdnum.util.isdigits` exists because Python's `str.isdigit` says yes
/// to Arabic-Indic and superscript digits, which `int()` then refuses.
/// Rust's `is_ascii_digit` has the narrow meaning already.
pub fn digits(number: &str) -> Option<Vec<u32>> {
    number
        .chars()
        .map(|c| c.to_digit(10).filter(|_| c.is_ascii_digit()))
        .collect()
}

/// Is every character an ASCII digit? Empty is *not* all digits, like
/// `isdigits('')` in `stdnum`.
pub fn is_digits(number: &str) -> bool {
    !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
}

/// The number as an integer, for the checks written as `int(number) % n`.
/// `None` when it is not digits or does not fit — a VAT number long
/// enough to overflow `u64` is long enough to be wrong.
pub fn as_u64(number: &str) -> Option<u64> {
    if !is_digits(number) {
        return None;
    }
    number.parse().ok()
}

/// The `n`th character, for the many rules phrased as "the third digit
/// must be...". Out of range answers `None` instead of panicking: the
/// input is user text and shorter than expected is the common case.
pub fn at(number: &str, index: usize) -> Option<char> {
    number.chars().nth(index)
}

/// Characters `from..to` (character positions, not bytes).
pub fn slice(number: &str, from: usize, to: usize) -> String {
    number
        .chars()
        .skip(from)
        .take(to.saturating_sub(from))
        .collect()
}

/// The last `n` characters.
pub fn tail(number: &str, n: usize) -> String {
    let len = number.chars().count();
    slice(number, len.saturating_sub(n), len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning_removes_the_separators_a_human_types() {
        assert_eq!(clean(" be 0477.47.27.01 ", " -./"), "BE0477472701");
        // a character not in the delete list survives, uppercased
        assert_eq!(clean("che-123.456.788 mwst", " -."), "CHE123456788MWST");
    }

    #[test]
    fn a_prefix_is_only_dropped_when_it_is_there() {
        assert_eq!(clean_prefixed("BE0477472701", " -./", "BE"), "0477472701");
        assert_eq!(clean_prefixed("0477472701", " -./", "BE"), "0477472701");
    }

    #[test]
    fn digits_refuses_anything_that_is_not_one() {
        assert_eq!(digits("12"), Some(vec![1, 2]));
        assert_eq!(digits("1A"), None);
        // the fullwidth digit Python's str.isdigit would have accepted
        assert_eq!(digits("１２"), None);
        assert!(!is_digits(""));
    }

    #[test]
    fn slicing_counts_characters_and_never_panics() {
        assert_eq!(slice("ABCDE", 1, 3), "BC");
        assert_eq!(slice("AB", 1, 9), "B");
        assert_eq!(tail("ABCDE", 2), "DE");
        assert_eq!(tail("A", 5), "A");
        assert_eq!(at("AB", 5), None);
        assert_eq!(zfill("42", 5), "00042");
    }
}

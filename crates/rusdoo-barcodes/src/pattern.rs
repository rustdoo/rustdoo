//! A rule's pattern: a regular expression matching the start of the
//! code, with a stretch in braces where the scale or the label printer
//! wrote a number.
//!
//! `2.....{NNNDD}` reads "codes that start with 2, and from the seventh
//! digit on comes a value with three integers and two decimals". What is
//! left when that value is zeroed is the code stored on the product.

use regex::Regex;

/// What matching a pattern against a code revealed.
pub struct PatternMatch {
    pub matched: bool,
    /// the number embedded in the code, zero when the pattern embeds none
    pub value: f64,
    /// the code with the numeric stretch zeroed — this is the one on the
    /// product
    pub base_code: String,
}

/// Where the pattern's numeric stretch is: the first `{N…D…}`.
///
/// Answers the positions in characters, opening at the `{` and closing
/// after the `}`. With neither N nor D it still counts (`{}`), because
/// what refuses that is the rule's constraint, not this scanner.
fn numeric_span(pattern: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = pattern.chars().collect();
    for start in 0..chars.len() {
        if chars[start] != '{' {
            continue;
        }
        let mut cursor = start + 1;
        while chars.get(cursor) == Some(&'N') {
            cursor += 1;
        }
        while chars.get(cursor) == Some(&'D') {
            cursor += 1;
        }
        if chars.get(cursor) == Some(&'}') {
            return Some((start, cursor + 1));
        }
    }
    None
}

/// The regular expression the pattern really becomes: the numeric
/// stretch swapped for the zeros it occupies in the base code.
///
/// The match always runs against the already-zeroed code, so it is this
/// text — and not the pattern as written — that has to compile. That is
/// why
/// que a constraint da regra valida exatamente ele.
fn effective_regex(pattern: &str) -> String {
    let Some((start, end)) = numeric_span(pattern) else {
        return pattern.to_string();
    };
    let chars: Vec<char> = pattern.chars().collect();
    let head: String = chars[..start].iter().collect();
    let tail: String = chars[end..].iter().collect();
    format!("{head}{}{tail}", "0".repeat(end - start - 2))
}

/// Match a code against a pattern and extract the number it embedded.
///
/// The pattern matches a *prefix* of the code (as in Odoo: `re.match`),
/// and the code is cut to the pattern's length before the comparison.
pub fn match_pattern(barcode: &str, pattern: &str) -> PatternMatch {
    let code: Vec<char> = barcode.chars().collect();
    let mut value = 0.0;
    let mut base_code = barcode.to_string();

    if let Some((start, end)) = numeric_span(pattern) {
        let inside: Vec<char> = pattern
            .chars()
            .skip(start + 1)
            .take(end - start - 2)
            .collect();
        let decimals = inside.iter().filter(|c| **c == 'D').count();
        let whole_len = inside.len() - decimals;
        // the stretch of the code lying under the braces; a code shorter
        // than the pattern simply hands over fewer digits
        let digits: Vec<char> = code
            .iter()
            .skip(start)
            .take(inside.len())
            .copied()
            .collect();
        let whole: String = digits.iter().take(whole_len).collect();
        let decimal: String = digits.iter().skip(whole_len).collect();
        let whole = if whole.is_empty() {
            "0".to_string()
        } else {
            whole
        };
        // if there was no number there, there is no value to extract and
        // no base code to build: the rule may still not match, and it is
        // the regex that decides
        if whole.chars().all(|c| c.is_ascii_digit()) {
            let integer: f64 = whole.parse().unwrap_or(0.0);
            let fraction: f64 = format!("0.{decimal}").parse().unwrap_or(0.0);
            value = integer + fraction;
            let head: String = code.iter().take(start).collect();
            let tail: String = code.iter().skip(start + inside.len()).collect();
            base_code = format!("{head}{}{tail}", "0".repeat(inside.len()));
        }
    }

    let effective = effective_regex(pattern);
    let subject: String = base_code
        .chars()
        .take(effective.chars().count())
        .collect::<String>();
    // a pattern that does not compile matches nothing; the rule's
    // constraint keeps one of those from reaching here
    let matched = Regex::new(&format!("^(?:{effective})"))
        .map(|regex| regex.is_match(&subject))
        .unwrap_or(false);

    PatternMatch {
        matched,
        value,
        base_code,
    }
}

/// Is a rule's pattern usable?
///
/// The refusal happens when the rule is saved, not when a code is
/// scanned: a broken pattern discovered at the counter is a sale that
/// stops, and nobody there is going to know the problem is one brace too
/// many on a settings screen.
pub fn check_pattern(pattern: &str) -> Result<(), String> {
    // escaped braces are part of the text to match, they do not delimit
    // the numeric stretch: they leave the count
    let bare = pattern
        .replace("\\\\", "X")
        .replace("\\{", "X")
        .replace("\\}", "X");
    let braces = bare.chars().filter(|c| *c == '{' || *c == '}').count();
    if braces == 2 {
        if numeric_span(&bare).is_none() {
            return Err(format!(
                "pattern {pattern:?} has braces holding something other than N followed by D; \
                 write the integers and then the decimals, as in {{NNNDD}}"
            ));
        }
        if bare.contains("{}") {
            return Err(format!(
                "pattern {pattern:?} has empty braces; say how many digits the value \
                 takes, as in {{NNN}}"
            ));
        }
    } else if braces != 0 {
        return Err(format!(
            "pattern {pattern:?} has more than one pair of braces; a rule embeds one \
             value only"
        ));
    } else if bare == "*" {
        return Err("'*' is not a valid pattern; to match any code write '.*'".into());
    }
    Regex::new(&effective_regex(&bare))
        .map_err(|_| format!("pattern {pattern:?} is not a valid regular expression"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_without_braces_only_matches() {
        let hit = match_pattern("12345670", "........");
        assert!(hit.matched);
        assert_eq!(hit.value, 0.0, "there is no inline value");
        assert_eq!(hit.base_code, "12345670", "the code is itself");

        assert!(!match_pattern("16012344", "11......").matched);
    }

    #[test]
    fn the_whole_code_can_be_the_value() {
        let hit = match_pattern("12345670", "{NNNNNNNN}");
        assert!(hit.matched);
        assert_eq!(hit.value, 12345670.0);
        assert_eq!(hit.base_code, "00000000", "only the zero is left");
    }

    #[test]
    fn decimals_come_after_the_whole_part() {
        // 1020034051259: from the tenth digit on, 12 with one decimal
        let hit = match_pattern("1020034051259", "1........{NND}.");
        assert!(hit.matched);
        assert_eq!(hit.value, 12.5);
        assert_eq!(hit.base_code, "1020034050009");

        let hit = match_pattern("2212345610259", "22......{NNDD}.");
        assert!(hit.matched);
        assert_eq!(hit.value, 10.25);
        assert_eq!(hit.base_code, "2212345600009");
    }

    #[test]
    fn a_value_of_a_single_digit_still_works() {
        let hit = match_pattern("11012344", "11.....{N}");
        assert!(hit.matched);
        assert_eq!(hit.value, 4.0);
        assert_eq!(hit.base_code, "11012340");

        // the same pattern against a code with another prefix does not
        // match, even having extracted a value
        assert!(!match_pattern("66012344", "11.....{N}").matched);
    }

    #[test]
    fn a_leading_zero_in_the_value_is_still_a_number() {
        let hit = match_pattern("66012344", "66{NN}....");
        assert!(hit.matched);
        assert_eq!(hit.value, 1.0);
        assert_eq!(hit.base_code, "66002344");
    }

    #[test]
    fn a_pattern_the_server_can_apply_is_accepted() {
        for pattern in [".*", "........", "22......{NNDD}.", "..>>>{ND}", "{NNN}"] {
            assert!(check_pattern(pattern).is_ok(), "{pattern} devia passar");
        }
    }

    #[test]
    fn a_pattern_the_server_cannot_apply_is_refused() {
        // empty braces: how many digits?
        assert!(check_pattern("......{}..")
            .unwrap_err()
            .contains("empty braces"));
        // decimal antes do inteiro
        assert!(check_pattern("......{DN}")
            .unwrap_err()
            .contains("N followed by D"));
        // two values in a single rule
        assert!(check_pattern("....{NN}{DD}")
            .unwrap_err()
            .contains("more than one pair"));
        // '*' on its own is the mistake everybody makes
        assert!(check_pattern("*").unwrap_err().contains("'.*'"));
        // and what is not even a regex
        assert!(check_pattern("**>>>{ND}")
            .unwrap_err()
            .contains("regular expression"));
    }
}

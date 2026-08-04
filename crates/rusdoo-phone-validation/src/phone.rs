//! Port of `odoo/addons/phone_validation/tools/phone_validation.py`, and
//! of just enough of libphonenumber underneath it to make that port mean
//! something.
//!
//! Odoo's module is thin: four functions wrapping `phonenumbers`. The
//! interesting one is [`phone_parse`], and its trick is worth stating
//! plainly because it looks like a mistake until you know why:
//!
//! > parse the number, format the result internationally, and parse *that*.
//!
//! It is there because Odoo patches libphonenumber's metadata by
//! appending **formatting** rules — a Brazilian mobile gains its ninth
//! digit, a Mexican mobile loses its leading 1 — and formatting is the
//! only place those rules apply. `E164` in libphonenumber takes an early
//! exit that skips formatting entirely, so a number asked for in `E164`
//! would come back exactly as the unpatched library parsed it: wrong.
//! Going through `INTERNATIONAL` once forces the patch to run, and the
//! second parse reads the corrected number back in.
//!
//! The layer underneath — parse, validate, format — is this port's own;
//! see [`crate::metadata`] for what it knows and what it does not.

use crate::metadata::{NumberFormat, Region, REGIONS};
use regex::Regex;
use rusdoo_core::RusdooError;
use std::sync::OnceLock;

/// How many digits an ITU calling code can have.
const MAX_COUNTRY_CODE_DIGITS: usize = 3;

/// The four shapes a number is written in, port of libphonenumber's
/// `PhoneNumberFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `+32456998877` — what a machine stores
    E164,
    /// `+32 456 99 88 77` — what a person reads
    #[default]
    International,
    /// `0456 99 88 77` — what a person dials at home
    National,
    /// `tel:+32-456-99-88-77` — what a `tel:` link carries
    Rfc3966,
}

impl Format {
    /// The format a caller named, the way Odoo names them over RPC.
    pub fn named(name: &str) -> Result<Format, RusdooError> {
        match name {
            "E164" => Ok(Format::E164),
            "INTERNATIONAL" => Ok(Format::International),
            "NATIONAL" => Ok(Format::National),
            "RFC3966" => Ok(Format::Rfc3966),
            other => Err(RusdooError::User(format!(
                "unknown phone format {other:?}: expected E164, INTERNATIONAL, NATIONAL or RFC3966"
            ))),
        }
    }
}

/// A parsed number: the country that answers it, and the rest.
///
/// The national part is kept as digits rather than as an integer because
/// its leading zeros carry meaning — an Ivorian mobile *is* `07…`, and
/// an integer would silently lose the difference between that and `7…`.
/// libphonenumber pays for the integer with a second field,
/// `italian_leading_zero`, which this only has to expose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneNumber {
    pub country_code: u32,
    national: String,
}

impl PhoneNumber {
    /// The national number as dialled, leading zeros and all.
    pub fn national_significant_number(&self) -> &str {
        &self.national
    }

    /// The national number the way `phonenumbers` reports it: an integer,
    /// so without its leading zeros. This is what Odoo puts in
    /// [`RegionData::national_number`].
    pub fn national_number(&self) -> &str {
        let trimmed = self.national.trim_start_matches('0');
        // a number that is all zeros is not a number, but it is also not
        // an empty string: keep the last digit rather than invent one
        if trimmed.is_empty() {
            &self.national[self.national.len() - 1..]
        } else {
            trimmed
        }
    }

    pub fn italian_leading_zero(&self) -> bool {
        self.national.starts_with('0')
    }

    pub fn number_of_leading_zeros(&self) -> usize {
        self.national.len() - self.national.trim_start_matches('0').len()
    }

    /// The ISO country this number belongs to, when the table knows one.
    pub fn region_code(&self) -> Option<&'static str> {
        region_of(self).map(|region| region.meta.code)
    }
}

/// What [`phone_get_region_data_for_number`] answers, mirroring the dict
/// Odoo returns — including the empty strings it uses for "no idea".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionData {
    pub code: String,
    pub national_number: String,
    pub phone_code: String,
}

// ---------------------------------------------------------------------
// The compiled metadata table
// ---------------------------------------------------------------------

struct CompiledFormat {
    pattern: Regex,
    format: &'static str,
    leading: Option<Regex>,
    national_prefix_rule: &'static str,
}

struct Compiled {
    meta: &'static Region,
    general: Regex,
    descs: Vec<Regex>,
    formats: Vec<CompiledFormat>,
    intl_formats: Vec<CompiledFormat>,
    international_prefix: Regex,
    area: Option<Regex>,
}

/// A pattern that has to match the whole national number.
fn whole(pattern: &str) -> Regex {
    Regex::new(&format!("^(?:{pattern})$")).unwrap_or_else(|error| {
        panic!("phone metadata: {pattern:?} is not a regex ({error})");
    })
}

/// A pattern that has to match the *start* of the national number —
/// libphonenumber's `leading_digits_pattern`, which says which formatting
/// rule a number falls under.
fn prefix(pattern: &str) -> Regex {
    Regex::new(&format!("^(?:{pattern})")).unwrap_or_else(|error| {
        panic!("phone metadata: {pattern:?} is not a regex ({error})");
    })
}

fn compile_formats(formats: &'static [NumberFormat]) -> Vec<CompiledFormat> {
    formats
        .iter()
        .map(|rule| CompiledFormat {
            pattern: whole(rule.pattern),
            format: rule.format,
            leading: (!rule.leading.is_empty()).then(|| prefix(rule.leading)),
            national_prefix_rule: rule.national_prefix_rule,
        })
        .collect()
}

/// The table, compiled once for the life of the process.
fn table() -> &'static [Compiled] {
    static TABLE: OnceLock<Vec<Compiled>> = OnceLock::new();
    TABLE.get_or_init(|| {
        REGIONS
            .iter()
            .map(|meta| Compiled {
                meta,
                general: whole(meta.general),
                descs: meta.descs.iter().map(|desc| whole(desc)).collect(),
                formats: compile_formats(meta.formats),
                intl_formats: compile_formats(meta.intl_formats),
                international_prefix: prefix(meta.international_prefix),
                area: (!meta.area.is_empty()).then(|| prefix(meta.area)),
            })
            .collect()
    })
}

fn find_region(code: &str) -> Option<&'static Compiled> {
    table().iter().find(|region| region.meta.code == code)
}

fn regions_for(country_code: u32) -> Vec<&'static Compiled> {
    table()
        .iter()
        .filter(|region| region.meta.country_code == country_code)
        .collect()
}

/// Which country a parsed number belongs to.
///
/// One calling code can serve several countries (the whole of North
/// America shares `+1`), and then only the area code tells them apart;
/// whoever holds the code answers for everything unclaimed.
fn region_of(number: &PhoneNumber) -> Option<&'static Compiled> {
    let candidates = regions_for(number.country_code);
    if let [only] = candidates[..] {
        return Some(only);
    }
    for candidate in &candidates {
        if let Some(area) = &candidate.area {
            if area.is_match(&number.national) {
                return Some(candidate);
            }
        }
    }
    candidates
        .iter()
        .copied()
        .find(|candidate| candidate.meta.main)
        .or_else(|| candidates.first().copied())
}

// ---------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------

/// Why a string is not a phone number at all — libphonenumber's
/// `NumberParseException`, which Odoo shows to the user as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    NotANumber,
    InvalidCountryCode,
    MissingRegion,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            ParseError::NotANumber => "the string supplied did not seem to be a phone number",
            ParseError::InvalidCountryCode => "country calling code supplied was not recognised",
            ParseError::MissingRegion => "missing or invalid default region",
        };
        f.write_str(text)
    }
}

/// The international prefix a caller dialled, dropped — libphonenumber's
/// `_maybe_strip_i18n_prefix`. `None` when the number does not start with
/// one.
///
/// The digit after the prefix may not be a zero: `00` in front of `0…` is
/// somebody's national number that happens to begin with two zeros, not a
/// call abroad.
fn strip_international_prefix<'a>(home: &Compiled, digits: &'a str) -> Option<&'a str> {
    let matched = home.international_prefix.find(digits)?;
    let rest = &digits[matched.end()..];
    if rest.is_empty() || rest.starts_with('0') {
        return None;
    }
    Some(rest)
}

/// The calling code at the front of an international number, and what
/// follows it.
///
/// Shortest first, like libphonenumber: the codes are assigned so that no
/// short one is the start of a long one, so the first match is the right
/// one.
fn split_country_code(digits: &str) -> Option<(u32, &str)> {
    for length in 1..=MAX_COUNTRY_CODE_DIGITS {
        if digits.len() <= length {
            break;
        }
        let Ok(candidate) = digits[..length].parse::<u32>() else {
            break;
        };
        if table()
            .iter()
            .any(|region| region.meta.country_code == candidate)
        {
            return Some((candidate, &digits[length..]));
        }
    }
    None
}

/// The national prefix dropped, port of
/// `_maybe_strip_national_prefix_and_carrier_code`.
///
/// It stays when dropping it would break a number that was already
/// well-formed: `0` is Belgium's trunk prefix, but it is also the first
/// digit of numbers elsewhere, and a rule that always strips would eat
/// them.
fn strip_national_prefix<'a>(home: &Compiled, digits: &'a str) -> &'a str {
    let prefix = home.meta.national_prefix;
    if prefix.is_empty() {
        return digits;
    }
    let Some(stripped) = digits.strip_prefix(prefix) else {
        return digits;
    };
    if stripped.is_empty() {
        return digits;
    }
    if home.general.is_match(digits) && !home.general.is_match(stripped) {
        return digits;
    }
    stripped
}

/// One pass of libphonenumber's `parse`: what the string says, before
/// anybody asks whether such a number could exist.
fn parse_raw(input: &str, country_code: Option<&str>) -> Result<PhoneNumber, ParseError> {
    let trimmed = input.trim();
    // an RFC3966 number arrives as `tel:+32456998877`
    let trimmed = trimmed.strip_prefix("tel:").unwrap_or(trimmed);
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err(ParseError::NotANumber);
    }
    let home = country_code.and_then(find_region);

    let mut international = trimmed.starts_with('+');
    let mut rest = digits.as_str();
    if !international {
        if let Some(home) = home {
            if let Some(after) = strip_international_prefix(home, rest) {
                international = true;
                rest = after;
            }
        }
    }
    if international {
        let (country_code, national) =
            split_country_code(rest).ok_or(ParseError::InvalidCountryCode)?;
        return Ok(PhoneNumber {
            country_code,
            national: national.to_string(),
        });
    }

    // no `+`, no international prefix: the country has to come from
    // outside the number, and without it there is nothing to parse
    let home = home.ok_or(ParseError::MissingRegion)?;
    Ok(PhoneNumber {
        country_code: home.meta.country_code,
        national: strip_national_prefix(home, rest).to_string(),
    })
}

/// Whether a number *could* exist, and when not, what is wrong with it —
/// libphonenumber's `is_possible_number_with_reason`.
///
/// Length only. It is a cheaper and far more stable question than "is
/// this number in service": digits get dropped and doubled by hand far
/// more often than a numbering plan changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Possibility {
    Possible,
    InvalidCountryCode,
    TooShort,
    TooLong,
    InvalidLength,
}

fn possibility(number: &PhoneNumber) -> Possibility {
    let candidates = regions_for(number.country_code);
    if candidates.is_empty() {
        return Possibility::InvalidCountryCode;
    }
    let lengths: Vec<usize> = candidates
        .iter()
        .flat_map(|region| region.meta.lengths.iter().copied())
        .collect();
    let length = number.national.len();
    let shortest = lengths.iter().copied().min().unwrap_or(0);
    let longest = lengths.iter().copied().max().unwrap_or(0);
    if length < shortest {
        return Possibility::TooShort;
    }
    if length > longest {
        return Possibility::TooLong;
    }
    if !lengths.contains(&length) {
        return Possibility::InvalidLength;
    }
    Possibility::Possible
}

/// Whether the number matches a range that is actually handed out —
/// libphonenumber's `is_valid_number`.
fn is_valid(number: &PhoneNumber) -> bool {
    regions_for(number.country_code).iter().any(|region| {
        region
            .descs
            .iter()
            .any(|desc| desc.is_match(&number.national))
    })
}

// ---------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------

/// The first rule that fits, applied. `None` when no rule does, which is
/// the caller's cue to print the digits unadorned rather than to fail —
/// a number nobody knows how to lay out is still a number.
fn apply(
    formats: &[CompiledFormat],
    national: &str,
    with_national_prefix: bool,
) -> Option<String> {
    for rule in formats {
        if let Some(leading) = &rule.leading {
            if !leading.is_match(national) {
                continue;
            }
        }
        let Some(captures) = rule.pattern.captures(national) else {
            continue;
        };
        // the national prefix rule replaces the first group in the
        // template, and carries the prefix with it: `${1}` becomes
        // `0${1}` or `(${1})`
        let template = if with_national_prefix && !rule.national_prefix_rule.is_empty() {
            rule.format.replacen("${1}", rule.national_prefix_rule, 1)
        } else {
            rule.format.to_string()
        };
        let mut out = String::new();
        captures.expand(&template, &mut out);
        return Some(out);
    }
    None
}

/// The rules for writing a number that is not a local call: a country's
/// own if it declares any, otherwise its national rules read without the
/// trunk prefix.
fn international_formats(region: &'static Compiled) -> &'static [CompiledFormat] {
    if region.intl_formats.is_empty() {
        &region.formats
    } else {
        &region.intl_formats
    }
}

/// Separators as RFC 3966 wants them: one hyphen wherever the readable
/// form put a space or a bracket.
fn dashed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending = false;
    for character in text.chars() {
        if character.is_ascii_digit() {
            if pending && !out.is_empty() {
                out.push('-');
            }
            pending = false;
            out.push(character);
        } else {
            pending = true;
        }
    }
    out
}

/// Write a parsed number out. Port of `phonenumbers.format_number`.
pub fn format(number: &PhoneNumber, want: Format) -> String {
    let country_code = number.country_code;
    let national = &number.national;
    if want == Format::E164 {
        return format!("+{country_code}{national}");
    }
    let Some(region) = region_of(number) else {
        // a calling code the table does not carry: the digits are all
        // that is honestly known about the number
        return format!("+{country_code}{national}");
    };
    match want {
        Format::E164 => unreachable!("handled above"),
        Format::National => {
            apply(&region.formats, national, true).unwrap_or_else(|| national.clone())
        }
        Format::International => {
            let body = apply(international_formats(region), national, false)
                .unwrap_or_else(|| national.clone());
            format!("+{country_code} {body}")
        }
        Format::Rfc3966 => {
            let body = apply(international_formats(region), national, false)
                .unwrap_or_else(|| national.clone());
            format!("tel:+{country_code}-{}", dashed(&body))
        }
    }
}

// ---------------------------------------------------------------------
// The module's own API, as `tools/phone_validation.py` declares it
// ---------------------------------------------------------------------

fn user_error(message: String) -> RusdooError {
    RusdooError::User(message)
}

/// Parse `number`, reading it as a number of `country_code` when it does
/// not say which country it belongs to.
///
/// Port of `phone_validation.phone_parse`, including the two things that
/// look odd in it and are not: the double parse (see the module docs),
/// and the retry when a number has too many digits — somebody who typed
/// `0033…` or `33…` meant `+33…`, and telling them "too many digits"
/// when the digits are right and the prefix is missing helps nobody.
pub fn phone_parse(number: &str, country_code: Option<&str>) -> Result<PhoneNumber, RusdooError> {
    let parsed = parse_raw(number, country_code)
        .map_err(|error| user_error(format!("Unable to parse {number}: {error}")))?;
    // format and re-read: it is the only place Odoo's metadata patches
    // apply, and skipping it silently un-patches Brazil and Mexico
    let formatted = format(&parsed, Format::International);
    let mut parsed = parse_raw(&formatted, country_code)
        .map_err(|error| user_error(format!("Unable to parse {number}: {error}")))?;

    match possibility(&parsed) {
        Possibility::Possible => {}
        Possibility::InvalidCountryCode => {
            return Err(user_error(format!(
                "Impossible number {number}: not a valid country prefix."
            )))
        }
        Possibility::TooShort => {
            return Err(user_error(format!(
                "Impossible number {number}: not enough digits."
            )))
        }
        Possibility::TooLong => {
            let too_long = || user_error(format!("Impossible number {number}: too many digits."));
            // people may enter 0033... instead of +33...
            if let Some(rest) = number.strip_prefix("00") {
                parsed = phone_parse(&format!("+{rest}"), country_code).map_err(|_| too_long())?;
            // people may enter 33... instead of +33...
            } else if !number.starts_with('+') {
                parsed = phone_parse(&format!("+{number}"), country_code).map_err(|_| too_long())?;
            } else {
                return Err(too_long());
            }
        }
        Possibility::InvalidLength => {
            return Err(user_error(format!(
                "The phone number {number} is invalid! Let's fix it - you are not dialing aliens."
            )))
        }
    }
    if !is_valid(&parsed) {
        return Err(user_error(format!(
            "Invalid number {number}: probably incorrect prefix."
        )));
    }
    Ok(parsed)
}

/// Format `number` for a reader in `country_code` / `country_phone_code`.
///
/// Port of `phone_validation.phone_format`. `raise_exception = false`
/// answers with the number exactly as it came in when it cannot be
/// parsed, which is what lets a screen show what the user typed instead
/// of blanking it.
pub fn phone_format(
    number: &str,
    country_code: Option<&str>,
    country_phone_code: Option<u32>,
    force_format: Format,
    raise_exception: bool,
) -> Result<String, RusdooError> {
    let parsed = match phone_parse(number, country_code) {
        Ok(parsed) => parsed,
        Err(error) => {
            if raise_exception {
                return Err(error);
            }
            return Ok(number.to_string());
        }
    };
    // a national format only means anything to a reader in that same
    // country; to anyone else it is a number missing its prefix
    let want = match force_format {
        Format::National if Some(parsed.country_code) != country_phone_code => Format::International,
        other => other,
    };
    Ok(format(&parsed, want))
}

/// The ISO country a number belongs to, or `""` when it cannot be told.
/// Port of `phone_get_country_code_for_number`.
pub fn phone_get_country_code_for_number(number: &str) -> String {
    phone_get_region_data_for_number(number).code
}

/// What is known about a number written in full, with no country to lean
/// on. Port of `phone_get_region_data_for_number`: a number that cannot
/// be parsed answers with empty strings rather than an error, because the
/// callers use it to *enrich* a record and must not fail over one bad
/// contact.
pub fn phone_get_region_data_for_number(number: &str) -> RegionData {
    let Ok(parsed) = phone_parse(number, None) else {
        return RegionData::default();
    };
    RegionData {
        code: parsed.region_code().unwrap_or_default().to_string(),
        national_number: parsed.national_number().to_string(),
        phone_code: parsed.country_code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern in the table is a regex, and every region is
    /// reachable. A typo in the metadata is a panic on first use
    /// otherwise — in whichever request happened to touch that country.
    #[test]
    fn the_whole_metadata_table_compiles() {
        assert!(!table().is_empty());
        for region in table() {
            assert!(
                find_region(region.meta.code).is_some(),
                "{} is not reachable by its code",
                region.meta.code
            );
            assert!(
                !region.meta.lengths.is_empty(),
                "{} declares no possible length",
                region.meta.code
            );
            assert!(
                !region.descs.is_empty(),
                "{} describes no kind of number",
                region.meta.code
            );
        }
    }

    /// Exactly one country answers for each calling code, so that a
    /// number written in full always resolves.
    #[test]
    fn every_calling_code_has_one_country_answering_for_it() {
        for region in table() {
            let sharing = regions_for(region.meta.country_code);
            let main = sharing.iter().filter(|other| other.meta.main).count();
            assert_eq!(
                main, 1,
                "+{} has {main} main countries",
                region.meta.country_code
            );
        }
    }

    #[test]
    fn a_national_number_needs_the_country_from_outside() {
        // Odoo's `test_country_code_falsy`
        assert_eq!(
            phone_format("0456998877", Some("BE"), Some(32), Format::E164, true).unwrap(),
            "+32456998877"
        );
        let error = phone_format("0456998877", None, Some(32), Format::E164, true)
            .expect_err("without a country there is nothing to read the 0 against");
        assert!(error.to_string().contains("Unable to parse"), "{error}");
    }

    #[test]
    fn a_word_is_not_a_number() {
        // Odoo's `test_phone_format_error`
        let error = phone_format("abc", Some("BE"), Some(32), Format::International, true)
            .expect_err("letters are not digits");
        assert!(error.to_string().contains("Unable to parse"), "{error}");
        // and with raise_exception off the caller gets back what was typed
        assert_eq!(
            phone_format("abc", Some("BE"), Some(32), Format::International, false).unwrap(),
            "abc"
        );
    }

    #[test]
    fn a_number_says_which_country_it_came_from() {
        // Odoo's `test_get_region_data_for_number`
        for (source, code, national, phone_code) in [
            ("+32456998877", "BE", "456998877", "32"),
            // Canada and the United States share +1: only the area code
            // tells them apart
            ("+1-613-555-0177", "CA", "6135550177", "1"),
            ("+1-202-555-0124", "US", "2025550124", "1"),
        ] {
            assert_eq!(
                phone_get_region_data_for_number(source),
                RegionData {
                    code: code.into(),
                    national_number: national.into(),
                    phone_code: phone_code.into(),
                },
                "{source}"
            );
            assert_eq!(phone_get_country_code_for_number(source), code);
        }
        // a number nobody can read answers with nothing, never an error
        assert_eq!(
            phone_get_region_data_for_number("nonsense"),
            RegionData::default()
        );
    }

    #[test]
    fn a_brazilian_mobile_gains_its_ninth_digit() {
        // Odoo's `test_phone_format_e164_brazil`: the 2016 change, and
        // the reason `phone_parse` formats before it parses again
        for (number, expected) in [
            ("11 6123 4560", "+5511961234560"),
            ("+55 11 6123 4561", "+5511961234561"),
            // a landline must NOT grow a ninth digit
            ("11 2345 6789", "+551123456789"),
            ("+55 11 2345 6798", "+551123456798"),
        ] {
            assert_eq!(
                phone_format(number, Some("BR"), Some(55), Format::E164, true).unwrap(),
                expected,
                "{number}"
            );
        }
    }

    #[test]
    fn a_mexican_mobile_loses_its_leading_one() {
        // Odoo's `test_phone_format_e164_mexico`: the 2019 change
        for (number, expected) in [
            ("+5215585440659", "+525585440659"),
            ("15585440749", "+525585440749"),
            ("+525595440749", "+525595440749"),
            ("5585460749", "+525585460749"),
        ] {
            assert_eq!(
                phone_format(number, Some("MX"), Some(52), Format::E164, true).unwrap(),
                expected,
                "{number}"
            );
        }
    }

    #[test]
    fn the_patched_regions_parse_the_numbers_odoo_says_they_must() {
        // Odoo's `test_region_*_monkey_patch`, one line per case
        for (number, region, national, country_code) in [
            ("+2250506007995", None, "506007995", 225),
            ("0506007995", Some("CI"), "506007995", 225),
            ("+225 05 20 963 777", None, "520963777", 225),
            ("3241234567", Some("CO"), "3241234567", 57),
            ("+57 324 1234567", None, "3241234567", 57),
            ("055 294 1234", Some("IL"), "552941234", 972),
            ("+972 55 295 1235", None, "552951235", 972),
            ("+212 6 23 24 56 28", None, "623245628", 212),
            ("+212603190852", None, "603190852", 212),
            ("+212780137429", None, "780137429", 212),
            ("+212546547649", None, "546547649", 212),
            ("+212690979618", None, "690979618", 212),
            ("+23057654321", None, "57654321", 230),
            ("+2305 76/54 3-21 ", None, "57654321", 230),
            ("57654321", Some("MU"), "57654321", 230),
            ("5 76/54 3-21 ", Some("MU"), "57654321", 230),
            ("+254711123456", None, "711123456", 254),
            ("+254 711 123 456", None, "711123456", 254),
            ("+254-711-123-456", None, "711123456", 254),
            ("0711123456", Some("KE"), "711123456", 254),
            ("0711/123/456", Some("KE"), "711123456", 254),
            ("6198 5462", Some("PA"), "61985462", 507),
            ("+507 833 8744", None, "8338744", 507),
            ("+221750142092", None, "750142092", 221),
            ("+22176 707 0065", None, "767070065", 221),
        ] {
            let parsed = phone_parse(number, region)
                .unwrap_or_else(|error| panic!("{number} ({region:?}): {error}"));
            assert_eq!(parsed.national_number(), national, "{number}");
            assert_eq!(parsed.country_code, country_code, "{number}");
        }
    }

    #[test]
    fn an_ivorian_mobile_keeps_the_zero_it_is_dialled_with() {
        let parsed = phone_parse("0506007995", Some("CI")).unwrap();
        assert!(parsed.italian_leading_zero(), "a CI mobile really is 07…/05…");
        assert_eq!(parsed.number_of_leading_zeros(), 1);
        assert_eq!(parsed.national_significant_number(), "0506007995");
        // and the integer form is what Odoo reports, so without the zero
        assert_eq!(parsed.national_number(), "506007995");
    }

    #[test]
    fn one_number_reads_four_ways() {
        let parsed = phone_parse("+32456998877", None).unwrap();
        assert_eq!(format(&parsed, Format::E164), "+32456998877");
        assert_eq!(format(&parsed, Format::International), "+32 456 99 88 77");
        assert_eq!(format(&parsed, Format::National), "0456 99 88 77");
        assert_eq!(format(&parsed, Format::Rfc3966), "tel:+32-456-99-88-77");
    }

    #[test]
    fn the_national_format_is_only_offered_to_a_local_reader() {
        // a Belgian number shown to a Belgian company
        assert_eq!(
            phone_format("0456998877", Some("BE"), Some(32), Format::National, true).unwrap(),
            "0456 99 88 77"
        );
        // the same number on a French company's screen: without the
        // prefix, `0456 99 88 77` is a number nobody there can dial
        assert_eq!(
            phone_format("+32456998877", Some("FR"), Some(33), Format::National, true).unwrap(),
            "+32 456 99 88 77"
        );
    }

    #[test]
    fn a_number_typed_without_its_plus_is_still_read() {
        // The TOO_LONG retry (`phone_validation.py`): `0033…` and `33…`
        // both mean `+33…`. It only runs on a number that *parsed* and
        // came out too long, which needs a region — without one and
        // without a `+`, libphonenumber refuses before there is anything
        // to retry, and so does this.
        for typed in ["0033456789012", "33456789012"] {
            assert_eq!(
                phone_parse(typed, Some("BE"))
                    .unwrap_or_else(|error| panic!("{typed}: {error}"))
                    .country_code,
                33,
                "{typed}"
            );
            let error = phone_parse(typed, None).expect_err("no region, no plus, no number");
            assert!(error.to_string().contains("Unable to parse"), "{error}");
        }
    }

    #[test]
    fn a_number_that_is_short_a_digit_is_refused() {
        // Odoo has two sentences for a number it will not take — "not
        // enough digits" when the length rules it out, "probably
        // incorrect prefix" when the shape does. Which one a given
        // number gets depends on libphonenumber's per-country length
        // table, and this port's is coarser; what both agree on, and
        // what a caller depends on, is the refusal.
        let error = phone_parse("+3245699887", None).expect_err("nine digits, not ten");
        let message = error.to_string();
        assert!(
            message.contains("not enough digits") || message.contains("incorrect prefix"),
            "{message}"
        );
    }

    #[test]
    fn an_unassigned_prefix_is_refused_rather_than_guessed() {
        // +999 is not a calling code, and inventing one would store a
        // number that can never be dialled
        let error = phone_parse("+9991234567", None).expect_err("+999 belongs to nobody");
        assert!(error.to_string().contains("Unable to parse"), "{error}");
    }

    #[test]
    fn a_well_shaped_number_in_a_range_nobody_hands_out_is_refused() {
        // right length for Belgium, but `1` is not the start of any
        // Belgian range that is actually assigned
        let error = phone_parse("+32111111111", None).expect_err("no such Belgian range");
        assert!(error.to_string().contains("Invalid number"), "{error}");
    }

    #[test]
    fn each_format_name_the_client_may_send_is_known() {
        assert_eq!(Format::named("E164").unwrap(), Format::E164);
        assert_eq!(Format::named("NATIONAL").unwrap(), Format::National);
        assert_eq!(Format::named("RFC3966").unwrap(), Format::Rfc3966);
        assert_eq!(
            Format::named("INTERNATIONAL").unwrap(),
            Format::International
        );
        assert!(Format::named("E123").is_err());
    }
}

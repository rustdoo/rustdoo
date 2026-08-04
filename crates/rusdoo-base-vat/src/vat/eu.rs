//! The twenty-seven member states, plus the United Kingdom.
//!
//! Every function here is a port of the `stdnum` module Odoo reaches for
//! through `stdnum.util.get_cc_module(country_code, 'vat')`, except where
//! `base_vat` writes its own `check_vat_xx` — those say so.
//!
//! Each country contributes two things: a `compact`, which is what Odoo's
//! `_format_vat_number` stores, and a `check`, which is what decides
//! whether the number is refused.

use super::checksum::{
    ends_with_digit, luhn_check_digit, luhn_is_valid, mod_11_10_is_valid, mod_97_10_is_valid,
    weighted,
};
use super::text::{as_u64, at, clean, clean_prefixed, digits, is_digits, slice, tail, zfill};
use chrono::NaiveDate;

/// Is there a day `day` of month `month` in year `year`?
fn is_a_date(year: i32, month: u32, day: u32) -> bool {
    NaiveDate::from_ymd_opt(year, month, day).is_some()
}

/// The number `from..to` read as an integer, for the date parts embedded
/// in the personal codes of half of Europe.
fn part(number: &str, from: usize, to: usize) -> Option<i64> {
    slice(number, from, to).parse().ok()
}

// ---------------------------------------------------------------------
// AT — Umsatzsteuer-Identifikationsnummer (stdnum.at.uid)
// ---------------------------------------------------------------------

pub fn at_compact(vat: &str) -> String {
    clean_prefixed(vat, " -./", "AT")
}

pub fn at_check(vat: &str) -> bool {
    let number = at_compact(vat);
    if number.chars().count() != 9 || at(&number, 0) != Some('U') || !is_digits(&slice(&number, 1, 9))
    {
        return false;
    }
    // Luhn over the seven digits between the U and the check digit
    let Some(checksum) = super::checksum::luhn(&slice(&number, 1, 8)) else {
        return false;
    };
    // `(96 - total) % 10`, and `luhn` already returned `total % 10`, so
    // 16 keeps it non-negative like Python's modulo. The comparison is on
    // the digits alone: the leading `U` is part of the number and not of
    // the checksum, and `ends_with_digit` refuses anything that is not
    // all digits.
    ends_with_digit(&slice(&number, 1, 9), i64::from((16 - checksum) % 10))
}

// ---------------------------------------------------------------------
// BE — BTW-identificatienummer (stdnum.be.vat)
// ---------------------------------------------------------------------

pub fn be_compact(vat: &str) -> String {
    let mut number = clean_prefixed(vat, " -./", "BE");
    if let Some(rest) = number.strip_prefix("(0)") {
        number = format!("0{rest}");
    }
    if number.chars().count() == 9 {
        // the old format had nine digits; the tenth is a leading zero
        number = format!("0{number}");
    }
    number
}

pub fn be_check(vat: &str) -> bool {
    let number = be_compact(vat);
    if number.chars().count() != 10 || as_u64(&number).is_none_or(|value| value == 0) {
        return false;
    }
    if !matches!(at(&number, 0), Some('0' | '1')) {
        return false;
    }
    let (Some(body), Some(check)) = (as_u64(&slice(&number, 0, 8)), as_u64(&tail(&number, 2)))
    else {
        return false;
    };
    (body + check) % 97 == 0
}

// ---------------------------------------------------------------------
// BG — Идентификационен номер по ДДС (stdnum.bg.vat)
// ---------------------------------------------------------------------

pub fn bg_compact(vat: &str) -> String {
    clean_prefixed(vat, " -.", "BG")
}

/// The check digit of a nine-digit number, which belongs to a company.
fn bg_legal_check(number: &str) -> bool {
    let body = slice(number, 0, 8);
    let Some(values) = digits(&body) else {
        return false;
    };
    let mut check: i64 = values
        .iter()
        .enumerate()
        .map(|(i, n)| (i as i64 + 1) * i64::from(*n))
        .sum::<i64>()
        % 11;
    if check == 10 {
        check = values
            .iter()
            .enumerate()
            .map(|(i, n)| (i as i64 + 3) * i64::from(*n))
            .sum::<i64>()
            % 11;
    }
    ends_with_digit(number, check % 10)
}

/// ЕГН, the personal code: a birth date and a check digit.
fn bg_egn_check(number: &str) -> bool {
    if number.chars().count() != 10 || !is_digits(number) {
        return false;
    }
    let (Some(yy), Some(mm), Some(dd)) = (part(number, 0, 2), part(number, 2, 4), part(number, 4, 6))
    else {
        return false;
    };
    let (mut year, mut month) = (yy + 1900, mm);
    // the month carries the century: +40 for the 2000s, +20 for the 1800s
    if month > 40 {
        year += 100;
        month -= 40;
    } else if month > 20 {
        year -= 100;
        month -= 20;
    }
    if !is_a_date(year as i32, month as u32, dd as u32) {
        return false;
    }
    let Some(sum) = weighted(number, &[2, 4, 8, 5, 10, 9, 7, 3, 6]) else {
        return false;
    };
    ends_with_digit(number, sum % 11 % 10)
}

/// ЛНЧ, the personal number of a foreigner.
fn bg_pnf_check(number: &str) -> bool {
    if number.chars().count() != 10 || !is_digits(number) {
        return false;
    }
    let Some(sum) = weighted(number, &[21, 19, 17, 13, 11, 9, 7, 3, 1]) else {
        return false;
    };
    ends_with_digit(number, sum % 10)
}

pub fn bg_check(vat: &str) -> bool {
    let number = bg_compact(vat);
    if !is_digits(&number) {
        return false;
    }
    match number.chars().count() {
        9 => bg_legal_check(&number),
        10 => {
            bg_egn_check(&number) || bg_pnf_check(&number) || {
                let sum = weighted(&number, &[4, 3, 2, 7, 6, 5, 4, 3, 2]).unwrap_or(0);
                ends_with_digit(&number, (11 - sum).rem_euclid(11))
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------
// CY — Αριθμός Εγγραφής Φ.Π.Α. (stdnum.cy.vat)
// ---------------------------------------------------------------------

pub fn cy_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "CY")
}

pub fn cy_check(vat: &str) -> bool {
    let number = cy_compact(vat);
    if number.chars().count() != 9 || !is_digits(&slice(&number, 0, 8)) {
        return false;
    }
    if slice(&number, 0, 2) == "12" {
        return false;
    }
    // the even positions are translated through a table, the odd ones
    // counted as they are
    const TRANSLATION: [i64; 10] = [1, 0, 5, 7, 9, 13, 15, 17, 19, 21];
    let Some(values) = digits(&slice(&number, 0, 8)) else {
        return false;
    };
    let total: i64 = values
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if i % 2 == 0 {
                TRANSLATION[*n as usize]
            } else {
                i64::from(*n)
            }
        })
        .sum();
    let expected = b'A' + (total % 26) as u8;
    at(&number, 8) == Some(expected as char)
}

// ---------------------------------------------------------------------
// CZ — Daňové identifikační číslo (stdnum.cz.dic)
// ---------------------------------------------------------------------

pub fn cz_compact(vat: &str) -> String {
    clean_prefixed(vat, " /", "CZ")
}

/// Rodné číslo, the birth number Czechia and Slovakia share.
fn birth_number_check(number: &str) -> bool {
    let length = number.chars().count();
    if !is_digits(number) || !(9..=10).contains(&length) {
        return false;
    }
    let (Some(yy), Some(mm), Some(dd)) = (part(number, 0, 2), part(number, 2, 4), part(number, 4, 6))
    else {
        return false;
    };
    let mut year = 1900 + yy;
    // women have 50 added to the month, and 20 more when the serial for
    // the day overflowed (which only happens from 2004 on)
    let month = mm % 50 % 20;
    if length == 9 {
        if year >= 1980 {
            year -= 100;
        }
        if year > 1953 {
            // nine-digit numbers stopped being issued in 1954
            return false;
        }
    } else if year < 1954 {
        year += 100;
    }
    if !is_a_date(year as i32, month as u32, dd as u32) {
        return false;
    }
    if length == 10 {
        let Some(body) = as_u64(&slice(number, 0, 9)) else {
            return false;
        };
        return ends_with_digit(number, (body % 11 % 10) as i64);
    }
    true
}

pub fn cz_check(vat: &str) -> bool {
    let number = cz_compact(vat);
    if !is_digits(&number) {
        return false;
    }
    match number.chars().count() {
        8 => {
            if at(&number, 0) == Some('9') {
                return false;
            }
            let Some(sum) = weighted(&number, &[8, 7, 6, 5, 4, 3, 2]) else {
                return false;
            };
            let check = (11 - sum).rem_euclid(11);
            ends_with_digit(&number, if check == 0 { 1 } else { check } % 10)
        }
        9 if at(&number, 0) == Some('6') => {
            // a special case whose first digit is left out of the sum
            let body = slice(&number, 1, 8);
            let Some(sum) = weighted(&body, &[8, 7, 6, 5, 4, 3, 2]) else {
                return false;
            };
            let check = sum % 11;
            ends_with_digit(&number, (8 - (10 - check).rem_euclid(11)).rem_euclid(10))
        }
        9 | 10 => birth_number_check(&number),
        _ => false,
    }
}

// ---------------------------------------------------------------------
// DE — Umsatzsteuer-Identifikationsnummer (stdnum.de.vat and de.stnr)
// ---------------------------------------------------------------------

pub fn de_compact(vat: &str) -> String {
    clean_prefixed(vat, " -./,", "DE")
}

/// The Steuernummer, the *national* tax number a German business also
/// has (`stdnum.de.stnr`).
///
/// `stdnum` matches it against a table of sixteen regional formats. Every
/// ten- and eleven-digit number matches one of them — Baden-Württemberg's
/// `FFBBBUUUUP` and Bayern's `FFFBBBUUUUP` are all-digit patterns — so
/// only the thirteen-digit, country-wide forms actually constrain
/// anything: a state prefix, then a `0` in the fifth position.
fn de_stnr_check(number: &str) -> bool {
    if !is_digits(number) {
        return false;
    }
    match number.chars().count() {
        10 | 11 => true,
        13 => {
            const STATE_PREFIXES: [&str; 16] = [
                "28", "9", "11", "30", "24", "22", "26", "40", "23", "5", "27", "10", "32", "31",
                "21", "41",
            ];
            at(number, 4) == Some('0')
                && STATE_PREFIXES
                    .iter()
                    .any(|prefix| number.starts_with(prefix))
        }
        _ => false,
    }
}

/// `check_vat_de` in `base_vat`: a German partner may carry either the
/// intra-community number or its national Steuernummer.
pub fn de_check(vat: &str) -> bool {
    let number = de_compact(vat);
    let vat_number = number.chars().count() == 9
        && at(&number, 0) != Some('0')
        && is_digits(&number)
        && mod_11_10_is_valid(&number);
    vat_number || de_stnr_check(&number)
}

// ---------------------------------------------------------------------
// DK — CVR-nummer (stdnum.dk.cvr)
// ---------------------------------------------------------------------

pub fn dk_compact(vat: &str) -> String {
    clean_prefixed(vat, " -.,/:", "DK")
}

pub fn dk_check(vat: &str) -> bool {
    let number = dk_compact(vat);
    number.chars().count() == 8
        && is_digits(&number)
        && at(&number, 0) != Some('0')
        && weighted(&number, &[2, 7, 6, 5, 4, 3, 2, 1]).is_some_and(|sum| sum % 11 == 0)
}

// ---------------------------------------------------------------------
// EE — Käibemaksukohustuslase number (stdnum.ee.kmkr)
// ---------------------------------------------------------------------

pub fn ee_compact(vat: &str) -> String {
    clean_prefixed(vat, " ", "EE")
}

pub fn ee_check(vat: &str) -> bool {
    let number = ee_compact(vat);
    number.chars().count() == 9
        && is_digits(&number)
        && weighted(&number, &[3, 7, 1, 3, 7, 1, 3, 7, 1]).is_some_and(|sum| sum % 10 == 0)
}

// ---------------------------------------------------------------------
// ES — Número de Identificación Fiscal (stdnum.es.nif)
// ---------------------------------------------------------------------

pub fn es_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "ES")
}

/// The letter that closes a DNI, which is the number modulo 23.
fn es_dni_letter(body: &str) -> Option<char> {
    const LETTERS: &[u8] = b"TRWAGMYFPDXBNJZSQVHLCKE";
    let value = as_u64(body)?;
    Some(LETTERS[(value % 23) as usize] as char)
}

pub fn es_check(vat: &str) -> bool {
    let number = es_compact(vat);
    if number.chars().count() != 9 || !is_digits(&slice(&number, 1, 8)) {
        return false;
    }
    let body = slice(&number, 0, 8);
    let last = at(&number, 8);
    match at(&number, 0) {
        // K, L and M are Spaniards without a DNI of their own; they keep
        // the DNI's own check letter
        Some('K' | 'L' | 'M') => es_dni_letter(&slice(&number, 1, 8)) == last,
        Some(c) if c.is_ascii_digit() => es_dni_letter(&body) == last,
        Some(first @ ('X' | 'Y' | 'Z')) => {
            // a foreign natural person: X, Y and Z stand for 0, 1 and 2
            let replaced = format!(
                "{}{}",
                "XYZ".find(first).expect("matched above"),
                slice(&number, 1, 8)
            );
            es_dni_letter(&replaced) == last
        }
        Some(first) if "ABCDEFGHJNPQRSUVW".contains(first) => {
            // a company (CIF): the check may be written as a digit or as
            // the letter standing for it, and both are accepted
            let Some(digit) = luhn_check_digit(&slice(&number, 1, 8)) else {
                return false;
            };
            let letter = b"JABCDEFGHI"[digit.to_digit(10).expect("a digit") as usize] as char;
            last == Some(digit) || last == Some(letter)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------
// FI — Arvonlisäveronumero (stdnum.fi.alv)
// ---------------------------------------------------------------------

pub fn fi_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "FI")
}

pub fn fi_check(vat: &str) -> bool {
    let number = fi_compact(vat);
    number.chars().count() == 8
        && is_digits(&number)
        && weighted(&number, &[7, 9, 10, 5, 8, 4, 2, 1]).is_some_and(|sum| sum % 11 == 0)
}

// ---------------------------------------------------------------------
// FR — Numéro de TVA intracommunautaire (stdnum.fr.tva)
// ---------------------------------------------------------------------

/// The alphabet the two key characters are drawn from: digits and the
/// letters, minus I and O, which read as 1 and 0.
const FR_ALPHABET: &str = "0123456789ABCDEFGHJKLMNPQRSTUVWXYZ";

pub fn fr_compact(vat: &str) -> String {
    clean_prefixed(vat, " -.", "FR")
}

pub fn fr_check(vat: &str) -> bool {
    let number = fr_compact(vat);
    if number.chars().count() != 11 {
        return false;
    }
    let key = slice(&number, 0, 2);
    let siren = slice(&number, 2, 11);
    if !key.chars().all(|c| FR_ALPHABET.contains(c)) || !is_digits(&siren) {
        return false;
    }
    // a Monaco number is a valid TVA but not a SIREN, and says so by
    // starting with three zeros
    if slice(&number, 2, 5) != "000" && !luhn_is_valid(&siren) {
        return false;
    }
    if is_digits(&number) {
        let (Some(expected), Some(body)) = (as_u64(&key), as_u64(&format!("{siren}12"))) else {
            return false;
        };
        return expected == body % 97;
    }
    // one of the two key characters is a letter: the key is read as a
    // number in that alphabet instead
    let first = FR_ALPHABET.find(at(&number, 0).unwrap_or(' ')).unwrap_or(0) as i64;
    let second = FR_ALPHABET.find(at(&number, 1).unwrap_or(' ')).unwrap_or(0) as i64;
    let check = if at(&number, 0).is_some_and(|c| c.is_ascii_digit()) {
        first * 24 + second - 10
    } else {
        first * 34 + second - 100
    };
    let Some(body) = siren.parse::<i64>().ok() else {
        return false;
    };
    (body + 1 + check.div_euclid(11)).rem_euclid(11) == check.rem_euclid(11)
}

// ---------------------------------------------------------------------
// GB / XI — VAT registration number (stdnum.gb.vat)
// ---------------------------------------------------------------------

pub fn gb_compact(vat: &str) -> String {
    let cleaned = clean(vat, " -.");
    for prefix in ["GB", "XI"] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    cleaned
}

/// The nine-digit checksum, which lands on 0 or 42 (or 55 for the block
/// reissued from 100 000 000 on).
fn gb_checksum(number: &str) -> Option<i64> {
    weighted(number, &[8, 7, 6, 5, 4, 3, 2, 10, 1]).map(|sum| sum % 97)
}

pub fn gb_check(vat: &str) -> bool {
    let number = gb_compact(vat);
    match number.chars().count() {
        // a government department or a health authority
        5 => {
            let Some(value) = as_u64(&slice(&number, 2, 5)) else {
                return false;
            };
            (number.starts_with("GD") && value < 500) || (number.starts_with("HA") && value >= 500)
        }
        11 if matches!(slice(&number, 0, 6).as_str(), "GD8888" | "HA8888") => {
            let (Some(body), Some(check)) = (as_u64(&slice(&number, 6, 9)), as_u64(&tail(&number, 2)))
            else {
                return false;
            };
            let kind = (number.starts_with("GD") && body < 500)
                || (number.starts_with("HA") && body >= 500);
            kind && body % 97 == check
        }
        // the ordinary number, and the branch trader's, whose last three
        // digits are not part of the checksum
        9 | 12 => {
            if !is_digits(&number) {
                return false;
            }
            let Some(checksum) = gb_checksum(&slice(&number, 0, 9)) else {
                return false;
            };
            let restarted = as_u64(&slice(&number, 0, 3)).is_some_and(|head| head >= 100);
            if restarted {
                [0, 42, 55].contains(&checksum)
            } else {
                checksum == 0
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------
// GR / EL — Αριθμός Φορολογικού Μητρώου (stdnum.gr.vat)
// ---------------------------------------------------------------------

pub fn gr_compact(vat: &str) -> String {
    let cleaned = clean(vat, " -./:");
    let mut number = cleaned.clone();
    for prefix in ["EL", "GR"] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            number = rest.to_string();
            break;
        }
    }
    if number.chars().count() == 8 {
        // the old format had eight digits
        number = format!("0{number}");
    }
    number
}

/// `check_vat_gr` in `base_vat`: a handful of numbers the Greek EDI test
/// environment issues, which no checksum would accept.
const GR_TEST_NUMBERS: [&str; 5] = [
    "047747270",
    "047747210",
    "047747220",
    "117747270",
    "127747270",
];

pub fn gr_check(vat: &str) -> bool {
    let number = gr_compact(vat);
    if GR_TEST_NUMBERS.contains(&number.as_str()) {
        return true;
    }
    if number.chars().count() != 9 || !is_digits(&number) {
        return false;
    }
    let Some(values) = digits(&slice(&number, 0, 8)) else {
        return false;
    };
    let checksum = values.iter().fold(0i64, |acc, n| acc * 2 + i64::from(*n));
    ends_with_digit(&number, checksum * 2 % 11 % 10)
}

// ---------------------------------------------------------------------
// HR — Osobni identifikacijski broj (stdnum.hr.oib)
// ---------------------------------------------------------------------

pub fn hr_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "HR")
}

pub fn hr_check(vat: &str) -> bool {
    let number = hr_compact(vat);
    number.chars().count() == 11 && is_digits(&number) && mod_11_10_is_valid(&number)
}

// ---------------------------------------------------------------------
// HU — Közösségi adószám (stdnum.hu.anum, plus base_vat's own forms)
// ---------------------------------------------------------------------

pub fn hu_compact_plain(vat: &str) -> String {
    clean_prefixed(vat, " -", "HU")
}

/// A Hungarian company number, `xxxxxxxx-y-zz`, once the separators are
/// gone: eight digits, then a digit between 1 and 5, then two digits.
fn hu_is_company(number: &str) -> bool {
    number.chars().count() == 11
        && is_digits(number)
        && at(number, 8).is_some_and(|c| ('1'..='5').contains(&c))
}

/// `format_vat_hu`: the dashes go back in, because the Hungarian EDI
/// wants them and because the three parts mean different things.
pub fn hu_compact(vat: &str) -> String {
    let number = hu_compact_plain(vat);
    if hu_is_company(&number) {
        return format!(
            "{}-{}-{}",
            slice(&number, 0, 8),
            slice(&number, 8, 9),
            slice(&number, 9, 11)
        );
    }
    number
}

/// `check_vat_hu`: the community number, the company number and the
/// individual's tax code are all written in this field.
pub fn hu_check(vat: &str) -> bool {
    let number = hu_compact_plain(vat);
    let length = number.chars().count();
    if hu_is_company(&number) {
        return true;
    }
    // an individual's number starts with 8 and is ten digits long
    if length == 10 && is_digits(&number) && at(&number, 0) == Some('8') {
        return true;
    }
    // the community number is the first eight digits of the full one
    if length == 8 && is_digits(&number) {
        return true;
    }
    length == 8
        && is_digits(&number)
        && weighted(&number, &[9, 7, 3, 1, 9, 7, 3, 1]).is_some_and(|sum| sum % 10 == 0)
}

// ---------------------------------------------------------------------
// IE — Value added tax identification no. (stdnum.ie.vat)
// ---------------------------------------------------------------------

const IE_ALPHABET: &str = "WABCDEFGHIJKLMNOPQRSTUV";

pub fn ie_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "IE")
}

fn ie_check_char(body: &str) -> Option<char> {
    let padded = zfill(body, 7);
    let head = slice(&padded, 0, 7);
    let values = digits(&head)?;
    let trailing = slice(&padded, 7, padded.chars().count());
    // an empty tail counts as the first letter of the alphabet, which is
    // what `''.index()` answers in Python
    let extra = match trailing.chars().next() {
        None => 0,
        Some(c) => IE_ALPHABET.find(c)? as i64,
    };
    let total: i64 = values
        .iter()
        .enumerate()
        .map(|(i, n)| (8 - i as i64) * i64::from(*n))
        .sum::<i64>()
        + 9 * extra;
    IE_ALPHABET.chars().nth((total % 23) as usize)
}

pub fn ie_check(vat: &str) -> bool {
    let number = ie_compact(vat);
    let length = number.chars().count();
    if !(8..=9).contains(&length) {
        return false;
    }
    if !is_digits(&slice(&number, 0, 1)) || !is_digits(&slice(&number, 2, 7)) {
        return false;
    }
    if !slice(&number, 7, length)
        .chars()
        .all(|c| IE_ALPHABET.contains(c))
    {
        return false;
    }
    let head = slice(&number, 0, 7);
    let expected = if is_digits(&head) {
        // the new system: seven digits and one or two letters
        ie_check_char(&format!("{head}{}", slice(&number, 8, length)))
    } else if at(&number, 1).is_some_and(|c| c.is_ascii_uppercase() || c == '+' || c == '*') {
        // the old system, whose second character is a letter or a symbol
        ie_check_char(&format!("{}{}", slice(&number, 2, 7), slice(&number, 0, 1)))
    } else {
        return false;
    };
    expected.is_some() && expected == at(&number, 7)
}

// ---------------------------------------------------------------------
// IT — Partita IVA (stdnum.it.iva)
// ---------------------------------------------------------------------

pub fn it_compact(vat: &str) -> String {
    clean_prefixed(vat, " -:", "IT")
}

pub fn it_check(vat: &str) -> bool {
    let number = it_compact(vat);
    if number.chars().count() != 11 || !is_digits(&number) {
        return false;
    }
    if as_u64(&slice(&number, 0, 7)) == Some(0) {
        return false;
    }
    // the three digits in the middle name the province of residence
    let province = slice(&number, 7, 10);
    let known = (province.as_str() >= "001" && province.as_str() <= "100")
        || ["120", "121", "888", "999"].contains(&province.as_str());
    known && luhn_is_valid(&number)
}

// ---------------------------------------------------------------------
// LT — PVM mokėtojo kodas (stdnum.lt.pvm)
// ---------------------------------------------------------------------

pub fn lt_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "LT")
}

fn lt_check_digit(body: &str) -> Option<i64> {
    let values = digits(body)?;
    let mut check: i64 = values
        .iter()
        .enumerate()
        .map(|(i, n)| (1 + (i % 9) as i64) * i64::from(*n))
        .sum::<i64>()
        % 11;
    if check == 10 {
        // the weights shift by two and the sum is taken raw this time
        check = values
            .iter()
            .enumerate()
            .map(|(i, n)| (1 + ((i + 2) % 9) as i64) * i64::from(*n))
            .sum();
    }
    Some(check % 11 % 10)
}

pub fn lt_check(vat: &str) -> bool {
    let number = lt_compact(vat);
    if !is_digits(&number) {
        return false;
    }
    let length = number.chars().count();
    // the digit before the check digit says what kind of taxpayer it is
    let kind_ok = match length {
        9 => at(&number, 7) == Some('1'),
        12 => at(&number, 10) == Some('1'),
        _ => return false,
    };
    kind_ok
        && lt_check_digit(&slice(&number, 0, length - 1))
            .is_some_and(|check| ends_with_digit(&number, check))
}

// ---------------------------------------------------------------------
// LU — Numéro d'identification à la TVA (stdnum.lu.tva)
// ---------------------------------------------------------------------

pub fn lu_compact(vat: &str) -> String {
    clean_prefixed(vat, " :.-", "LU")
}

pub fn lu_check(vat: &str) -> bool {
    let number = lu_compact(vat);
    if number.chars().count() != 8 || !is_digits(&number) {
        return false;
    }
    as_u64(&slice(&number, 0, 6)).is_some_and(|body| format!("{:02}", body % 89) == tail(&number, 2))
}

// ---------------------------------------------------------------------
// LV — Pievienotās vērtības nodokļa numurs (stdnum.lv.pvn)
// ---------------------------------------------------------------------

pub fn lv_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "LV")
}

/// The check digit of a personal code.
fn lv_personal_check(number: &str) -> bool {
    let Some(sum) = weighted(number, &[10, 5, 8, 4, 2, 1, 6, 3, 7, 9]) else {
        return false;
    };
    ends_with_digit(number, (1 + sum) % 11 % 10)
}

pub fn lv_check(vat: &str) -> bool {
    let number = lv_compact(vat);
    if number.chars().count() != 11 || !is_digits(&number) {
        return false;
    }
    if at(&number, 0).is_some_and(|c| c > '3') {
        // a legal entity
        return weighted(&number, &[9, 1, 4, 8, 3, 10, 2, 5, 7, 6, 1])
            .is_some_and(|sum| sum % 11 == 3);
    }
    if number.starts_with("32") {
        // a personal code issued from July 2017 on, with no birth date
        return lv_personal_check(&number);
    }
    let (Some(dd), Some(mm), Some(yy), Some(century)) = (
        part(&number, 0, 2),
        part(&number, 2, 4),
        part(&number, 4, 6),
        part(&number, 6, 7),
    ) else {
        return false;
    };
    is_a_date((1800 + century * 100 + yy) as i32, mm as u32, dd as u32) && lv_personal_check(&number)
}

// ---------------------------------------------------------------------
// MT — Vat reg. no. (stdnum.mt.vat)
// ---------------------------------------------------------------------

pub fn mt_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "MT")
}

pub fn mt_check(vat: &str) -> bool {
    let number = mt_compact(vat);
    number.chars().count() == 8
        && is_digits(&number)
        && at(&number, 0) != Some('0')
        && weighted(&number, &[3, 4, 6, 7, 8, 9, 10, 1]).is_some_and(|sum| sum % 37 == 0)
}

// ---------------------------------------------------------------------
// NL — Btw-identificatienummer (stdnum.nl.btw)
// ---------------------------------------------------------------------

pub fn nl_compact(vat: &str) -> String {
    let number = clean_prefixed(vat, " -.", "NL");
    let length = number.chars().count();
    if length < 3 {
        return number;
    }
    // the number is a nine-digit BSN, the letter B and two more digits;
    // the BSN half is zero-filled, like `bsn.compact` does
    format!(
        "{}{}",
        zfill(&slice(&number, 0, length - 3), 9),
        slice(&number, length - 3, length)
    )
}

/// Burgerservicenummer, the citizen number the older btw numbers embed.
fn nl_bsn_check(number: &str) -> bool {
    if number.chars().count() != 9 || !is_digits(number) || as_u64(number) == Some(0) {
        return false;
    }
    let Some(values) = digits(number) else {
        return false;
    };
    let head: i64 = values[..8]
        .iter()
        .enumerate()
        .map(|(i, n)| (9 - i as i64) * i64::from(*n))
        .sum();
    (head - i64::from(values[8])).rem_euclid(11) == 0
}

pub fn nl_check(vat: &str) -> bool {
    let number = nl_compact(vat);
    if number.chars().count() != 12 || at(&number, 9) != Some('B') {
        return false;
    }
    let bsn = slice(&number, 0, 9);
    let branch = slice(&number, 10, 12);
    if as_u64(&bsn).is_none_or(|value| value == 0) || as_u64(&branch).is_none_or(|value| value == 0)
    {
        return false;
    }
    // the old numbers carry a BSN, the new ones a mod-97 checksum over
    // the whole thing including the country code
    nl_bsn_check(&bsn) || mod_97_10_is_valid(&format!("NL{number}"))
}

// ---------------------------------------------------------------------
// PL — Numer identyfikacji podatkowej (stdnum.pl.nip)
// ---------------------------------------------------------------------

pub fn pl_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "PL")
}

pub fn pl_check(vat: &str) -> bool {
    let number = pl_compact(vat);
    number.chars().count() == 10
        && is_digits(&number)
        && weighted(&number, &[6, 5, 7, 2, 3, 4, 5, 6, 7, -1])
            .is_some_and(|sum| sum.rem_euclid(11) == 0)
}

// ---------------------------------------------------------------------
// PT — Número de identificação fiscal (stdnum.pt.nif)
// ---------------------------------------------------------------------

pub fn pt_compact(vat: &str) -> String {
    clean_prefixed(vat, " -.", "PT")
}

pub fn pt_check(vat: &str) -> bool {
    let number = pt_compact(vat);
    if number.chars().count() != 9 || !is_digits(&number) || at(&number, 0) == Some('0') {
        return false;
    }
    let Some(values) = digits(&slice(&number, 0, 8)) else {
        return false;
    };
    let sum: i64 = values
        .iter()
        .enumerate()
        .map(|(i, n)| (9 - i as i64) * i64::from(*n))
        .sum();
    ends_with_digit(&number, (11 - sum).rem_euclid(11) % 10)
}

// ---------------------------------------------------------------------
// RO — Codul de identificare fiscală (stdnum.ro.cf, plus base_vat's own)
// ---------------------------------------------------------------------

pub fn ro_compact(vat: &str) -> String {
    // `ro.cf.compact` deliberately keeps the RO prefix
    clean(vat, " -")
}

/// CUI, the company identifier.
fn ro_cui_check(vat: &str) -> bool {
    let number = clean_prefixed(vat, " -", "RO");
    let length = number.chars().count();
    if !is_digits(&number) || at(&number, 0) == Some('0') || !(2..=10).contains(&length) {
        return false;
    }
    let body = zfill(&slice(&number, 0, length - 1), 9);
    let Some(sum) = weighted(&body, &[7, 5, 3, 2, 1, 7, 5, 3, 2]) else {
        return false;
    };
    ends_with_digit(&number, 10 * sum % 11 % 10)
}

/// The counties a CNP may name, `stdnum.ro.cnp`'s table reduced to the
/// codes: only membership is checked, never the name.
fn ro_county_exists(code: &str) -> bool {
    matches!(code, "51" | "52" | "70" | "80" | "81" | "82" | "83")
        || code
            .parse::<u32>()
            .is_ok_and(|value| (1..=48).contains(&value) && code.chars().count() == 2)
}

/// CNP, the personal code.
fn ro_cnp_check(number: &str) -> bool {
    if number.chars().count() != 13 || !is_digits(number) {
        return false;
    }
    let Some(first) = at(number, 0) else {
        return false;
    };
    if first == '0' {
        return false;
    }
    let century = match first {
        '1' | '2' => 1900,
        '3' | '4' => 1800,
        '5' | '6' => 2000,
        _ => 1900,
    };
    let (Some(yy), Some(mm), Some(dd)) = (part(number, 1, 3), part(number, 3, 5), part(number, 5, 7))
    else {
        return false;
    };
    if !is_a_date((century + yy) as i32, mm as u32, dd as u32) {
        return false;
    }
    if !ro_county_exists(&slice(number, 7, 9)) {
        return false;
    }
    let Some(sum) = weighted(number, &[2, 7, 9, 1, 4, 6, 3, 5, 8, 2, 7, 9]) else {
        return false;
    };
    let check = sum % 11;
    ends_with_digit(number, if check == 10 { 1 } else { check })
}

/// `check_vat_ro`: besides the company identifier, a Romanian partner may
/// carry a personal tax number — `xyyzzaabbxxxx`, or the `9000` series.
pub fn ro_check(vat: &str) -> bool {
    let number = ro_compact(vat);
    if ro_natural_person(&number) {
        return true;
    }
    let bare = clean_prefixed(&number, " -", "RO");
    match bare.chars().count() {
        13 => ro_cnp_check(&bare),
        2..=10 => ro_cui_check(&number),
        _ => false,
    }
}

/// `_check_tin1_ro_natural_persons` and `_check_tin2_ro_natural_persons`:
/// a date-shaped number, or one starting with `9000`.
///
/// Odoo matches these with `re.match`, which anchors at the start only —
/// so anything longer that begins the right way passes too, and the port
/// keeps that.
fn ro_natural_person(number: &str) -> bool {
    let head = slice(number, 0, 13);
    if head.chars().count() == 13 && is_digits(&head) {
        let month = slice(&head, 3, 5);
        let day = slice(&head, 5, 7);
        let first = at(&head, 0).unwrap_or('0');
        let month_ok = month.as_str() >= "01" && month.as_str() <= "12";
        let day_ok = day.as_str() >= "01" && day.as_str() <= "31";
        if first != '0' && month_ok && day_ok {
            return true;
        }
        if head.starts_with("9000") {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------
// SE — Momsregistreringsnummer (stdnum.se.vat)
// ---------------------------------------------------------------------

pub fn se_compact(vat: &str) -> String {
    clean_prefixed(vat, " -.", "SE")
}

pub fn se_check(vat: &str) -> bool {
    let number = se_compact(vat);
    if !is_digits(&number) || tail(&number, 2) != "01" {
        return false;
    }
    // what is left is the organisation number, ten digits closed by Luhn
    let orgnr = slice(&number, 0, number.chars().count() - 2);
    orgnr.chars().count() == 10 && luhn_is_valid(&orgnr)
}

// ---------------------------------------------------------------------
// SI — Davčna številka (stdnum.si.ddv)
// ---------------------------------------------------------------------

pub fn si_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "SI")
}

pub fn si_check(vat: &str) -> bool {
    let number = si_compact(vat);
    if number.chars().count() != 8 || !is_digits(&number) || number.starts_with('0') {
        return false;
    }
    let Some(sum) = weighted(&number, &[8, 7, 6, 5, 4, 3, 2]) else {
        return false;
    };
    let check = 11 - sum % 11;
    // 10 is written as 0; 11 has no single-digit spelling, so it fails
    ends_with_digit(&number, if check == 10 { 0 } else { check })
}

// ---------------------------------------------------------------------
// SK — Identifikačné číslo pre daň z pridanej hodnoty (stdnum.sk.dph)
// ---------------------------------------------------------------------

pub fn sk_compact(vat: &str) -> String {
    clean_prefixed(vat, " -", "SK")
}

pub fn sk_check(vat: &str) -> bool {
    let number = sk_compact(vat);
    if number.chars().count() != 10 || !is_digits(&number) {
        return false;
    }
    // it is unclear whether the birth number counts as a VAT number, and
    // stdnum accepts it, so this does too
    if birth_number_check(&number) {
        return true;
    }
    if at(&number, 0) == Some('0') || !matches!(at(&number, 2), Some('2' | '3' | '4' | '7' | '8' | '9'))
    {
        return false;
    }
    as_u64(&number).is_some_and(|value| value % 11 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each country's own reference number from `base_vat`'s `_ref_vat`,
    /// which is what the error message offers the user as an example: if
    /// one of these were refused the message would be telling them to
    /// type something the same code rejects.
    #[test]
    fn every_reference_number_passes_its_own_check() {
        type Case = (fn(&str) -> bool, &'static str);
        let cases: [Case; 26] = [
            (at_check, "ATU12345675"),
            (be_check, "BE0477472701"),
            (bg_check, "BG1234567892"),
            (cy_check, "CY10259033P"),
            (cz_check, "CZ12345679"),
            (de_check, "DE123456788"),
            (dk_check, "DK12345674"),
            (ee_check, "EE123456780"),
            (es_check, "ESA12345674"),
            (fi_check, "FI12345671"),
            (fr_check, "FR23334175221"),
            (gb_check, "GB123456782"),
            (gr_check, "EL123456783"),
            (hr_check, "HR01234567896"),
            (hu_check, "HU12345676"),
            (ie_check, "IE1234567FA"),
            (it_check, "IT12345670017"),
            (lt_check, "LT123456715"),
            (lu_check, "LU12345613"),
            (lv_check, "LV41234567891"),
            (mt_check, "MT12345634"),
            (nl_check, "NL123456782B90"),
            (pl_check, "PL1234567883"),
            (pt_check, "PT123456789"),
            (si_check, "SI12345679"),
            (sk_check, "SK2022749619"),
        ];
        for (check, number) in cases {
            assert!(check(number), "{number} is the example the user is given");
        }
    }

    #[test]
    fn a_wrong_check_digit_is_refused_everywhere() {
        assert!(!at_check("ATU12345674"));
        assert!(!be_check("BE0477472702"));
        assert!(!cy_check("CY10259033Q"));
        assert!(!de_check("DE136695978"));
        assert!(!dk_check("DK12345675"));
        assert!(!ee_check("EE123456781"));
        assert!(!es_check("ESA12345675"));
        assert!(!fi_check("FI12345672"));
        assert!(!fr_check("FR23334175222"));
        assert!(!gb_check("GB123456783"));
        assert!(!gr_check("EL123456784"));
        assert!(!hr_check("HR01234567897"));
        assert!(!ie_check("IE1234567GA"));
        assert!(!it_check("IT12345670018"));
        assert!(!lt_check("LT123456716"));
        assert!(!lu_check("LU12345614"));
        assert!(!lv_check("LV41234567892"));
        assert!(!mt_check("MT12345635"));
        // NL fica de fora: os dois últimos dígitos identificam a filial e
        // não entram no checksum do BSN, então `B91` é tão válido quanto
        // `B90` — ver o teste do módulo holandês abaixo
        assert!(!pl_check("PL1234567884"));
        assert!(!pt_check("PT123456780"));
        assert!(!si_check("SI12345670"));
        assert!(!sk_check("SK2022749610"));
    }

    #[test]
    fn the_dutch_number_is_checked_by_whichever_half_it_carries() {
        // `stdnum.nl.btw` accepts a number that passes EITHER the BSN
        // check on the first nine digits OR mod 97,10 over the whole
        // thing with the country code in front
        assert!(nl_check("NL123456782B90"));
        // the branch digits are outside the BSN checksum, so changing
        // them changes nothing — Odoo answers the same
        assert!(nl_check("NL123456782B91"));
        // the BSN itself is checked
        assert!(!nl_check("NL123456783B90"));
        // and the shape is: nine digits, a B, two more, none of them zero
        assert!(!nl_check("NL123456782B00"));
        assert!(!nl_check("NL123456782C90"));
    }

    #[test]
    fn the_separators_a_human_types_do_not_change_the_answer() {
        assert!(be_check("BE 0477.47.27.01"));
        assert_eq!(be_compact("BE 0477.47.27.01"), "0477472701");
        // the old nine-digit Belgian number gains its leading zero
        assert_eq!(be_compact("477472701"), "0477472701");
        assert_eq!(gr_compact("GR 123456783"), "123456783");
        assert_eq!(nl_compact("NL 1234.56782.B90"), "123456782B90");
    }

    #[test]
    fn germany_accepts_the_national_tax_number_too() {
        // a Steuernummer, which is not an intra-community VAT number
        assert!(de_check("201/123/12340"));
        assert_eq!(de_compact("201/123/12340"), "20112312340");
        // nine digits are read as a VAT number and must pass its checksum
        assert!(!de_check("136695978"));
        // an intra-community number, which is what Odoo's own test file
        // asserts (`base_vat/tests/test_vat_numbers.py::test_nif_de`)
        assert!(de_check("DE123456788"));
        // thirteen digits are the country-wide Steuernummer: it names a
        // state and carries a zero in the fifth position. Which digits
        // exactly the federal layout fixes is `stdnum.de.stnr`'s table,
        // which this build does not have to hand — so what is asserted
        // here is only what holds under any reading of it: a number
        // naming no state at all is refused.
        assert!(de_check("2801012345678"));
        assert!(!de_check("6801012345678"));
        assert!(!de_check("2801112345678"));
    }

    #[test]
    fn hungary_writes_a_company_number_back_with_its_dashes() {
        assert_eq!(hu_compact("HU12345678123"), "12345678-1-23");
        assert!(hu_check("12345678-1-23"));
        // an individual's number starts with 8
        assert!(hu_check("8071592153"));
        assert!(!hu_check("7071592153"));
    }

    #[test]
    fn romania_takes_a_company_id_or_a_personal_one() {
        assert!(ro_check("RO1234567897"));
        assert!(ro_check("1234567897"));
        // the two personal forms the module documents
        assert!(ro_check("8001011234567"));
        assert!(ro_check("9000123456789"));
        assert!(!ro_check("1234567890"));
    }

    #[test]
    fn the_united_kingdom_still_has_its_special_registrations() {
        // a government department, and a health authority
        assert!(gb_check("GD001"));
        assert!(gb_check("HA500"));
        assert!(!gb_check("GD500"));
        // a branch trader's three extra digits are outside the checksum
        assert!(gb_check("123456782000"));
    }

    #[test]
    fn greece_lets_its_edi_test_numbers_through() {
        assert!(gr_check("047747270"));
        assert!(!gr_check("047747271"));
    }
}

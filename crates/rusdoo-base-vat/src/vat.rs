//! Which arithmetic belongs to which country.
//!
//! Odoo asks `stdnum` for a module named after the country code and
//! falls back to its own `check_vat_xx`; here the same question is a
//! table, because a Rust build has no `getattr`. A country missing from
//! the table is a country this build cannot check — [`check_for`]
//! returns `None` and the caller accepts the number, which is what Odoo
//! does when neither source knows it.

pub mod checksum;
pub mod eu;
pub mod text;

/// What a number of this country looks like, for the sentence a refusal
/// prints. Odoo keeps the same list as `_ref_vat` in
/// `base_vat/models/res_partner.py`.
type Example = &'static str;

/// A country's three answers: is it valid, how is it stored, what does
/// one look like.
struct Country {
    /// the ISO code, and for two of them the VAT prefix as well
    code: &'static str,
    check: fn(&str) -> bool,
    compact: fn(&str) -> String,
    example: Example,
}

/// The twenty-seven member states and the United Kingdom.
///
/// Two codes are not ISO codes: Greece writes `EL` on its VAT numbers
/// and Northern Ireland writes `XI` on what are otherwise British ones
/// (`EU_EXTRA_VAT_CODES` in `odoo/addons/base/models/res_partner.py`).
/// Both spellings reach the same arithmetic, because both appear on real
/// invoices.
const COUNTRIES: &[Country] = &[
    Country { code: "AT", check: eu::at_check, compact: eu::at_compact, example: "ATU12345675" },
    Country { code: "BE", check: eu::be_check, compact: eu::be_compact, example: "BE0477472701" },
    Country { code: "BG", check: eu::bg_check, compact: eu::bg_compact, example: "BG1234567892" },
    Country { code: "CY", check: eu::cy_check, compact: eu::cy_compact, example: "CY10259033P" },
    Country { code: "CZ", check: eu::cz_check, compact: eu::cz_compact, example: "CZ12345679" },
    Country {
        code: "DE",
        check: eu::de_check,
        compact: eu::de_compact,
        example: "DE123456788 or 12/345/67890",
    },
    Country { code: "DK", check: eu::dk_check, compact: eu::dk_compact, example: "DK12345674" },
    Country { code: "EE", check: eu::ee_check, compact: eu::ee_compact, example: "EE123456780" },
    Country { code: "EL", check: eu::gr_check, compact: eu::gr_compact, example: "EL123456783" },
    Country { code: "ES", check: eu::es_check, compact: eu::es_compact, example: "ESA12345674" },
    Country { code: "FI", check: eu::fi_check, compact: eu::fi_compact, example: "FI12345671" },
    Country { code: "FR", check: eu::fr_check, compact: eu::fr_compact, example: "FR23334175221" },
    Country {
        code: "GB",
        check: eu::gb_check,
        compact: eu::gb_compact,
        example: "GB123456782 or XI123456782",
    },
    Country { code: "GR", check: eu::gr_check, compact: eu::gr_compact, example: "EL123456783" },
    Country { code: "HR", check: eu::hr_check, compact: eu::hr_compact, example: "HR01234567896" },
    Country {
        code: "HU",
        check: eu::hu_check,
        compact: eu::hu_compact,
        example: "HU12345676 or 12345678-1-11 or 8071592153",
    },
    Country { code: "IE", check: eu::ie_check, compact: eu::ie_compact, example: "IE1234567FA" },
    Country { code: "IT", check: eu::it_check, compact: eu::it_compact, example: "IT12345670017" },
    Country { code: "LT", check: eu::lt_check, compact: eu::lt_compact, example: "LT123456715" },
    Country { code: "LU", check: eu::lu_check, compact: eu::lu_compact, example: "LU12345613" },
    Country { code: "LV", check: eu::lv_check, compact: eu::lv_compact, example: "LV41234567891" },
    Country { code: "MT", check: eu::mt_check, compact: eu::mt_compact, example: "MT12345634" },
    Country { code: "NL", check: eu::nl_check, compact: eu::nl_compact, example: "NL123456782B90" },
    Country { code: "PL", check: eu::pl_check, compact: eu::pl_compact, example: "PL1234567883" },
    Country { code: "PT", check: eu::pt_check, compact: eu::pt_compact, example: "PT123456789" },
    Country {
        code: "RO",
        check: eu::ro_check,
        compact: eu::ro_compact,
        example: "RO1234567897 or 8001011234567 or 9000123456789",
    },
    Country { code: "SE", check: eu::se_check, compact: eu::se_compact, example: "SE123456789701" },
    Country { code: "SI", check: eu::si_check, compact: eu::si_compact, example: "SI12345679" },
    Country { code: "SK", check: eu::sk_check, compact: eu::sk_compact, example: "SK2022749619" },
    Country {
        code: "XI",
        check: eu::gb_check,
        compact: eu::gb_compact,
        example: "XI123456782",
    },
];

fn country_of(code: &str) -> Option<&'static Country> {
    let code = code.trim().to_uppercase();
    COUNTRIES.iter().find(|country| country.code == code)
}

/// The check `code` uses, or `None` when this build has none for it.
pub fn check_for(code: &str) -> Option<fn(&str) -> bool> {
    country_of(code).map(|country| country.check)
}

/// How `code` writes its numbers down.
pub fn compact_for(code: &str) -> Option<fn(&str) -> String> {
    country_of(code).map(|country| country.compact)
}

/// What a number of `code` looks like, for a refusal that teaches.
pub fn example_for(code: &str) -> Option<&'static str> {
    country_of(code).map(|country| country.example)
}

/// Every country this build can check, for whoever wants to say so on a
/// screen.
pub fn covered() -> impl Iterator<Item = &'static str> {
    COUNTRIES.iter().map(|country| country.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greece_and_northern_ireland_answer_to_both_names() {
        // EL is what a Greek invoice says; GR is what the country is
        assert!(check_for("EL").is_some_and(|check| check("EL123456783")));
        assert!(check_for("GR").is_some_and(|check| check("EL123456783")));
        // XI is a British number issued in Northern Ireland
        assert!(check_for("XI").is_some_and(|check| check("XI123456782")));
    }

    #[test]
    fn a_country_outside_the_table_has_no_arithmetic() {
        assert!(check_for("BR").is_none());
        assert!(example_for("BR").is_none());
    }

    #[test]
    fn the_lookup_does_not_care_how_the_code_was_typed() {
        assert!(check_for("be").is_some());
        assert!(check_for(" BE ").is_some());
    }

    #[test]
    fn every_country_can_answer_its_own_example() {
        // the example a refusal prints has to be a number that passes —
        // otherwise the message teaches the wrong thing
        for country in COUNTRIES {
            let first = country
                .example
                .split(" or ")
                .next()
                .expect("an example is never empty");
            assert!(
                (country.check)(first),
                "{}: the printed example {first:?} does not pass its own check",
                country.code
            );
        }
    }
}

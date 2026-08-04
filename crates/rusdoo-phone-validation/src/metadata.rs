//! The phone-number metadata: what a country's numbers look like, and
//! how they are written down.
//!
//! Odoo does not carry this data. It leans on the `phonenumbers` Python
//! package — Google's libphonenumber — and `phone_validation` only ships
//! *patches* to it, one file per region whose rules moved on faster than
//! the library did (`lib/phonenumbers_patch/region_*.py`).
//!
//! There is no libphonenumber in this workspace, so the table below is
//! the port's own. Two kinds of entry live here and the difference
//! matters when you read them:
//!
//! * the nine regions Odoo patches — BR, CI, CO, IL, KE, MA, MU, PA, SN —
//!   are transcribed **verbatim** from the vendored patch files, patterns
//!   and formats alike, including the appended Brazilian mobile-9 rule
//!   and the Mexican leading-1 rules from `phonenumbers_patch/__init__.py`.
//!   Those are the ones the addon exists for, and they are exact;
//! * the rest are the markets a port needs to be usable at all, written
//!   from libphonenumber's published metadata but **simplified**: the
//!   general shape and the mobile/fixed split are right, the long tail of
//!   premium/voip/pager ranges is folded away. A number outside the
//!   descriptions here is refused, so a simplification can only ever be
//!   stricter about what it *accepts*, never looser.
//!
//! Everything else on Earth is absent, and `phone_parse` says so rather
//! than guessing. See the crate docs for what that costs.

/// One way of writing a national number out, port of libphonenumber's
/// `NumberFormat`.
pub(crate) struct NumberFormat {
    /// groups the national number, e.g. `(\d{2})(\d{4})(\d{4})`
    pub pattern: &'static str,
    /// what to write instead, e.g. `${1} ${2}-${3}`
    pub format: &'static str,
    /// which numbers this rule is for, matched against the *start* of the
    /// national number; empty means any. Where libphonenumber lists
    /// several, this is the last — the most specific — as the library
    /// itself does.
    pub leading: &'static str,
    /// what replaces the first group when the number is written for
    /// dialling inside the country (`0${1}`, `(${1})`); empty means the
    /// groups are used as they are
    pub national_prefix_rule: &'static str,
}

/// A country's numbering plan, port of libphonenumber's `PhoneMetadata`.
pub(crate) struct Region {
    /// ISO 3166-1 alpha-2
    pub code: &'static str,
    /// the ITU calling code (32 for Belgium)
    pub country_code: u32,
    /// `main_country_for_code`: who answers for the calling code when
    /// several countries share it
    pub main: bool,
    /// what a caller dials before a national number, dropped on parsing
    pub national_prefix: &'static str,
    /// what a caller dials before a foreign number, as a regex; matching
    /// it makes the rest an international number, exactly as a `+` would
    pub international_prefix: &'static str,
    /// `general_desc.possible_length`: how many digits a national number
    /// of this country has. This is what tells "too short" from "too
    /// long" — the difference between "you dropped a digit" and "that is
    /// not a phone number".
    pub lengths: &'static [usize],
    /// `general_desc.national_number_pattern`: the shape a national
    /// number has before anyone asks what *kind* of number it is
    pub general: &'static str,
    /// the kinds — fixed line, mobile, toll free, and so on. A number is
    /// valid when it matches one of them.
    pub descs: &'static [&'static str],
    pub formats: &'static [NumberFormat],
    /// how the number is written when it is not a local call; empty means
    /// [`Region::formats`] is used with the national prefix rule dropped
    pub intl_formats: &'static [NumberFormat],
    /// which national numbers are this country's when several share a
    /// calling code (the NANP); empty means "whatever is left"
    pub area: &'static str,
}

macro_rules! number_format {
    ($pattern:expr, $format:expr) => {
        NumberFormat {
            pattern: $pattern,
            format: $format,
            leading: "",
            national_prefix_rule: "",
        }
    };
    ($pattern:expr, $format:expr, $leading:expr) => {
        NumberFormat {
            pattern: $pattern,
            format: $format,
            leading: $leading,
            national_prefix_rule: "",
        }
    };
    ($pattern:expr, $format:expr, $leading:expr, $national:expr) => {
        NumberFormat {
            pattern: $pattern,
            format: $format,
            leading: $leading,
            national_prefix_rule: $national,
        }
    };
}

/// The North American Numbering Plan: the United States, Canada and the
/// Caribbean share a plan and a calling code, so one shape serves both
/// entries and only the area code tells them apart.
const NANP_DESCS: &[&str] = &[r"[2-9]\d{2}[2-9]\d{6}"];
const NANP_FORMATS: &[NumberFormat] = &[number_format!(
    r"(\d{3})(\d{3})(\d{4})",
    "(${1}) ${2}-${3}",
    "",
    "${1}"
)];
const NANP_INTL_FORMATS: &[NumberFormat] =
    &[number_format!(r"(\d{3})(\d{3})(\d{4})", "${1}-${2}-${3}")];

/// Canada's area codes, as of the 2024 plan. Without them a Canadian
/// number reports itself as American: the two share every pattern, and
/// the area code is the only thing that differs.
const CA_AREA: &str = "(?:204|226|236|249|250|263|289|306|343|354|365|367|368|382|387|403|416|418|\
                       428|431|437|438|450|468|474|506|514|519|548|579|581|584|587|604|613|639|647|\
                       672|683|705|709|742|753|778|780|782|807|819|825|867|873|879|902|905)";

pub(crate) const REGIONS: &[Region] = &[
    // ---------------------------------------------------------------
    // North America
    // ---------------------------------------------------------------
    Region {
        code: "US",
        country_code: 1,
        main: true,
        national_prefix: "1",
        international_prefix: "011",
        lengths: &[10],
        general: r"[2-9]\d{9}",
        descs: NANP_DESCS,
        formats: NANP_FORMATS,
        intl_formats: NANP_INTL_FORMATS,
        area: "",
    },
    Region {
        code: "CA",
        country_code: 1,
        main: false,
        national_prefix: "1",
        international_prefix: "011",
        lengths: &[10],
        general: r"[2-9]\d{9}",
        descs: NANP_DESCS,
        formats: NANP_FORMATS,
        intl_formats: NANP_INTL_FORMATS,
        area: CA_AREA,
    },
    // ---------------------------------------------------------------
    // Europe
    // ---------------------------------------------------------------
    Region {
        code: "NL",
        country_code: 31,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[9],
        general: r"[1-9]\d{8}",
        descs: &[
            r"(?:1[0-35-8]|2[0346]|3[03-68]|4[0356]|5[0358]|7\d|8[458])\d{7}",
            r"6[1-58]\d{7}",
            r"800\d{4,7}",
            r"90[069]\d{4,7}",
        ],
        formats: &[
            number_format!(r"(\d{3})(\d{4,7})", "${1} ${2}", "[89]0", "0${1}"),
            number_format!(r"(\d)(\d{8})", "${1} ${2}", "6", "0${1}"),
            number_format!(r"(\d{2})(\d{3})(\d{4})", "${1} ${2} ${3}", "[1-57-9]", "0${1}"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "BE",
        country_code: 32,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[8, 9],
        general: r"4\d{8}|[1-9]\d{7}",
        descs: &[
            r"80[2-8]\d{5}|(?:1[0-69]|[23][2-8]|4[23]|5\d|6[013-57-9]|71|8[1-79]|9[2-4])\d{6}",
            r"4[5-9]\d{7}",
            r"800[1-9]\d{4}",
            r"(?:70[2-7]|90[0-9])\d{5}",
        ],
        formats: &[
            number_format!(
                r"(\d{3})(\d{2})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "4[5-9]",
                "0${1}"
            ),
            number_format!(
                r"(\d)(\d{3})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "[23]|4[23]|9[2-4]",
                "0${1}"
            ),
            number_format!(
                r"(\d{2})(\d{2})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "[15-8]",
                "0${1}"
            ),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "FR",
        country_code: 33,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[9],
        general: r"[1-9]\d{8}",
        descs: &[
            r"[1-5]\d{8}",
            r"(?:6\d|7[3-9])\d{7}",
            r"80[0-5]\d{6}",
            r"8(?:1[01]|2[0156]|84|9[0-37-9])\d{6}",
            r"9\d{8}",
        ],
        formats: &[
            number_format!(
                r"(\d)(\d{2})(\d{2})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4} ${5}",
                "[1-79]",
                "0${1}"
            ),
            number_format!(
                r"(\d{3})(\d{2})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "8",
                "0${1}"
            ),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "ES",
        country_code: 34,
        main: true,
        national_prefix: "",
        international_prefix: "00",
        lengths: &[9],
        general: r"[5-9]\d{8}",
        descs: &[r"[89]\d{8}", r"[67]\d{8}", r"5[0-9]\d{7}"],
        formats: &[number_format!(
            r"(\d{3})(\d{3})(\d{3})",
            "${1} ${2} ${3}",
            "[5-9]"
        )],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "IT",
        country_code: 39,
        main: true,
        national_prefix: "",
        international_prefix: "00",
        lengths: &[6, 7, 8, 9, 10, 11],
        general: r"[01389]\d{5,10}",
        descs: &[r"0\d{5,10}", r"3\d{8,9}", r"1\d{6,9}", r"8\d{7,9}"],
        formats: &[
            number_format!(r"(\d{2})(\d{3,4})(\d{4})", "${1} ${2} ${3}", "0[26]"),
            number_format!(r"(\d{3})(\d{3})(\d{3,4})", "${1} ${2} ${3}", "3"),
            number_format!(r"(\d{3})(\d{3,6})", "${1} ${2}", "0[13-57-9]"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "GB",
        country_code: 44,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[9, 10],
        general: r"[1-9]\d{8,9}",
        descs: &[
            r"(?:1\d{8,9}|2\d{9})",
            r"7(?:[1-3]\d{8}|[4-9]\d{8})",
            r"80[08]\d{6,7}",
            r"(?:3|5[56]|8[47]|9[018])\d{8}",
        ],
        formats: &[
            number_format!(r"(\d{4})(\d{6})", "${1} ${2}", "7[1-9]", "0${1}"),
            number_format!(
                r"(\d{2})(\d{4})(\d{4})",
                "${1} ${2} ${3}",
                "2|5[56]|7[06]",
                "0${1}"
            ),
            number_format!(
                r"(\d{3})(\d{3})(\d{4})",
                "${1} ${2} ${3}",
                "[1-9]",
                "0${1}"
            ),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "DE",
        country_code: 49,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[7, 8, 9, 10, 11],
        general: r"[1-9]\d{6,10}",
        descs: &[
            r"[2-9]\d{6,10}",
            r"1[5-7]\d{7,9}",
            r"800\d{4,8}",
            r"137[7-9]\d{6}|900(?:[135]\d{6}|9\d{7})",
        ],
        formats: &[
            number_format!(r"(\d{3})(\d{7,8})", "${1} ${2}", "1[5-7]", "0${1}"),
            number_format!(r"(\d{2})(\d{4,9})", "${1} ${2}", "3[02]|40|[68]9", "0${1}"),
            number_format!(r"(\d{3})(\d{4,8})", "${1} ${2}", "[1-9]", "0${1}"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "PT",
        country_code: 351,
        main: true,
        national_prefix: "",
        international_prefix: "00",
        lengths: &[9],
        general: r"[1-9]\d{8}",
        descs: &[
            r"2\d{8}",
            r"9[1236]\d{7}",
            r"80[02]\d{6}",
            r"7[0-24-9]\d{7}",
            r"6\d{8}",
        ],
        formats: &[
            number_format!(r"(\d{2})(\d{3})(\d{4})", "${1} ${2} ${3}", "2[12]"),
            number_format!(
                r"(\d{3})(\d{3})(\d{3})",
                "${1} ${2} ${3}",
                "(?:2[3-9]|[3-46-9])|8[01]"
            ),
        ],
        intl_formats: &[],
        area: "",
    },
    // ---------------------------------------------------------------
    // The nine regions Odoo patches — transcribed from
    // `lib/phonenumbers_patch/region_*.py`, verbatim.
    // ---------------------------------------------------------------
    Region {
        code: "CO",
        country_code: 57,
        main: true,
        national_prefix: "0",
        international_prefix: "00(?:4(?:[14]4|56)|[579])",
        lengths: &[10, 11],
        general: r"(?:60\d\d|9101)\d{6}|(?:1\d|3)\d{9}",
        descs: &[
            r"601055(?:[0-4]\d|50)\d\d|6010(?:[0-4]\d|5[0-4])\d{4}|60(?:[124-7][2-9]|8[1-9])\d{6}",
            r"333301[0-5]\d{3}|3333(?:00|2[5-9]|[3-9]\d)\d{4}|(?:3(?:24[1-9]|3(?:00|3[0-24-9]))|9101)\d{6}|3(?:0[0-5]|1\d|2[0-3]|5[01]|70)\d{7}",
            r"1800\d{7}",
            r"19(?:0[01]|4[78])\d{7}",
        ],
        formats: &[
            number_format!(r"(\d{3})(\d{7})", "${1} ${2}", "6", "(${1})"),
            number_format!(r"(\d{3})(\d{7})", "${1} ${2}", "3[0-357]|91"),
            number_format!(r"(\d)(\d{3})(\d{7})", "${1}-${2}-${3}", "1", "0${1}"),
        ],
        intl_formats: &[
            number_format!(r"(\d{3})(\d{7})", "${1} ${2}", "6"),
            number_format!(r"(\d{3})(\d{7})", "${1} ${2}", "3[0-357]|91"),
            number_format!(r"(\d)(\d{3})(\d{7})", "${1} ${2} ${3}", "1"),
        ],
        area: "",
    },
    Region {
        code: "BR",
        country_code: 55,
        main: true,
        national_prefix: "0",
        international_prefix: r"00(?:1[245]|2[1-35]|31|4[13]|[56]5|99)",
        lengths: &[8, 9, 10, 11],
        general: r"(?:[1-46-9]\d\d|5(?:[0-46-9]\d|5[0-46-9]))\d{8}|[1-9]\d{9}|[3589]\d{8}|[34]\d{7}",
        descs: &[
            r"(?:[14689][1-9]|2[12478]|3[1-578]|5[13-5]|7[13-579])[2-5]\d{7}",
            r"(?:[14689][1-9]|2[12478]|3[1-578]|5[13-5]|7[13-579])(?:7|9\d)\d{7}",
            r"800\d{6,7}",
            r"300\d{6}|[59]00\d{6,7}",
            r"(?:30[03]\d{3}|4(?:0(?:0\d|20)|370))\d{4}|300\d{5}",
        ],
        formats: &[
            number_format!(
                r"(\d{3,6})",
                "${1}",
                "1(?:1[25-8]|2[357-9]|3[02-68]|4[12568]|5|6[0-8]|8[015]|9[0-47-9])|321|610"
            ),
            number_format!(r"(\d{4})(\d{4})", "${1}-${2}", "4(?:02|37)0|[34]00"),
            number_format!(
                r"(\d{4})(\d{4})",
                "${1}-${2}",
                "[2357]|4(?:[0-24-9]|3(?:[0-689]|7[1-9]))"
            ),
            number_format!(
                r"(\d{3})(\d{2,3})(\d{4})",
                "${1} ${2} ${3}",
                "(?:[358]|90)0",
                "0${1}"
            ),
            number_format!(r"(\d{5})(\d{4})", "${1}-${2}", "9"),
            number_format!(
                r"(\d{2})(\d{4})(\d{4})",
                "${1} ${2}-${3}",
                "(?:[14689][1-9]|2[12478]|3[1-578]|5[13-5]|7[13-579])[2-57]",
                "(${1})"
            ),
            number_format!(
                r"(\d{2})(\d{5})(\d{4})",
                "${1} ${2}-${3}",
                "[16][1-9]|[2-57-9]",
                "(${1})"
            ),
        ],
        intl_formats: &[
            number_format!(r"(\d{4})(\d{4})", "${1}-${2}", "4(?:02|37)0|[34]00"),
            number_format!(r"(\d{3})(\d{2,3})(\d{4})", "${1} ${2} ${3}", "(?:[358]|90)0"),
            number_format!(
                r"(\d{2})(\d{4})(\d{4})",
                "${1} ${2}-${3}",
                "(?:[14689][1-9]|2[12478]|3[1-578]|5[13-5]|7[13-579])[2-57]"
            ),
            number_format!(r"(\d{2})(\d{5})(\d{4})", "${1} ${2}-${3}", "[16][1-9]|[2-57-9]"),
            // the patch itself (`phonenumbers_patch/__init__.py`): since
            // 2016 a Brazilian mobile carries a ninth digit, and a number
            // written the old way has to grow one. It is last on purpose —
            // a landline matches the rule above it and must not grow a 9.
            number_format!(
                r"(\d{2})(\d{4})(\d{4})",
                "${1} 9${2}-${3}",
                "(?:[14689][1-9]|2[12478]|3[1-578]|5[13-5]|7[13-579][689])"
            ),
        ],
        area: "",
    },
    Region {
        code: "MX",
        country_code: 52,
        main: true,
        national_prefix: "01",
        international_prefix: "0[09]",
        lengths: &[10, 11],
        general: r"1\d{10}|[2-9]\d{9}",
        descs: &[r"[2-9]\d{9}", r"1[2-9]\d{9}"],
        formats: &[
            number_format!(
                r"(\d{2})(\d{4})(\d{4})",
                "${1} ${2} ${3}",
                "33|5[56]|81",
                "0${1}"
            ),
            number_format!(
                r"(\d{3})(\d{3})(\d{4})",
                "${1} ${2} ${3}",
                "[2-9]",
                "0${1}"
            ),
        ],
        intl_formats: &[
            number_format!(r"(\d{2})(\d{4})(\d{4})", "${1} ${2} ${3}", "33|5[56]|81"),
            number_format!(r"(\d{3})(\d{3})(\d{4})", "${1} ${2} ${3}", "[2-9]"),
            // the patch (`phonenumbers_patch/__init__.py`): since 2019 a
            // Mexican mobile is dialled without the leading 1, and a
            // number still carrying one loses it here — which is why
            // `phone_parse` formats and re-parses instead of trusting the
            // first parse.
            number_format!(
                r"(\d)(\d{2})(\d{4})(\d{4})",
                "${2} ${3} ${4}",
                "1(?:33|5[56]|81)"
            ),
            number_format!(r"(\d)(\d{3})(\d{3})(\d{4})", "${2} ${3} ${4}", "1"),
        ],
        area: "",
    },
    Region {
        code: "IN",
        country_code: 91,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[10],
        general: r"[1-9]\d{9}",
        descs: &[r"[2-9]\d{9}"],
        formats: &[
            number_format!(r"(\d{5})(\d{5})", "${1} ${2}", "[6-9]", "0${1}"),
            number_format!(r"(\d{2})(\d{4})(\d{4})", "${1} ${2} ${3}", "[2-5]", "0${1}"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "MA",
        country_code: 212,
        main: true,
        national_prefix: "0",
        international_prefix: "00",
        lengths: &[9],
        general: r"[5-8]\d{8}",
        descs: &[
            r"5(?:2(?:[0-25-79]\d|3[1-578]|4[02-46-8]|8[0235-7])|3(?:[0-47]\d|5[02-9]|6[02-8]|8[014-9]|9[3-9])|(?:4[067]|5[03])\d)\d{5}",
            r"(?:6(?:[0-79]\d|8[0-247-9])|7(?:[0167]\d|2[0-4]|5[01]|8[0-3]))\d{6}",
            r"80[0-7]\d{6}",
            r"89\d{7}",
            r"(?:592(?:4[0-2]|93)|80[89]\d\d)\d{4}",
        ],
        formats: &[
            number_format!(
                r"(\d{3})(\d{2})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "5[45]",
                "0${1}"
            ),
            number_format!(
                r"(\d{4})(\d{5})",
                "${1}-${2}",
                r"5(?:2[2-46-9]|3[3-9]|9)|8(?:0[89]|92)",
                "0${1}"
            ),
            number_format!(r"(\d{2})(\d{7})", "${1}-${2}", "8", "0${1}"),
            number_format!(r"(\d{3})(\d{6})", "${1}-${2}", "[5-7]", "0${1}"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "SN",
        country_code: 221,
        main: true,
        national_prefix: "",
        international_prefix: "00",
        lengths: &[9],
        general: r"(?:[378]\d|93)\d{7}",
        descs: &[
            r"3(?:0(?:1[0-2]|80)|282|3(?:8[1-9]|9[3-9])|611)\d{5}",
            r"7(?:(?:[06-8]\d|21|90)\d|5(?:01|[19]0|25|[38]3|[4-7]\d))\d{5}",
            r"800\d{6}",
            r"88[4689]\d{6}",
            r"81[02468]\d{6}",
            r"(?:3(?:392|9[01]\d)\d|93(?:3[13]0|929))\d{4}",
        ],
        formats: &[
            number_format!(
                r"(\d{3})(\d{2})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "8"
            ),
            number_format!(
                r"(\d{2})(\d{3})(\d{2})(\d{2})",
                "${1} ${2} ${3} ${4}",
                "[379]"
            ),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "CI",
        country_code: 225,
        main: true,
        national_prefix: "",
        international_prefix: "00",
        lengths: &[10],
        general: r"[02]\d{9}",
        descs: &[
            r"2(?:[15]\d{3}|7(?:2(?:0[23]|1[2357]|[23][45]|4[3-5])|3(?:06|1[69]|[2-6]7)))\d{5}",
            r"0704[0-7]\d{5}|0(?:[15]\d\d|7(?:0[0-37-9]|[4-9][7-9]))\d{6}",
        ],
        formats: &[
            number_format!(r"(\d{2})(\d{2})(\d)(\d{5})", "${1} ${2} ${3} ${4}", "2"),
            number_format!(r"(\d{2})(\d{2})(\d{2})(\d{4})", "${1} ${2} ${3} ${4}", "0"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "MU",
        country_code: 230,
        main: true,
        national_prefix: "",
        international_prefix: r"0(?:0|[24-7]0|3[03])",
        lengths: &[7, 8, 10],
        general: r"(?:[57]|8\d\d)\d{7}|[2-468]\d{6}",
        descs: &[
            r"(?:2(?:[0346-8]\d|1[0-7])|4(?:[013568]\d|2[4-8])|54(?:[3-5]\d|71)|6\d\d|8(?:14|3[129]))\d{4}",
            r"5(?:4(?:2[1-389]|7[1-9])|87[15-8])\d{4}|(?:5(?:2[5-9]|4[3-689]|[57]\d|8[0-689]|9[0-8])|7(?:0[0-3]|3[013]))\d{5}",
            r"802\d{7}|80[0-2]\d{4}",
            r"30\d{5}",
            r"3(?:20|9\d)\d{4}",
        ],
        formats: &[
            number_format!(r"(\d{3})(\d{4})", "${1} ${2}", "[2-46]|8[013]"),
            number_format!(r"(\d{4})(\d{4})", "${1} ${2}", "[57]"),
            number_format!(r"(\d{5})(\d{5})", "${1} ${2}", "8"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "KE",
        country_code: 254,
        main: true,
        national_prefix: "0",
        international_prefix: "000",
        lengths: &[7, 8, 9, 10],
        general: r"(?:[17]\d\d|900)\d{6}|(?:2|80)0\d{6,7}|[4-6]\d{6,8}",
        descs: &[
            r"(?:4[245]|5[1-79]|6[01457-9])\d{5,7}|(?:4[136]|5[08]|62)\d{7}|(?:[24]0|66)\d{6,7}",
            r"(?:1(?:0[0-8]|1[0-7]|2[014]|30)|7\d\d)\d{6}",
            r"800[02-8]\d{5,6}",
            r"900[02-9]\d{5}",
        ],
        formats: &[
            number_format!(r"(\d{2})(\d{5,7})", "${1} ${2}", "[24-6]", "0${1}"),
            number_format!(r"(\d{3})(\d{6})", "${1} ${2}", "[17]", "0${1}"),
            number_format!(r"(\d{3})(\d{3})(\d{3,4})", "${1} ${2} ${3}", "[89]", "0${1}"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "PA",
        country_code: 507,
        main: true,
        national_prefix: "",
        international_prefix: "00",
        lengths: &[7, 8, 10, 11],
        general: r"(?:00800|8\d{3})\d{6}|[68]\d{7}|[1-57-9]\d{6}",
        descs: &[
            r"(?:1(?:0\d|1[479]|2[37]|3[0137]|4[17]|5[05]|6[58]|7[0167]|8[258]|9[1389])|2(?:[0235-79]\d|1[0-7]|4[013-9]|8[02-9])|3(?:[089]\d|1[0-7]|2[0-5]|33|4[0-79]|5[05]|6[068]|7[0-8])|4(?:00|3[0-579]|4\d|7[0-57-9])|5(?:[01]\d|2[0-7]|[56]0|79)|7(?:0[09]|2[0-26-8]|3[03]|4[04]|5[05-9]|6[056]|7[0-24-9]|8[6-9]|90)|8(?:09|2[89]|3\d|4[0-24-689]|5[014]|8[02])|9(?:0[5-9]|1[0135-8]|2[036-9]|3[35-79]|40|5[0457-9]|6[05-9]|7[04-9]|8[35-8]|9\d))\d{4}",
            r"(?:1[16]1|21[89]|6\d{3}|8(?:1[01]|7[23]))\d{4}",
            r"800\d{4,5}|(?:00800|800\d)\d{6}",
            r"(?:8(?:22|55|60|7[78]|86)|9(?:00|81))\d{4}",
        ],
        formats: &[
            number_format!(r"(\d{3})(\d{4})", "${1}-${2}", "[1-57-9]"),
            number_format!(r"(\d{4})(\d{4})", "${1}-${2}", "[68]"),
            number_format!(r"(\d{3})(\d{3})(\d{4})", "${1} ${2} ${3}", "8"),
        ],
        intl_formats: &[],
        area: "",
    },
    Region {
        code: "IL",
        country_code: 972,
        main: true,
        national_prefix: "0",
        international_prefix: r"0(?:0|1[2-9])",
        lengths: &[7, 8, 9, 10, 11, 12],
        general: r"1\d{6}(?:\d{3,5})?|[57]\d{8}|[1-489]\d{7}",
        descs: &[
            r"153\d{8,9}|29[1-9]\d{5}|(?:2[0-8]|[3489]\d)\d{6}",
            r"55410\d{4}|5(?:(?:[02][02-9]|[149][2-9]|[36]\d|8[3-7])\d|5(?:01|2\d|3[0-3]|4[34]|5[0-25689]|6[6-8]|7[0-267]|8[7-9]|9[1-9]))\d{5}",
            r"1(?:255|80[019]\d{3})\d{3}",
            r"1212\d{4}|1(?:200|9(?:0[0-2]|19))\d{6}",
            r"1700\d{6}",
            r"7(?:38(?:0\d|5[0-259]|88)|8(?:33|55|77|81)\d)\d{4}|7(?:18|2[23]|3[237]|47|6[258]|7\d|82|9[2-9])\d{6}",
            r"1599\d{6}",
            r"151\d{8,9}",
        ],
        formats: &[
            number_format!(r"(\d{4})(\d{3})", "${1}-${2}", "125"),
            number_format!(r"(\d{4})(\d{2})(\d{2})", "${1}-${2}-${3}", "121"),
            number_format!(r"(\d)(\d{3})(\d{4})", "${1}-${2}-${3}", "[2-489]", "0${1}"),
            number_format!(r"(\d{2})(\d{3})(\d{4})", "${1}-${2}-${3}", "[57]", "0${1}"),
            number_format!(r"(\d{4})(\d{3})(\d{3})", "${1}-${2}-${3}", "12"),
            number_format!(r"(\d{4})(\d{6})", "${1}-${2}", "159"),
            number_format!(r"(\d)(\d{3})(\d{3})(\d{3})", "${1}-${2}-${3}-${4}", "1[7-9]"),
            number_format!(
                r"(\d{3})(\d{1,2})(\d{3})(\d{4})",
                "${1}-${2} ${3}-${4}",
                "15"
            ),
        ],
        intl_formats: &[],
        area: "",
    },
];

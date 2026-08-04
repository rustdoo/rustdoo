//! The stub: the summary of paid bills printed next to the check.
//!
//! Port of `_check_make_stub_pages` and `_check_get_pages`. The part
//! ported here is the part that is pure — how the lines are laid out over
//! pages — and it is where the module's only real subtlety lives: a
//! company that does not want a multi-page stub gets it cropped with a
//! line left over for the ellipsis, and one that does must not have a
//! section header land as the last line of a page.

use serde_json::{json, Map, Value};

/// `INV_LINES_PER_STUB` — how many bills fit beside one check.
pub const INV_LINES_PER_STUB: usize = 9;

/// A line is a section header ("Bills", "Refunds") rather than a bill.
fn is_header(line: &Value) -> bool {
    line.get("header").and_then(Value::as_bool).unwrap_or(false)
}

/// Lay the stub lines out over pages.
///
/// With `multi_stub` off there is exactly one page, cropped — and when
/// there was more than fits, one line short of the limit, because the
/// template puts an ellipsis there to say the list goes on.
///
/// With it on the lines are split, and a page that would end on a section
/// header gives that header up to the next one: a "Refunds" heading alone
/// at the foot of a page announces nothing.
pub fn paginate(lines: Vec<Value>, multi_stub: bool) -> Vec<Vec<Value>> {
    if !multi_stub {
        let room = if lines.len() > INV_LINES_PER_STUB {
            INV_LINES_PER_STUB - 1
        } else {
            INV_LINES_PER_STUB
        };
        return vec![lines.into_iter().take(room).collect()];
    }
    let mut pages: Vec<Vec<Value>> = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let last_on_page = start + INV_LINES_PER_STUB - 1;
        let room = if lines.len() > last_on_page && is_header(&lines[last_on_page]) {
            INV_LINES_PER_STUB - 1
        } else {
            INV_LINES_PER_STUB
        };
        let end = (start + room).min(lines.len());
        pages.push(lines[start..end].to_vec());
        start = end;
    }
    pages
}

/// Port of `_check_get_pages`: one entry per page, with the check itself
/// only valid on the first one.
///
/// `header` is what the whole payment says (the number, the date, who it
/// is for); the amount and the amount in words are blanked to `VOID` on
/// every page after the first, which is the reason this function exists
/// at all — a stub that spills must not look like several checks.
pub fn build_pages(header: &Map<String, Value>, stub_pages: Vec<Vec<Value>>) -> Vec<Value> {
    // a payment with no bills behind it still prints one page: there is a
    // check to print, only nothing to itemize
    let stub_pages = if stub_pages.is_empty() {
        vec![Vec::new()]
    } else {
        stub_pages
    };
    stub_pages
        .into_iter()
        .enumerate()
        .map(|(index, lines)| {
            let mut page = header.clone();
            if index > 0 {
                page.insert("amount".into(), json!("VOID"));
                page.insert("amount_in_word".into(), json!("VOID"));
            }
            page.insert("stub_lines".into(), Value::Array(lines));
            Value::Object(page)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bill(number: usize) -> Value {
        json!({ "number": format!("BILL/{number:05}") })
    }

    fn header(name: &str) -> Value {
        json!({ "header": true, "name": name })
    }

    fn bills(count: usize) -> Vec<Value> {
        (0..count).map(bill).collect()
    }

    #[test]
    fn a_single_page_stub_is_cropped_with_room_for_the_ellipsis() {
        // exactly what fits stays whole
        let pages = paginate(bills(INV_LINES_PER_STUB), false);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].len(), INV_LINES_PER_STUB);

        // one more and the last line is given up to the ellipsis
        let pages = paginate(bills(INV_LINES_PER_STUB + 1), false);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].len(), INV_LINES_PER_STUB - 1);
    }

    #[test]
    fn a_short_stub_fits_on_one_page_either_way() {
        for multi_stub in [false, true] {
            let pages = paginate(bills(3), multi_stub);
            assert_eq!(pages.len(), 1);
            assert_eq!(pages[0].len(), 3);
        }
    }

    #[test]
    fn a_multi_page_stub_splits_and_keeps_every_line() {
        let pages = paginate(bills(INV_LINES_PER_STUB + 1), true);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].len(), INV_LINES_PER_STUB);
        assert_eq!(pages[1].len(), 1);
        assert_eq!(pages.iter().map(Vec::len).sum::<usize>(), INV_LINES_PER_STUB + 1);
    }

    #[test]
    fn a_section_header_never_ends_a_page_alone() {
        // eight bills, then the "Refunds" heading: it would land last
        let mut lines = bills(INV_LINES_PER_STUB - 1);
        lines.push(header("Refunds"));
        lines.extend(bills(3));

        let pages = paginate(lines, true);
        assert_eq!(pages[0].len(), INV_LINES_PER_STUB - 1);
        assert!(
            !is_header(pages[0].last().unwrap()),
            "a heading at the foot of a page announces nothing"
        );
        assert!(is_header(&pages[1][0]), "it opens the next page instead");
    }

    #[test]
    fn an_empty_stub_still_prints_one_check() {
        let pages = paginate(Vec::new(), true);
        assert!(pages.is_empty(), "no lines means no stub pages");
        let printed = build_pages(&Map::new(), pages);
        assert_eq!(printed.len(), 1, "but there is still a check to print");
        assert_eq!(printed[0]["stub_lines"], json!([]));
    }

    #[test]
    fn only_the_first_page_carries_the_amount() {
        let mut head = Map::new();
        head.insert("amount".into(), json!("$ 100.00"));
        head.insert("amount_in_word".into(), json!("One Hundred and 00/100"));
        head.insert("sequence_number".into(), json!("00042"));

        let pages = build_pages(&head, paginate(bills(INV_LINES_PER_STUB + 1), true));
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0]["amount"], "$ 100.00");
        // the second page is a stub, not a second check
        assert_eq!(pages[1]["amount"], "VOID");
        assert_eq!(pages[1]["amount_in_word"], "VOID");
        // and it still says which check it belongs to
        assert_eq!(pages[1]["sequence_number"], "00042");
    }
}

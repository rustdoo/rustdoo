//! A `.po` reader, the format every Odoo translation travels in.
//!
//! An addon ships `i18n/pt_BR.po` with pairs of `msgid` (the text in
//! the source language) and `msgstr` (the translation). The `#:`
//! comments say where each text came from — a field label, a view
//! string, a message in code — and they are how one knows *what* is
//! being translated.
//!
//! What is read here is deliberately the subset that matters: id,
//! translation, and the references. Plural forms and contexts
//! (`msgctxt`) stay out — Odoo itself barely uses them, and a reader
//! pretending to understand them would get it wrong in silence.
//!
//! An entry with an empty `msgstr` is not a translation: it is a text
//! nobody has translated yet, and keeping it would make the screen show
//! blank where it should show the original.

use std::collections::HashMap;

/// One entry of the file: the source text, the translation, and where
/// it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub msgid: String,
    pub msgstr: String,
    /// the `#:` lines — `model:ir.model.fields,field_description:...`,
    /// `code:addons/...`, `model_terms:ir.ui.view,arch_db:...`
    pub references: Vec<String>,
    /// the `#.` lines — the extracted comments, which is where the
    /// generator says which half of the program a text belongs to
    /// (`odoo-javascript`, `odoo-python`) and which module shipped it
    pub comments: Vec<String>,
}

impl Entry {
    /// Whether this text belongs to the client rather than the server.
    ///
    /// `/web/webclient/translations` serves the browser only what the
    /// browser can use: Odoo marks those with `#. odoo-javascript`
    /// (`JAVASCRIPT_TRANSLATION_COMMENT` in `odoo/tools/translate.py`),
    /// and sending the rest would mean shipping the whole server's
    /// vocabulary on every page load.
    pub fn is_javascript(&self) -> bool {
        self.comments
            .iter()
            .any(|comment| comment == "odoo-javascript")
    }

    /// If this entry translates a field's label, the pair
    /// `(model, field)` it names.
    ///
    /// The reference is `module.field_<table>__<field>`, with the table
    /// in snake_case — which is the model's name with its dots turned
    /// into underscores.
    pub fn field_label(&self) -> Option<(String, String)> {
        // every reference is examined, not only the first: a real Odoo
        // entry has several, and the label "Name" turns up with four
        // `#:` lines, any of which may be the field one
        self.references.iter().find_map(|reference| {
            let rest = reference.strip_prefix("model:ir.model.fields,field_description:")?;
            let (_module, ident) = rest.split_once('.')?;
            let table = ident.strip_prefix("field_")?;
            let (table, field) = table.rsplit_once("__")?;
            Some((table.replace('_', "."), field.to_string()))
        })
    }

    /// If this entry translates a value *stored on a record*, the
    /// `(model, field, external id)` it names.
    ///
    /// Half of what an addon ships in `i18n/` is not program text but
    /// data: the names of countries, of payment methods, of menus, of
    /// actions. The reference for one is
    /// `model:<model>,<field>:<module>.<id>`, which says everything
    /// needed to find the row and the column.
    ///
    /// `ir.model.fields` is excluded and is the one exception worth
    /// stating: a field there is not a row in this port — fields are
    /// declared in code — so its entries are labels, and
    /// [`Entry::field_label`] is what reads those.
    pub fn record_value(&self) -> Option<(String, String, String)> {
        self.references.iter().find_map(|reference| {
            let rest = reference.strip_prefix("model:")?;
            let (model, rest) = rest.split_once(',')?;
            if model == "ir.model.fields" || model == "ir.model" {
                return None;
            }
            let (field, xml_id) = rest.split_once(':')?;
            // `model_terms:` references have their own shape and their
            // own mechanism (terms inside a view's arch); this is not it
            if field.is_empty() || xml_id.is_empty() {
                return None;
            }
            Some((model.to_string(), field.to_string(), xml_id.to_string()))
        })
    }
}

/// One language's translations: from the source text to the translated
/// one.
pub type Catalogue = HashMap<String, String>;

/// Read a `.po`, answering the entries that carry a translation.
///
/// The header (the `msgid ""` entry) is dropped: it holds the file's
/// metadata, not a translation.
pub fn parse_po(source: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut references: Vec<String> = Vec::new();
    let mut comments: Vec<String> = Vec::new();
    let mut msgid: Option<String> = None;
    let mut msgstr: Option<String> = None;
    // which field the loose `"..."` lines continue
    let mut current: Option<Field> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(
                &mut entries,
                &mut msgid,
                &mut msgstr,
                &mut references,
                &mut comments,
            );
            current = None;
            continue;
        }
        if let Some(reference) = line.strip_prefix("#:") {
            references.push(reference.trim().to_string());
            continue;
        }
        if let Some(comment) = line.strip_prefix("#.") {
            comments.push(comment.trim().to_string());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("msgid ") {
            // a new msgid with no blank line before it closes the last
            if msgstr.is_some() {
                flush(
                    &mut entries,
                    &mut msgid,
                    &mut msgstr,
                    &mut references,
                    &mut comments,
                );
            }
            msgid = Some(unquote(rest));
            current = Some(Field::Id);
            continue;
        }
        if let Some(rest) = line.strip_prefix("msgstr ") {
            msgstr = Some(unquote(rest));
            current = Some(Field::Str);
            continue;
        }
        if line.starts_with('"') {
            let piece = unquote(line);
            match current {
                Some(Field::Id) => msgid.get_or_insert_with(String::new).push_str(&piece),
                Some(Field::Str) => msgstr.get_or_insert_with(String::new).push_str(&piece),
                None => {}
            }
            continue;
        }
        // msgctxt, msgid_plural, msgstr[n]: outside the subset
        current = None;
    }
    flush(
        &mut entries,
        &mut msgid,
        &mut msgstr,
        &mut references,
        &mut comments,
    );
    entries
}

/// A `.po`'s translations as a catalogue ready to be looked up.
pub fn catalogue(source: &str) -> Catalogue {
    parse_po(source)
        .into_iter()
        .map(|entry| (entry.msgid, entry.msgstr))
        .collect()
}

enum Field {
    Id,
    Str,
}

fn flush(
    entries: &mut Vec<Entry>,
    msgid: &mut Option<String>,
    msgstr: &mut Option<String>,
    references: &mut Vec<String>,
    comments: &mut Vec<String>,
) {
    let (Some(id), Some(text)) = (msgid.take(), msgstr.take()) else {
        references.clear();
        comments.clear();
        return;
    };
    // the header, and what nobody has translated yet
    if !id.is_empty() && !text.is_empty() {
        entries.push(Entry {
            msgid: id,
            msgstr: text,
            references: std::mem::take(references),
            comments: std::mem::take(comments),
        });
    } else {
        references.clear();
        comments.clear();
    }
}

/// The content of a `"..."` line, with the escapes undone.
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            // an escape we do not know comes back as it went in,
            // instead of disappearing: losing a character of a
            // translation is worse than showing a backslash
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# Translation of Odoo Server.
msgid ""
msgstr ""
"Project-Id-Version: Odoo Server 19.0\n"
"Language: pt_BR\n"

#. module: product
#. odoo-python
#: code:addons/product/models/product_product.py:0
#: model:ir.model.fields,field_description:product.field_product_product__name
#: model:ir.model.fields,field_description:product.field_product_template__name
msgid "Name"
msgstr "Nome"

#. module: product
#: code:addons/product/models/product_product.py:0
msgid ""
"\n"
"Note: products you cannot see are not shown."
msgstr ""
"\n"
"Note: products you cannot see are not shown."

#. module: product
#: model:ir.model.fields,field_description:product.field_product_product__list_price
msgid "Sales Price"
msgstr ""

#: model:ir.ui.view,arch_db:product.view_form
msgid "Quotation \"draft\""
msgstr "Cotação \"rascunho\""
"#;

    #[test]
    fn the_header_is_not_a_translation() {
        let entries = parse_po(SAMPLE);
        assert!(
            entries.iter().all(|e| !e.msgid.is_empty()),
            "the header has an empty msgid and translates nothing"
        );
    }

    #[test]
    fn an_untranslated_entry_is_left_out() {
        let entries = parse_po(SAMPLE);
        assert!(
            !entries.iter().any(|e| e.msgid == "Sales Price"),
            "an empty msgstr is 'nobody translated it yet', not 'translate to empty'"
        );
    }

    #[test]
    fn a_multiline_entry_is_joined_in_order() {
        let entries = parse_po(SAMPLE);
        let entry = entries
            .iter()
            .find(|e| e.msgid.starts_with('\n'))
            .expect("a entrada multilinha");
        assert_eq!(entry.msgid, "\nNote: products you cannot see are not shown.");
        assert_eq!(
            entry.msgstr,
            "\nNote: products you cannot see are not shown."
        );
    }

    #[test]
    fn escapes_come_back_as_characters() {
        let entries = parse_po(SAMPLE);
        let entry = entries
            .iter()
            .find(|e| e.msgid.contains("Quotation"))
            .unwrap();
        assert_eq!(entry.msgid, r#"Quotation "draft""#);
        assert_eq!(entry.msgstr, r#"Cotação "rascunho""#);
    }

    #[test]
    fn a_field_label_entry_names_its_model_and_field() {
        let entries = parse_po(SAMPLE);
        let entry = entries.iter().find(|e| e.msgid == "Name").unwrap();
        assert_eq!(
            entry.field_label(),
            Some(("product.product".into(), "name".into()))
        );
        // and an entry from code is no field's label
        let code = entries
            .iter()
            .find(|e| e.msgid.starts_with('\n'))
            .unwrap();
        assert_eq!(code.field_label(), None);
    }

    #[test]
    fn the_catalogue_maps_source_to_translation() {
        let map = catalogue(SAMPLE);
        assert_eq!(map.get("Name").map(String::as_str), Some("Nome"));
        assert_eq!(map.get("Sales Price"), None);
    }
}

//! Leitor de `.po`, o formato em que toda tradução do Odoo viaja.
//!
//! Um addon traz `i18n/pt_BR.po` com pares de `msgid` (o texto na língua
//! de origem) e `msgstr` (a tradução). Os comentários `#:` dizem de onde
//! cada texto veio — um rótulo de campo, uma string de view, uma mensagem
//! de código — e é por eles que se sabe *o que* está sendo traduzido.
//!
//! O que é lido aqui é deliberadamente o subconjunto que importa: id,
//! tradução, e as referências. Formas plurais e contextos (`msgctxt`)
//! ficam de fora — o Odoo mesmo quase não os usa, e um leitor que
//! fingisse entendê-los erraria em silêncio.
//!
//! Uma entrada com `msgstr` vazio não é uma tradução: é um texto que
//! alguém ainda não traduziu, e guardá-la faria a tela mostrar vazio
//! onde deveria mostrar o original.

use std::collections::HashMap;

/// Uma entrada do arquivo: o texto de origem, a tradução e de onde veio.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub msgid: String,
    pub msgstr: String,
    /// as linhas `#:` — `model:ir.model.fields,field_description:...`,
    /// `code:addons/...`, `model_terms:ir.ui.view,arch_db:...`
    pub references: Vec<String>,
}

impl Entry {
    /// Se esta entrada traduz o rótulo de um campo, o par
    /// `(modelo, campo)` que ela nomeia.
    ///
    /// A referência é `module.field_<tabela>__<campo>`, com a tabela em
    /// snake_case — que é o nome do modelo com pontos virados sublinhado.
    pub fn field_label(&self) -> Option<(String, String)> {
        // toda referência é examinada, não só a primeira: uma entrada
        // real do Odoo tem várias, e o rótulo "Name" aparece com quatro
        // linhas `#:` das quais qualquer uma pode ser a de campo
        self.references.iter().find_map(|reference| {
            let rest = reference.strip_prefix("model:ir.model.fields,field_description:")?;
            let (_module, ident) = rest.split_once('.')?;
            let table = ident.strip_prefix("field_")?;
            let (table, field) = table.rsplit_once("__")?;
            Some((table.replace('_', "."), field.to_string()))
        })
    }
}

/// As traduções de um idioma: do texto de origem para o traduzido.
pub type Catalogue = HashMap<String, String>;

/// Lê um `.po`, devolvendo as entradas que carregam tradução.
///
/// O cabeçalho (a entrada de `msgid ""`) é descartado: ele guarda
/// metadados do arquivo, não uma tradução.
pub fn parse_po(source: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut references: Vec<String> = Vec::new();
    let mut msgid: Option<String> = None;
    let mut msgstr: Option<String> = None;
    // em qual campo as linhas soltas `"..."` continuam
    let mut current: Option<Field> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut entries, &mut msgid, &mut msgstr, &mut references);
            current = None;
            continue;
        }
        if let Some(reference) = line.strip_prefix("#:") {
            references.push(reference.trim().to_string());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("msgid ") {
            // um novo msgid sem linha em branco antes fecha o anterior
            if msgstr.is_some() {
                flush(&mut entries, &mut msgid, &mut msgstr, &mut references);
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
        // msgctxt, msgid_plural, msgstr[n]: fora do subconjunto
        current = None;
    }
    flush(&mut entries, &mut msgid, &mut msgstr, &mut references);
    entries
}

/// As traduções de um `.po` como um catálogo pronto para consulta.
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
) {
    let (Some(id), Some(text)) = (msgid.take(), msgstr.take()) else {
        references.clear();
        return;
    };
    // o cabeçalho, e o que ninguém traduziu ainda
    if !id.is_empty() && !text.is_empty() {
        entries.push(Entry {
            msgid: id,
            msgstr: text,
            references: std::mem::take(references),
        });
    } else {
        references.clear();
    }
}

/// O conteúdo de uma linha `"..."`, com os escapes desfeitos.
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
            // um escape que não conhecemos volta como veio, em vez de
            // sumir: perder um caractere de uma tradução é pior que
            // mostrar uma barra invertida
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
"Nota: produtos que você não pode ver não aparecem."

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
            "o cabeçalho tem msgid vazio e não é tradução de nada"
        );
    }

    #[test]
    fn an_untranslated_entry_is_left_out() {
        let entries = parse_po(SAMPLE);
        assert!(
            !entries.iter().any(|e| e.msgid == "Sales Price"),
            "msgstr vazio é 'ninguém traduziu ainda', não 'traduza para vazio'"
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
            "\nNota: produtos que você não pode ver não aparecem."
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
        // e uma entrada de código não é rótulo de campo nenhum
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

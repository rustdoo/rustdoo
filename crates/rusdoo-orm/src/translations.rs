//! The catalogue of code-text translations, a port of what Odoo
//! carrega dos `.po` de cada addon (`odoo/tools/translate.py`).
//!
//! Two things translate in an Odoo, and only one of them lives in the
//! database. A record's *value* — a product's name — is a `jsonb`
//! column,
//! porque muda de registro para registro. O *texto do programa* — o
//! the "Create Date" label, an error message — is the same for
//! everyone, comes from the module's `.po` and lives in the server's
//! memory.
//!
//! Keeping the second in the database would be one query per label per
//! screen.

use std::collections::HashMap;

/// The translations that were loaded: language -> (source -> target).
#[derive(Debug, Default, Clone)]
pub struct Translations {
    by_lang: HashMap<String, HashMap<String, String>>,
}

impl Translations {
    pub fn new() -> Translations {
        Translations::default()
    }

    /// Add what a `.po` brought. Modules load in dependency order, and
    /// the last one to speak about a text wins — which is how one module
    /// corrects another's label.
    pub fn extend(&mut self, lang: &str, entries: impl IntoIterator<Item = (String, String)>) {
        let catalogue = self.by_lang.entry(lang.to_string()).or_default();
        catalogue.extend(entries);
    }

    /// The text in `lang`, or itself when nobody translated it.
    ///
    /// Never empty and never absent: an untranslated label shows in the
    /// source language, which is information, while a blank label is a
    /// column the user cannot read.
    pub fn get<'a>(&'a self, lang: &str, source: &'a str) -> &'a str {
        self.by_lang
            .get(lang)
            .and_then(|catalogue| catalogue.get(source))
            .map(String::as_str)
            .unwrap_or(source)
    }

    pub fn is_empty(&self) -> bool {
        self.by_lang.is_empty()
    }

    /// The languages that have any translation loaded.
    pub fn langs(&self) -> Vec<&str> {
        let mut langs: Vec<&str> = self.by_lang.keys().map(String::as_str).collect();
        langs.sort_unstable();
        langs
    }

    pub fn len_for(&self, lang: &str) -> usize {
        self.by_lang.get(lang).map(HashMap::len).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untranslated_string_comes_back_as_itself() {
        let mut t = Translations::new();
        t.extend("pt_BR", [("Create Date".to_string(), "Criado em".to_string())]);
        assert_eq!(t.get("pt_BR", "Create Date"), "Criado em");
        // with no translation, the source — never empty
        assert_eq!(t.get("pt_BR", "Write Date"), "Write Date");
        // a language nobody loaded
        assert_eq!(t.get("de_DE", "Create Date"), "Create Date");
    }

    #[test]
    fn a_later_module_can_correct_an_earlier_one() {
        let mut t = Translations::new();
        t.extend("pt_BR", [("Name".to_string(), "Nome".to_string())]);
        t.extend("pt_BR", [("Name".to_string(), "Company name".to_string())]);
        assert_eq!(t.get("pt_BR", "Name"), "Company name");
        assert_eq!(t.langs(), vec!["pt_BR"]);
    }
}

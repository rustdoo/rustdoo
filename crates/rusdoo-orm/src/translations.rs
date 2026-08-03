//! O catálogo de traduções de texto de código, port do que o Odoo
//! carrega dos `.po` de cada addon (`odoo/tools/translate.py`).
//!
//! Duas coisas se traduzem num Odoo, e só uma delas vive no banco. O
//! *valor* de um registro — o nome de um produto — é uma coluna `jsonb`,
//! porque muda de registro para registro. O *texto do programa* — o
//! rótulo "Create Date", a mensagem de um erro — é o mesmo para todo
//! mundo, vem do `.po` do módulo e mora na memória do servidor.
//!
//! Guardar o segundo no banco seria uma consulta por rótulo por tela.

use std::collections::HashMap;

/// As traduções carregadas: idioma -> (texto de origem -> tradução).
#[derive(Debug, Default, Clone)]
pub struct Translations {
    by_lang: HashMap<String, HashMap<String, String>>,
}

impl Translations {
    pub fn new() -> Translations {
        Translations::default()
    }

    /// Acrescenta o que um `.po` trouxe. Módulos carregam em ordem de
    /// dependência, e o último a falar sobre um texto vence — que é como
    /// um módulo corrige o rótulo de outro.
    pub fn extend(&mut self, lang: &str, entries: impl IntoIterator<Item = (String, String)>) {
        let catalogue = self.by_lang.entry(lang.to_string()).or_default();
        catalogue.extend(entries);
    }

    /// O texto em `lang`, ou ele mesmo quando ninguém o traduziu.
    ///
    /// Nunca vazio e nunca ausente: um rótulo que não foi traduzido
    /// aparece na língua de origem, que é informação, enquanto um rótulo
    /// em branco é uma coluna que o usuário não sabe ler.
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

    /// Os idiomas que têm alguma tradução carregada.
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
        // sem tradução, a origem — nunca vazio
        assert_eq!(t.get("pt_BR", "Write Date"), "Write Date");
        // idioma que ninguém carregou
        assert_eq!(t.get("de_DE", "Create Date"), "Create Date");
    }

    #[test]
    fn a_later_module_can_correct_an_earlier_one() {
        let mut t = Translations::new();
        t.extend("pt_BR", [("Name".to_string(), "Nome".to_string())]);
        t.extend("pt_BR", [("Name".to_string(), "Razão social".to_string())]);
        assert_eq!(t.get("pt_BR", "Name"), "Razão social");
        assert_eq!(t.langs(), vec!["pt_BR"]);
    }
}

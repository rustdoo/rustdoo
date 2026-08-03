//! `self.env.context` — the dict every Odoo call carries.
//!
//! It is not a bag of options: it is how a caller says *in what terms*
//! the work should be done. Which language to answer in, which timezone
//! a date belongs to, whether archived records count, what a form should
//! start filled with, which companies are in scope.
//!
//! Modelled as the JSON map it is on the wire, with named readers for
//! the keys the framework itself acts on. Keys it does not know are kept
//! and passed on untouched — most context keys in a real database belong
//! to a module, not to the framework, and a context that quietly dropped
//! what it did not recognise would break them in a way nobody could see.

use serde_json::{Map, Value};

/// Odoo's default language, and the fallback when a value is missing
/// from a translated field.
pub const DEFAULT_LANG: &str = "en_US";

/// Python's truthiness, which is what a context flag is read with.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context(Map<String, Value>);

impl Context {
    pub fn new() -> Context {
        Context(Map::new())
    }

    /// The context as the client sent it. A non-object (Odoo's `false`,
    /// or a client that sent nothing) is an empty context, not an error:
    /// every key has a default, so there is nothing to refuse.
    pub fn from_value(value: Option<&Value>) -> Context {
        match value.and_then(Value::as_object) {
            Some(map) => Context(map.clone()),
            None => Context::new(),
        }
    }

    /// Odoo's `with_context`: the same context with `key` set.
    pub fn with(mut self, key: &str, value: Value) -> Context {
        self.0.insert(key.to_string(), value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }

    /// The language a read is answered in.
    ///
    /// Odoo's own fallback: no `lang` in the context means `en_US`, the
    /// language source strings are written in.
    pub fn lang(&self) -> &str {
        self.0
            .get("lang")
            .and_then(Value::as_str)
            .filter(|lang| !lang.is_empty())
            .unwrap_or(DEFAULT_LANG)
    }

    /// The timezone dates are shown in, when the caller named one.
    pub fn tz(&self) -> Option<&str> {
        self.0
            .get("tz")
            .and_then(Value::as_str)
            .filter(|tz| !tz.is_empty())
    }

    /// Whether archived records stay out of a search.
    ///
    /// On unless the caller turned it off. "Off" is read with Python's
    /// own truthiness, because that is what decides it in Odoo
    /// (`context.get('active_test', True)` and then a plain `if`): `0`
    /// and `""` turn it off exactly like `False` does, and a client that
    /// sent one of those meant it.
    pub fn active_test(&self) -> bool {
        match self.0.get("active_test") {
            None => true,
            Some(flag) => truthy(flag),
        }
    }

    /// What a fresh form starts `field` with (`default_<field>`).
    pub fn default_for(&self, field: &str) -> Option<&Value> {
        self.0.get(&format!("default_{field}"))
    }

    /// The companies in scope, as the client's company switcher set them.
    pub fn allowed_company_ids(&self) -> Vec<i64> {
        self.0
            .get("allowed_company_ids")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default()
    }
}

impl From<Map<String, Value>> for Context {
    fn from(map: Map<String, Value>) -> Context {
        Context(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_context_answers_every_default() {
        let ctx = Context::from_value(None);
        assert_eq!(ctx.lang(), "en_US");
        assert_eq!(ctx.tz(), None);
        assert!(ctx.active_test(), "arquivados ficam de fora por padrão");
        assert_eq!(ctx.default_for("partner_id"), None);
        assert!(ctx.allowed_company_ids().is_empty());
    }

    #[test]
    fn odoos_false_context_is_an_empty_one() {
        // o cliente manda `false` quando não tem contexto
        assert!(Context::from_value(Some(&json!(false))).is_empty());
        assert!(Context::from_value(Some(&json!(null))).is_empty());
    }

    #[test]
    fn active_test_is_read_with_pythons_truthiness() {
        assert!(Context::new().active_test(), "ausente é ligado");
        for off in [json!(false), json!(null), json!(0), json!("")] {
            assert!(
                !Context::new().with("active_test", off.clone()).active_test(),
                "{off} desliga, como no Python"
            );
        }
        for on in [json!(true), json!(1), json!("1")] {
            assert!(Context::new().with("active_test", on.clone()).active_test(), "{on}");
        }
    }

    #[test]
    fn the_keys_the_framework_does_not_know_survive() {
        let ctx = Context::from_value(Some(&json!({
            "lang": "pt_BR",
            "params": {"action": 42},
            "meu_modulo_flag": true
        })));
        assert_eq!(ctx.lang(), "pt_BR");
        assert_eq!(ctx.get("meu_modulo_flag"), Some(&json!(true)));
        // e passam adiante inteiros
        let passed = ctx.clone().with("tz", json!("America/Sao_Paulo"));
        assert_eq!(passed.get("params"), Some(&json!({"action": 42})));
        assert_eq!(passed.tz(), Some("America/Sao_Paulo"));
    }

    #[test]
    fn a_form_reads_its_defaults_out_of_the_context() {
        let ctx = Context::from_value(Some(&json!({"default_partner_id": 7})));
        assert_eq!(ctx.default_for("partner_id"), Some(&json!(7)));
        assert_eq!(ctx.default_for("name"), None);
    }

    #[test]
    fn an_empty_lang_is_no_lang() {
        // um cliente que manda "" não escolheu um idioma
        assert_eq!(Context::from_value(Some(&json!({"lang": ""}))).lang(), "en_US");
    }
}

//! `/web/webclient/translations` — port of
//! `web/controllers/webclient.py::translations`.
//!
//! The client's localization service fetches this before its first
//! render and throws if the answer is not there, so this route is not a
//! nicety: without it nothing mounts. It carries two things — the texts
//! of the program in the user's language, and how that language writes a
//! date, a time and a number.

use crate::dispatch::OrmService;
use crate::session::Session;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusdoo_orm::crud::SearchOptions;
use serde_json::{json, Map, Value};
use std::hash::{Hash, Hasher};

/// The language a database always has: the one its source is written in.
const SOURCE_LANG: &str = "en_US";

/// What `res.lang` says about a language, as the client reads it. Odoo's
/// own `en_US` row is the fallback — a client that gets no formats
/// stops at `strftimeToLuxonFormat(undefined)`, so answering nothing is
/// not an option.
fn default_lang_parameters(code: &str) -> Value {
    json!({
        "name": code,
        "code": code,
        "direction": "ltr",
        "date_format": "%m/%d/%Y",
        "time_format": "%H:%M:%S",
        "grouping": "[3,0]",
        "decimal_point": ".",
        "thousands_sep": ",",
        "week_start": 7,
    })
}

impl OrmService {
    /// The texts of the client, per module, plus the language's formats.
    ///
    /// `modules` is what to look through: Odoo passes the installed
    /// modules, and this port knows them as the addon directories the
    /// asset hub resolved.
    async fn web_translations(&self, lang: &str) -> Value {
        let mut modules = Map::new();
        for (module, root) in self.assets.module_roots() {
            let path = root.join("i18n").join(format!("{lang}.po"));
            // no file is not an error: `en_US` never has one, and an
            // addon translated into two languages has two
            let messages: Vec<Value> = match std::fs::read_to_string(&path) {
                Ok(source) => rusdoo_modules::po::parse_po(&source)
                    .into_iter()
                    .filter(rusdoo_modules::po::Entry::is_javascript)
                    .map(|entry| json!({"id": entry.msgid, "string": entry.msgstr}))
                    .collect(),
                Err(_) => Vec::new(),
            };
            modules.insert(module.to_string(), json!({"messages": messages}));
        }
        json!(modules)
    }

    /// `res.lang` for a code, when the database has the row. A server
    /// whose `base` was never installed still answers, with the formats
    /// of Odoo's own `en_US` row.
    async fn lang_parameters(&self, lang: &str) -> Value {
        let Some(model) = self.registry.get("res.lang") else {
            return default_lang_parameters(lang);
        };
        let wanted: Vec<&str> = [
            "name",
            "code",
            "direction",
            "date_format",
            "time_format",
            "grouping",
            "decimal_point",
            "thousands_sep",
            "week_start",
        ]
        .into_iter()
        .filter(|field| model.field(field).is_some())
        .collect();
        let domain = json!([["code", "=", lang]]);
        let domain = match rusdoo_orm::domain::parse_domain(&domain) {
            Ok(domain) => domain,
            Err(_) => return default_lang_parameters(lang),
        };
        let opts = SearchOptions {
            limit: Some(1),
            ..SearchOptions::default()
        };
        let row = match self
            .registry
            .search(&self.pool, "res.lang", &domain, &opts)
            .await
        {
            Ok(ids) if !ids.is_empty() => self
                .registry
                .read(&self.pool, "res.lang", &ids, &wanted)
                .await
                .ok()
                .and_then(|rows| rows.into_iter().next()),
            // an uninstalled language is not a broken one: the client
            // formats with the defaults rather than not starting
            _ => None,
        };
        let Some(row) = row else {
            return default_lang_parameters(lang);
        };
        let mut params = default_lang_parameters(lang);
        let object = params.as_object_mut().expect("built as an object");
        for field in wanted {
            match row.get(field) {
                // `week_start` is a selection of digits in the database
                // and a number in the answer, exactly as Odoo's
                // `int(lang_data.week_start)`
                Some(value) if field == "week_start" => {
                    if let Some(n) = value.as_str().and_then(|s| s.parse::<i64>().ok()) {
                        object.insert(field.to_string(), json!(n));
                    }
                }
                Some(value) if !value.is_null() => {
                    object.insert(field.to_string(), value.clone());
                }
                _ => {}
            }
        }
        params
    }
}

/// The query the client sends: `?hash=<what it has>&lang=<its own>`.
#[derive(serde::Deserialize)]
pub(crate) struct TranslationsQuery {
    hash: Option<String>,
    lang: Option<String>,
    /// a comma-separated list, when the caller wants only some modules
    mods: Option<String>,
}

/// `GET /web/webclient/translations`
pub(crate) async fn translations(
    State(service): State<OrmService>,
    headers: axum::http::HeaderMap,
    Query(query): Query<TranslationsQuery>,
) -> Response {
    let session = crate::routes::current_session(&service, &headers);
    let lang = language_of(&service, &query, session.as_ref()).await;

    let mut modules = service.web_translations(&lang).await;
    if let Some(wanted) = query.mods.as_deref() {
        let wanted: Vec<&str> = wanted.split(',').map(str::trim).collect();
        if let Some(object) = modules.as_object_mut() {
            object.retain(|module, _| wanted.contains(&module.as_str()));
        }
    }
    let lang_parameters = service.lang_parameters(&lang).await;
    let multi_lang = service.installed_languages().await > 1;

    let body = json!({
        "lang": lang,
        "hash": catalogue_hash(&modules, &lang_parameters, &lang, multi_lang),
        "lang_parameters": lang_parameters,
        "modules": modules,
        "multi_lang": multi_lang,
    });
    // What the client already has, it does not need again: Odoo answers
    // the hash alone, and the client keeps what its IndexedDB holds.
    let hash = body["hash"].as_str().unwrap_or_default().to_string();
    if query.hash.as_deref() == Some(hash.as_str()) {
        return Json(json!({"lang": lang, "hash": hash})).into_response();
    }
    Json(body).into_response()
}

/// The language to answer in: what the client asked for, else the
/// language of whoever is logged in, else the source language.
async fn language_of(
    service: &OrmService,
    query: &TranslationsQuery,
    session: Option<&Session>,
) -> String {
    if let Some(lang) = query.lang.as_deref().filter(|lang| is_lang_code(lang)) {
        return lang.to_string();
    }
    match session {
        Some(session) => service
            .session_info(Some(session))
            .await
            .get("user_context")
            .and_then(|context| context.get("lang"))
            .and_then(Value::as_str)
            .unwrap_or(SOURCE_LANG)
            .to_string(),
        None => SOURCE_LANG.to_string(),
    }
}

/// A language code names a file this server will open, so it is checked
/// as one: letters, digits and `_`, nothing that walks out of `i18n/`.
fn is_lang_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 32
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'@')
}

/// Which version of the vocabulary this is. The client stores it and
/// sends it back, so it only has to be stable across restarts for the
/// same content — the same property the asset ETags rely on.
fn catalogue_hash(modules: &Value, lang_parameters: &Value, lang: &str, multi_lang: bool) -> String {
    let canonical = json!({
        "lang": lang,
        "lang_parameters": lang_parameters,
        "modules": modules,
        "multi_lang": multi_lang,
    })
    .to_string();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

impl OrmService {
    /// How many languages the database has active — `multi_lang` tells
    /// the client whether to offer the translation dialog at all.
    pub(crate) async fn installed_languages(&self) -> usize {
        if self.registry.get("res.lang").is_none() {
            return 1;
        }
        let Ok(domain) = rusdoo_orm::domain::parse_domain(&json!([])) else {
            return 1;
        };
        self.registry
            .search(&self.pool, "res.lang", &domain, &SearchOptions::default())
            .await
            .map(|ids| ids.len())
            .unwrap_or(1)
    }
}

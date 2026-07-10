//! Port of manifest handling in `odoo/modules/module.py`.

use crate::pyliteral::parse_py_literal;
use rusdoo_core::RusdooError;
use serde_json::Value;

/// The keys of an addon `__manifest__.py` that the loader consumes.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// technical name (the addon directory name)
    pub name: String,
    /// the manifest 'name' key
    pub display_name: String,
    pub version: String,
    pub category: String,
    pub summary: String,
    pub depends: Vec<String>,
    /// data files (XML/CSV) in load order
    pub data: Vec<String>,
    pub installable: bool,
    pub auto_install: bool,
}

pub fn parse_manifest(source: &str, technical_name: &str) -> Result<Manifest, RusdooError> {
    let value = parse_py_literal(source)
        .map_err(|e| RusdooError::Validation(format!("manifest {technical_name}: {e}")))?;
    let Value::Object(map) = value else {
        return Err(RusdooError::Validation(format!(
            "manifest {technical_name} must be a python dict"
        )));
    };
    let get_str = |key: &str, default: &str| -> Result<String, RusdooError> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(default.to_string()),
            Some(Value::String(text)) => Ok(text.clone()),
            // seen in the wild: 'version': 16.0 as a bare number
            Some(Value::Number(number)) => Ok(number.to_string()),
            Some(_) => Err(RusdooError::Validation(format!(
                "manifest {technical_name}: '{key}' must be a string"
            ))),
        }
    };
    let str_list = |key: &str| -> Result<Vec<String>, RusdooError> {
        match map.get(key) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        RusdooError::Validation(format!(
                            "manifest {technical_name}: '{key}' entries must be strings"
                        ))
                    })
                })
                .collect(),
            Some(_) => Err(RusdooError::Validation(format!(
                "manifest {technical_name}: '{key}' must be a list"
            ))),
        }
    };
    Ok(Manifest {
        name: technical_name.to_string(),
        display_name: get_str("name", technical_name)?,
        version: get_str("version", "1.0")?,
        category: get_str("category", "Uncategorized")?,
        summary: get_str("summary", "")?.trim().to_string(),
        depends: str_list("depends")?,
        data: str_list("data")?,
        installable: map
            .get("installable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        // bool, or a list of modules meaning "install when those are"
        auto_install: match map.get("auto_install") {
            Some(Value::Bool(enabled)) => *enabled,
            Some(Value::Array(_)) => true,
            _ => false,
        },
    })
}

//! rusdoo-python — running an addon's Python on the Rust core.
//!
//! The goal this crate exists for is stated in issue #10: an addon
//! published by the Odoo community tomorrow should run here without
//! being rewritten, even if at first its Python runs as Python. A Rust
//! ERP that only runs code written for it is not a port of Odoo.
//!
//! What works today is the first half of that: a `models.py` written
//! against the ordinary `odoo` API declares its models, and they land in
//! the Rust [`Registry`] as if a Rust module had declared them. From
//! there the whole server serves them — tables, ACL, views, RPC.
//!
//! What does *not* work yet, and is the next half: behaviour. A method
//! on a Python model is not callable from `call_kw`, `@api.depends` does
//! not compute anything, and there is no recordset to write `self.name`
//! against. The bridge is one-way for now — declarations cross, calls do
//! not.
//!
//! ```ignore
//! let mut registry = Registry::new();
//! load_python_models(&mut registry, "res.partner", r#"
//!     from odoo import models, fields
//!     class Partner(models.Model):
//!         _name = "res.partner"
//!         name = fields.Char(required=True)
//! "#)?;
//! ```

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};

/// The shim package, compiled into the binary.
///
/// Not read off disk: a server that needed its `odoo/` directory next to
/// it would be a server that runs in development and not in production,
/// and the shim is part of the program, not configuration.
const ODOO_INIT: &str = include_str!("../python/odoo/__init__.py");
const ODOO_FIELDS: &str = include_str!("../python/odoo/fields.py");
const ODOO_MODELS: &str = include_str!("../python/odoo/models.py");

/// What Python declared while a load was running.
///
/// A global because the `_rusdoo` module Python calls has no way to
/// carry a reference to the caller's registry — the callback comes from
/// inside the interpreter, and the interpreter is process-wide. The lock
/// makes the load a critical section, which it is anyway: CPython has
/// one GIL.
static DECLARED: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();

/// Serialises a whole load. See [`run_and_collect`].
static BRIDGE: Mutex<()> = Mutex::new(());

fn declared() -> &'static Mutex<Vec<Value>> {
    DECLARED.get_or_init(|| Mutex::new(Vec::new()))
}

/// `_rusdoo.declare_model(spec)` — what the metaclass calls.
#[pyfunction]
fn declare_model(spec: &Bound<'_, PyDict>) -> PyResult<()> {
    let json: Value = pythonize(spec.as_any())?;
    declared()
        .lock()
        .expect("the declaration list is not poisoned")
        .push(json);
    Ok(())
}

#[pymodule]
fn _rusdoo(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(declare_model, module)?)
}

/// A Python value as JSON — the only shape both sides already agree on.
///
/// Deliberately narrow: dict, list, str, int, float, bool, None. An
/// addon that puts something else in a field declaration is doing
/// something this bridge does not understand, and saying so beats
/// storing its `repr`.
fn pythonize(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    if value.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(flag) = value.extract::<bool>() {
        return Ok(Value::Bool(flag));
    }
    if let Ok(number) = value.extract::<i64>() {
        return Ok(Value::from(number));
    }
    if let Ok(number) = value.extract::<f64>() {
        return Ok(Value::from(number));
    }
    if let Ok(text) = value.extract::<String>() {
        return Ok(Value::from(text));
    }
    if let Ok(dict) = value.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (key, item) in dict.iter() {
            map.insert(key.extract::<String>()?, pythonize(&item)?);
        }
        return Ok(Value::Object(map));
    }
    if let Ok(list) = value.cast::<PyList>() {
        let mut items = Vec::new();
        for item in list.iter() {
            items.push(pythonize(&item)?);
        }
        return Ok(Value::Array(items));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "rusdoo cannot carry this value across: {}",
        value.get_type().name()?
    )))
}

/// Run `source` with the `odoo` shim importable, and register whatever
/// models it declared into `registry`.
///
/// `module_name` is what the code is called in tracebacks — an addon's
/// dotted name, so a syntax error says which file.
pub fn load_python_models(
    registry: &mut Registry,
    module_name: &str,
    source: &str,
) -> Result<Vec<String>, RusdooError> {
    let specs = run_and_collect(module_name, source)?;
    let mut loaded = Vec::new();
    for spec in specs {
        let model = model_from_spec(&spec)?;
        let name = model.meta.name.clone();
        registry.register(model)?;
        loaded.push(name);
    }
    Ok(loaded)
}

/// The interpreter side: install the shim, run the addon, take what it
/// declared.
fn run_and_collect(module_name: &str, source: &str) -> Result<Vec<Value>, RusdooError> {
    // One load at a time, and the *whole* load: installing the shim,
    // running the addon and taking what it declared are one critical
    // section. Two loads interleaving would hand each other's models to
    // the wrong registry — and, worse, the second would find the `odoo`
    // package half-built and fail on an import that is perfectly fine.
    let _bridge = BRIDGE
        .lock()
        .map_err(|_| RusdooError::Validation("the Python bridge is poisoned".into()))?;
    declared()
        .lock()
        .map_err(|_| RusdooError::Validation("the Python bridge is poisoned".into()))?
        .clear();

    Python::attach(|py| -> Result<(), RusdooError> {
        install_shim(py)?;
        let module = PyModule::from_code(
            py,
            &std::ffi::CString::new(source).map_err(|error| {
                RusdooError::Validation(format!("{module_name}: source has a NUL byte: {error}"))
            })?,
            &std::ffi::CString::new(format!("{module_name}.py")).unwrap(),
            &std::ffi::CString::new(module_name).unwrap(),
        );
        module.map(|_| ()).map_err(|error| python_error(py, error))
    })?;

    let mut pending = declared()
        .lock()
        .map_err(|_| RusdooError::Validation("the Python bridge is poisoned".into()))?;
    Ok(std::mem::take(&mut *pending))
}

/// Put `_rusdoo` and the `odoo` package into `sys.modules`, once.
///
/// The order is the whole difficulty. `odoo/models.py` says `from . import
/// fields`, and a relative import resolves through the *package*, so
/// `odoo` has to be in `sys.modules` before `odoo.models` runs — even
/// though `odoo/__init__.py` imports `odoo.models` in turn. Python
/// handles that circle for a package on disk by putting the empty module
/// in place first; there is no disk here, so it is done by hand.
fn install_shim(py: Python<'_>) -> Result<(), RusdooError> {
    let sys = py.import("sys").map_err(|e| python_error(py, e))?;
    let modules = sys
        .getattr("modules")
        .and_then(|m| m.cast_into::<PyDict>().map_err(Into::into))
        .map_err(|e| python_error(py, e))?;
    if modules.contains("odoo").unwrap_or(false) {
        return Ok(());
    }

    let native = pyo3::wrap_pymodule!(_rusdoo)(py);
    modules
        .set_item("_rusdoo", native)
        .map_err(|e| python_error(py, e))?;

    // the empty package first, so a relative import inside a submodule
    // has a parent to resolve against
    let package = PyModule::new(py, "odoo").map_err(|e| python_error(py, e))?;
    package
        .setattr("__path__", PyList::empty(py))
        .map_err(|e| python_error(py, e))?;
    modules
        .set_item("odoo", &package)
        .map_err(|e| python_error(py, e))?;

    for (leaf, code) in [("fields", ODOO_FIELDS), ("models", ODOO_MODELS)] {
        let name = format!("odoo.{leaf}");
        let module = PyModule::from_code(
            py,
            &std::ffi::CString::new(code).unwrap(),
            &std::ffi::CString::new(format!("{name}.py")).unwrap(),
            &std::ffi::CString::new(name.clone()).unwrap(),
        )
        .map_err(|e| python_error(py, e))?;
        module
            .setattr("__package__", "odoo")
            .map_err(|e| python_error(py, e))?;
        modules
            .set_item(&name, &module)
            .map_err(|e| python_error(py, e))?;
        // and as an attribute of the package, which is what
        // `from odoo import fields` reads
        package
            .setattr(leaf, &module)
            .map_err(|e| python_error(py, e))?;
    }

    // the package's own body last, into the module already in place
    py.run(
        &std::ffi::CString::new(ODOO_INIT).unwrap(),
        Some(&package.dict()),
        None,
    )
    .map_err(|e| python_error(py, e))?;
    Ok(())
}

/// A Python exception as a rusdoo error, traceback and all.
///
/// The traceback is the whole point: an addon that fails to import fails
/// somewhere inside its own file, and a message that said only
/// "ImportError" would send whoever installed it looking at the wrong
/// thing.
fn python_error(py: Python<'_>, error: PyErr) -> RusdooError {
    let mut message = error.to_string();
    if let Some(traceback) = error.traceback(py) {
        if let Ok(text) = traceback.format() {
            message = format!("{message}\n{text}");
        }
    }
    RusdooError::Validation(message)
}

/// One declared model, as a [`Model`] the registry accepts.
fn model_from_spec(spec: &Value) -> Result<Model, RusdooError> {
    let name = spec_str(spec, "name")?;
    let table = spec_str(spec, "table")?;
    let inherit: Vec<String> = spec
        .get("inherit")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut fields = Vec::new();
    for declared in spec
        .get("fields")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        fields.push(field_from_spec(&name, declared)?);
    }
    let mut model = Model::new(
        ModelMeta {
            name,
            table,
            inherit,
            inherits: vec![],
        },
        fields,
    );
    if spec.get("transient") == Some(&Value::Bool(true)) {
        model = model.transient();
    }
    if let Some(order) = spec.get("order").and_then(Value::as_str) {
        model = model.ordered(order);
    }
    Ok(model)
}

fn field_from_spec(model: &str, spec: &Value) -> Result<Field, RusdooError> {
    let name = spec_str(spec, "name")?;
    // a `fields.Field` with no type of its own is the base class, which
    // an addon is not supposed to instantiate; naming the field is the
    // difference between a message somebody can act on and one they
    // cannot
    let kind = spec
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            RusdooError::Validation(format!(
                "{model}.{name}: field type None is not supported yet — \
                 `fields.Field` is the base class, use one of its subclasses"
            ))
        })?;
    let comodel = || -> Result<String, RusdooError> {
        spec.get("comodel")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                RusdooError::Validation(format!(
                    "{model}.{name}: a {kind} field needs comodel_name"
                ))
            })
    };
    let ty = match kind.as_str() {
        "char" => FieldType::Char {
            size: spec.get("size").and_then(Value::as_u64).map(|n| n as u32),
        },
        "text" => FieldType::Text,
        "html" => FieldType::Html,
        "integer" => FieldType::Integer,
        "float" => FieldType::Float {
            digits: spec
                .get("digits")
                .and_then(Value::as_array)
                .and_then(|pair| {
                    Some((pair.first()?.as_u64()? as u8, pair.get(1)?.as_u64()? as u8))
                }),
        },
        "monetary" => FieldType::Monetary,
        "boolean" => FieldType::Boolean,
        "date" => FieldType::Date,
        "datetime" => FieldType::Datetime,
        "binary" => FieldType::Binary,
        "json" => FieldType::Json,
        "selection" => FieldType::Selection(
            spec.get("selection")
                .and_then(Value::as_array)
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|pair| {
                            let pair = pair.as_array()?;
                            Some((
                                pair.first()?.as_str()?.to_string(),
                                pair.get(1)?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "many2one" => FieldType::Many2one {
            comodel: comodel()?,
        },
        "one2many" => FieldType::One2many {
            comodel: comodel()?,
            inverse: spec
                .get("inverse")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    RusdooError::Validation(format!(
                        "{model}.{name}: a one2many needs inverse_name"
                    ))
                })?,
        },
        "many2many" => {
            let co = comodel()?;
            // Odoo's own naming when the addon does not say: the two
            // tables joined by an underscore, and each side named after
            // its own model
            let relation = spec
                .get("relation")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!("{}_{}_rel", model.replace('.', "_"), co.replace('.', "_"))
                });
            FieldType::Many2many {
                comodel: co.clone(),
                relation,
                column1: spec
                    .get("column1")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}_id", model.replace('.', "_"))),
                column2: spec
                    .get("column2")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}_id", co.replace('.', "_"))),
            }
        }
        other => {
            // a field type with no column here is a field nobody could
            // read: better to refuse the addon than to install a model
            // with a hole in it
            return Err(RusdooError::Validation(format!(
                "{model}.{name}: field type {other:?} is not supported yet"
            )));
        }
    };
    let mut field = Field::new(&name, ty);
    if spec.get("required") == Some(&Value::Bool(true)) {
        field = field.required();
    }
    if spec.get("readonly") == Some(&Value::Bool(true)) {
        field = field.readonly();
    }
    if spec.get("translate") == Some(&Value::Bool(true)) {
        field = field.translatable();
    }
    if let Some(default) = spec.get("default") {
        if !default.is_null() {
            field = field.default_value(default.clone());
        }
    }
    Ok(field)
}

fn spec_str(spec: &Value, key: &str) -> Result<String, RusdooError> {
    spec.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RusdooError::Validation(format!("a declared model has no {key:?}")))
}

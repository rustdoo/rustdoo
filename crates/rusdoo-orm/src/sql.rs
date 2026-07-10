//! Domain -> SQL WHERE translation with `$n` placeholders.
//!
//! Mirrors the NULL semantics of Odoo's condition operators: a False/None
//! value means *not set*, and negative operators also match unset records.
//! With a model in context, "not set" additionally matches the field's
//! falsy value (0, '', false). With a registry in context, dotted paths
//! (`company_id.name`) and `any`/`not any` become semi-join subqueries.

use crate::domain::{parse_domain, Domain, Operator, Term};
use crate::fields::FieldType;
use crate::model::Model;
use crate::registry::Registry;
use rusdoo_core::RusdooError;
use serde_json::Value;

/// Hard cap mirroring the parser's limit: `Domain` values can also be built
/// programmatically, so rendering must bound its own recursion.
const MAX_RENDER_DEPTH: usize = 100;

/// What the renderer knows about its surroundings. Both parts are optional:
/// without a model, falsy-value matching is skipped; without a registry,
/// relational traversal is an explicit error.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub model: Option<&'a Model>,
    pub registry: Option<&'a Registry>,
}

impl<'a> Ctx<'a> {
    pub fn empty() -> Ctx<'static> {
        Ctx {
            model: None,
            registry: None,
        }
    }

    pub fn model(model: &'a Model) -> Ctx<'a> {
        Ctx {
            model: Some(model),
            registry: None,
        }
    }

    pub fn full(model: &'a Model, registry: &'a Registry) -> Ctx<'a> {
        Ctx {
            model: Some(model),
            registry: Some(registry),
        }
    }

    fn with_model(self, model: &'a Model) -> Ctx<'a> {
        Ctx {
            model: Some(model),
            ..self
        }
    }
}

/// Validate and double-quote a SQL identifier.
pub fn quote_ident(name: &str) -> Result<String, RusdooError> {
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(format!("\"{name}\""))
    } else {
        Err(RusdooError::Validation(format!(
            "invalid SQL identifier: {name:?}"
        )))
    }
}

/// Context-free rendering; prefer [`render`] with a fuller [`Ctx`].
pub fn where_clause(domain: &Domain) -> Result<(String, Vec<Value>), RusdooError> {
    let mut params = Vec::new();
    let sql = render(domain, &mut params, Ctx::empty())?;
    Ok((sql, params))
}

/// Render a domain, appending its bind values to `params` (placeholders
/// continue from the current length, so callers can prepend their own).
pub fn render(domain: &Domain, params: &mut Vec<Value>, ctx: Ctx) -> Result<String, RusdooError> {
    render_at(domain, params, ctx, 0)
}

pub(crate) fn bind(params: &mut Vec<Value>, value: Value) -> String {
    params.push(value);
    format!("${}", params.len())
}

fn depth_check(depth: usize) -> Result<(), RusdooError> {
    if depth > MAX_RENDER_DEPTH {
        return Err(RusdooError::Validation(format!(
            "domain nesting exceeds {MAX_RENDER_DEPTH} levels"
        )));
    }
    Ok(())
}

fn render_at(
    domain: &Domain,
    params: &mut Vec<Value>,
    ctx: Ctx,
    depth: usize,
) -> Result<String, RusdooError> {
    depth_check(depth)?;
    match domain {
        Domain::True => Ok("TRUE".into()),
        Domain::False => Ok("FALSE".into()),
        Domain::And(children) => render_nary("AND", children, params, ctx, depth),
        Domain::Or(children) => render_nary("OR", children, params, ctx, depth),
        Domain::Not(child) => Ok(format!(
            "NOT ({})",
            render_at(child, params, ctx, depth + 1)?
        )),
        Domain::Term(term) => render_term(term, params, ctx, depth),
    }
}

fn render_nary(
    op: &str,
    children: &[Domain],
    params: &mut Vec<Value>,
    ctx: Ctx,
    depth: usize,
) -> Result<String, RusdooError> {
    match children {
        [] => Ok("TRUE".into()),
        [only] => render_at(only, params, ctx, depth + 1),
        _ => {
            let parts: Vec<String> = children
                .iter()
                .map(|c| render_at(c, params, ctx, depth + 1))
                .collect::<Result<_, _>>()?;
            Ok(format!("({})", parts.join(&format!(" {op} "))))
        }
    }
}

/// Odoo semantics: False/None as a condition value means "not set".
fn is_unset(value: &Value) -> bool {
    value.is_null() || *value == Value::Bool(false)
}

/// The field's stored falsy value, when the model is known and the type
/// has one (0 for numeric, '' for text, false for boolean).
fn falsy_value(field: &str, ctx: Ctx) -> Option<Value> {
    ctx.model?.field(field)?.falsy_value()
}

fn require_env<'a>(ctx: Ctx<'a>, what: &str) -> Result<(&'a Model, &'a Registry), RusdooError> {
    match (ctx.model, ctx.registry) {
        (Some(model), Some(registry)) => Ok((model, registry)),
        _ => Err(RusdooError::Validation(format!(
            "{what} requires model and registry context"
        ))),
    }
}

fn comodel<'a>(registry: &'a Registry, name: &str) -> Result<&'a Model, RusdooError> {
    registry
        .get(name)
        .ok_or_else(|| RusdooError::Validation(format!("comodel not registered: {name}")))
}

fn render_term(
    term: &Term,
    params: &mut Vec<Value>,
    ctx: Ctx,
    depth: usize,
) -> Result<String, RusdooError> {
    if let Some((head, rest)) = term.field.split_once('.') {
        return render_path(head, rest, term, params, ctx, depth);
    }
    if matches!(term.op, Operator::Any | Operator::NotAny) {
        return render_any(term, params, ctx, depth);
    }

    let col = quote_ident(&term.field)?;
    let value = &term.value;
    use Operator::*;

    // Odoo rewrites '='/'!=' with a collection value into 'in'/'not in'
    if value.is_array() && matches!(term.op, Eq | Neq) {
        return render_in(&col, value, term.op == Neq, params);
    }

    match &term.op {
        Eq => Ok(if is_unset(value) {
            match falsy_value(&term.field, ctx) {
                Some(fv) => format!("({col} = {p} OR {col} IS NULL)", p = bind(params, fv)),
                None => format!("{col} IS NULL"),
            }
        } else {
            format!("{col} = {}", bind(params, value.clone()))
        }),
        Neq => Ok(if is_unset(value) {
            match falsy_value(&term.field, ctx) {
                Some(fv) => format!("({col} != {p} AND {col} IS NOT NULL)", p = bind(params, fv)),
                None => format!("{col} IS NOT NULL"),
            }
        } else {
            // negative operators also match unset records
            format!(
                "({col} != {p} OR {col} IS NULL)",
                p = bind(params, value.clone())
            )
        }),
        EqMaybe => Ok(if is_unset(value) {
            "TRUE".into()
        } else {
            format!("{col} = {}", bind(params, value.clone()))
        }),
        Lt => render_cmp(&col, "<", value, params),
        Lte => render_cmp(&col, "<=", value, params),
        Gt => render_cmp(&col, ">", value, params),
        Gte => render_cmp(&col, ">=", value, params),
        In | NotIn => render_in(&col, value, term.op == NotIn, params),
        Like => render_like(&col, "LIKE", value, true, false, params),
        ILike => render_like(&col, "ILIKE", value, true, false, params),
        NotLike => render_like(&col, "LIKE", value, true, true, params),
        NotILike => render_like(&col, "ILIKE", value, true, true, params),
        EqLike => render_like(&col, "LIKE", value, false, false, params),
        EqILike => render_like(&col, "ILIKE", value, false, false, params),
        NotEqLike => render_like(&col, "LIKE", value, false, true, params),
        NotEqILike => render_like(&col, "ILIKE", value, false, true, params),
        ChildOf | ParentOf => Err(RusdooError::Validation(format!(
            "operator not yet supported: {:?}",
            term.op
        ))),
        Any | NotAny => unreachable!("handled above"),
    }
}

/// `company_id.name = x` -> `company_id IN (SELECT id FROM res_company
/// WHERE name = x)`; one2many paths go through the inverse column.
fn render_path(
    head: &str,
    rest: &str,
    term: &Term,
    params: &mut Vec<Value>,
    ctx: Ctx,
    depth: usize,
) -> Result<String, RusdooError> {
    depth_check(depth)?;
    let (model, registry) = require_env(ctx, "dotted field path")?;
    let field = model.field(head).ok_or_else(|| {
        RusdooError::Validation(format!("unknown field on {}: {head:?}", model.meta.name))
    })?;
    let inner = Term {
        field: rest.to_string(),
        op: term.op.clone(),
        value: term.value.clone(),
    };
    match &field.ty {
        FieldType::Many2one { comodel: co_name } => {
            let co = comodel(registry, co_name)?;
            let sub = render_term(&inner, params, ctx.with_model(co), depth + 1)?;
            Ok(format!(
                r#"{} IN (SELECT "id" FROM {} WHERE {sub})"#,
                quote_ident(head)?,
                quote_ident(&co.meta.table)?
            ))
        }
        FieldType::One2many {
            comodel: co_name,
            inverse,
        } => {
            let co = comodel(registry, co_name)?;
            let sub = render_term(&inner, params, ctx.with_model(co), depth + 1)?;
            Ok(format!(
                r#""id" IN (SELECT {} FROM {} WHERE {sub})"#,
                quote_ident(inverse)?,
                quote_ident(&co.meta.table)?
            ))
        }
        other => Err(RusdooError::Validation(format!(
            "cannot traverse field {head:?} of type {other:?}"
        ))),
    }
}

/// `["company_id", "any", <domain>]` -> semi-join with the sub-domain
/// rendered against the comodel.
fn render_any(
    term: &Term,
    params: &mut Vec<Value>,
    ctx: Ctx,
    depth: usize,
) -> Result<String, RusdooError> {
    depth_check(depth)?;
    let (model, registry) = require_env(ctx, "'any' operator")?;
    let field = model.field(&term.field).ok_or_else(|| {
        RusdooError::Validation(format!(
            "unknown field on {}: {:?}",
            model.meta.name, term.field
        ))
    })?;
    let sub_domain = parse_domain(&term.value)?;
    let negative = term.op == Operator::NotAny;
    match &field.ty {
        FieldType::Many2one { comodel: co_name } => {
            let co = comodel(registry, co_name)?;
            let sub = render_at(&sub_domain, params, ctx.with_model(co), depth + 1)?;
            let col = quote_ident(&term.field)?;
            let table = quote_ident(&co.meta.table)?;
            Ok(if negative {
                format!(r#"({col} NOT IN (SELECT "id" FROM {table} WHERE {sub}) OR {col} IS NULL)"#)
            } else {
                format!(r#"{col} IN (SELECT "id" FROM {table} WHERE {sub})"#)
            })
        }
        FieldType::One2many {
            comodel: co_name,
            inverse,
        } => {
            let co = comodel(registry, co_name)?;
            let sub = render_at(&sub_domain, params, ctx.with_model(co), depth + 1)?;
            let inv = quote_ident(inverse)?;
            let table = quote_ident(&co.meta.table)?;
            Ok(if negative {
                // NULL inverse rows would poison NOT IN; filter them inside
                format!(
                    r#""id" NOT IN (SELECT {inv} FROM {table} WHERE ({sub}) AND {inv} IS NOT NULL)"#
                )
            } else {
                format!(r#""id" IN (SELECT {inv} FROM {table} WHERE {sub})"#)
            })
        }
        other => Err(RusdooError::Validation(format!(
            "'any' not supported on field type {other:?}"
        ))),
    }
}

fn render_cmp(
    col: &str,
    sql_op: &str,
    value: &Value,
    params: &mut Vec<Value>,
) -> Result<String, RusdooError> {
    Ok(format!("{col} {sql_op} {}", bind(params, value.clone())))
}

fn render_in(
    col: &str,
    value: &Value,
    negative: bool,
    params: &mut Vec<Value>,
) -> Result<String, RusdooError> {
    let items = value.as_array().ok_or_else(|| {
        RusdooError::Validation(format!("'in' operator expects a list, got {value}"))
    })?;
    let has_unset = items.iter().any(is_unset);
    let set_values: Vec<&Value> = items.iter().filter(|v| !is_unset(v)).collect();

    if set_values.is_empty() {
        return Ok(match (negative, has_unset) {
            (false, true) => format!("{col} IS NULL"),
            (false, false) => "FALSE".into(),
            (true, true) => format!("{col} IS NOT NULL"),
            (true, false) => "TRUE".into(),
        });
    }
    let placeholders: Vec<String> = set_values
        .into_iter()
        .map(|v| bind(params, v.clone()))
        .collect();
    let list = placeholders.join(", ");
    Ok(match (negative, has_unset) {
        (false, false) => format!("{col} IN ({list})"),
        (false, true) => format!("({col} IN ({list}) OR {col} IS NULL)"),
        // "not set" excluded explicitly: value must exist and differ
        (true, true) => format!("({col} NOT IN ({list}) AND {col} IS NOT NULL)"),
        (true, false) => format!("({col} NOT IN ({list}) OR {col} IS NULL)"),
    })
}

fn render_like(
    col: &str,
    sql_op: &str,
    value: &Value,
    wrap: bool,
    negative: bool,
    params: &mut Vec<Value>,
) -> Result<String, RusdooError> {
    let text = value.as_str().ok_or_else(|| {
        RusdooError::Validation(format!("pattern operator expects a string, got {value}"))
    })?;
    let pattern = if wrap {
        // the user searched a literal string: escape LIKE wildcards, then wrap
        let escaped = text
            .replace('\\', r"\\")
            .replace('%', r"\%")
            .replace('_', r"\_");
        format!("%{escaped}%")
    } else {
        text.to_string()
    };
    let p = bind(params, Value::String(pattern));
    Ok(if negative {
        format!("({col} NOT {sql_op} {p} OR {col} IS NULL)")
    } else {
        format!("{col} {sql_op} {p}")
    })
}

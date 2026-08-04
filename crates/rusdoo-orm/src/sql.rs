//! Domain -> SQL WHERE translation with `$n` placeholders.
//!
//! Mirrors the NULL semantics of Odoo's condition operators: a False/None
//! value means *not set*, and negative operators also match unset records.
//! With a model in context, "not set" additionally matches the field's
//! falsy value (0, '', false). With a registry in context, dotted paths
//! (`company_id.name`) and `any`/`not any` become semi-join subqueries.

use crate::domain::{parse_domain, Domain, Operator, Term};
use crate::fields::{Field, FieldType};
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
    /// the language a condition on a translated field compares in.
    ///
    /// Without it a filter on a translated column compares the whole
    /// language map against a string, which PostgreSQL refuses outright
    /// (`operator does not exist: jsonb = text`) — a search bar that
    /// errors instead of filtering.
    pub lang: &'a str,
}

impl<'a> Ctx<'a> {
    pub fn empty() -> Ctx<'static> {
        Ctx {
            model: None,
            registry: None,
            lang: crate::context::DEFAULT_LANG,
        }
    }

    /// The same context, comparing translated fields in `lang`.
    pub fn in_lang(mut self, lang: &'a str) -> Ctx<'a> {
        self.lang = lang;
        self
    }

    pub fn model(model: &'a Model) -> Ctx<'a> {
        Ctx {
            model: Some(model),
            registry: None,
            lang: crate::context::DEFAULT_LANG,
        }
    }

    pub fn full(model: &'a Model, registry: &'a Registry) -> Ctx<'a> {
        Ctx {
            model: Some(model),
            registry: Some(registry),
            lang: crate::context::DEFAULT_LANG,
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

/// A column that lives on an `_inherits` parent, as an expression the
/// child's own query can select, group and order by.
///
/// `None` when `name` is not a delegated field of `model` — it is either
/// the model's own column or nothing at all, and the caller says which.
///
/// The expression is a correlated subquery per hop:
///
/// ```sql
/// (SELECT "product_template"."name" FROM "product_template"
///   WHERE "product_template"."id" = "product_product"."product_tmpl_id")
/// ```
///
/// A join would read better and plan better, but every query this ORM
/// builds selects `FROM` one table; a subquery is the form that fits
/// wherever a column fits, which is what `ORDER BY` and `GROUP BY` need.
/// Each hop is an index lookup on a primary key — the same work a join
/// would do, planned per row instead of once.
pub(crate) fn delegated_expr(
    registry: &Registry,
    model: &Model,
    name: &str,
) -> Result<Option<String>, RusdooError> {
    if model.field(name).is_some() {
        return Ok(None);
    }
    let Some(chain) = registry.delegation_chain(model, name, 0) else {
        return Ok(None);
    };
    // walk the chain outwards, each hop resolving the id of the next
    // record from the one before it
    let mut owner_id = format!(
        "{}.{}",
        quote_ident(&model.meta.table)?,
        quote_ident(&chain[0].0)?
    );
    for (index, (_, parent_name)) in chain.iter().enumerate() {
        let parent = registry.get(parent_name).ok_or_else(|| {
            RusdooError::Validation(format!("_inherits parent not registered: {parent_name}"))
        })?;
        let table = quote_ident(&parent.meta.table)?;
        // the last hop selects the column itself; the others select the
        // link that leads to the next record
        let selected = match chain.get(index + 1) {
            Some((link, _)) => quote_ident(link)?,
            None => quote_ident(name)?,
        };
        owner_id =
            format!(r#"(SELECT {table}.{selected} FROM {table} WHERE {table}."id" = {owner_id})"#);
    }
    Ok(Some(owner_id))
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

/// The cast a column needs to come back as a JSON-friendly value.
///
/// `numeric` has no native decoder here, and Odoo itself hands money to
/// Python as a float — so a fixed-precision column is read as float8
/// rather than dragging in a decimal type the wire format has no room
/// for.
pub(crate) fn read_cast_for(ty: &FieldType) -> &'static str {
    match ty {
        FieldType::Float { digits: Some(_) } | FieldType::Monetary => "::float8",
        _ => "",
    }
}

/// The cast a bound value needs to be comparable with a column of this
/// type. Dates and datetimes arrive as strings, so their parameter is
/// text and PostgreSQL finds no `date >= text` operator; casting the
/// parameter (never the column) keeps the comparison typed and leaves
/// any index on the column usable.
/// Reading a translated column: the asked-for language, falling back to
/// the source one. Odoo's `get_translation_fallback_langs`.
///
/// The language is an identifier from a context the client controls, so
/// it is quoted as a literal rather than pasted in — a `lang` of
/// `x' || (SELECT ...) || '` would otherwise be a query of its own.
pub(crate) fn translated_read(column: &str, lang: &str) -> Result<String, RusdooError> {
    let lang = quote_literal(lang);
    if lang == "'en_US'" {
        return Ok(format!("{column}->>'en_US'"));
    }
    Ok(format!("COALESCE({column}->>{lang}, {column}->>'en_US')"))
}

/// A string as a PostgreSQL literal, quotes doubled.
pub(crate) fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn value_cast_for(ty: Option<&FieldType>) -> &'static str {
    match ty {
        Some(FieldType::Date) => "::date",
        Some(FieldType::Datetime) => "::timestamp",
        // a `numeric` column would have the driver's 8-byte float read as
        // a numeric — the bytes are not the same shape, and PostgreSQL
        // rejects the result as an overflow. Typing the parameter float8
        // and letting the assignment cast do the conversion is what makes
        // a price with decimals storable at all.
        Some(FieldType::Float { digits: Some(_) } | FieldType::Monetary) => "::float8",
        _ => "",
    }
}

/// The column a condition compares against: the plain identifier, or
/// the translated read when the field holds one value per language.
pub(crate) fn column_expr(field: &str, ctx: Ctx) -> Result<String, RusdooError> {
    let quoted = quote_ident(field)?;
    match ctx.model.and_then(|m| m.field(field)) {
        Some(f) if f.translate => translated_read(&quoted, ctx.lang),
        _ => Ok(quoted),
    }
}

/// The cast for a field of the model in context. Without a model there is
/// no type to cast to, so the value is bound as-is — the same degradation
/// every other type-aware rewrite here makes.
fn value_cast(field: &str, ctx: Ctx) -> &'static str {
    value_cast_for(ctx.model.and_then(|m| m.field(field)).map(|f| &f.ty))
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
    // _inherits delegation: a field living on a delegation parent is
    // reached through its link field, like Odoo's inherited fields
    if let (Some(model), Some(registry)) = (ctx.model, ctx.registry) {
        let head = term.field.split('.').next().unwrap_or(&term.field);
        if head != "id" && model.field(head).is_none() {
            if let Some(chain) = registry.delegation_chain(model, head, 0) {
                depth_check(depth)?;
                let mut path: Vec<&str> = chain.iter().map(|(link, _)| link.as_str()).collect();
                path.push(head);
                let rewritten = Term {
                    field: format!("{}{}", path.join("."), &term.field[head.len()..]),
                    op: term.op.clone(),
                    value: term.value.clone(),
                };
                return render_term(&rewritten, params, ctx, depth + 1);
            }
        }
    }
    // a related field is a name for a path: rewrite it and let the path
    // machinery below do the joining, exactly like a delegated field
    if let Some(model) = ctx.model {
        let head = term.field.split('.').next().unwrap_or(&term.field);
        if let Some(path) = model.field(head).and_then(|f| f.related.clone()) {
            depth_check(depth)?;
            let rewritten = Term {
                field: format!("{path}{}", &term.field[head.len()..]),
                op: term.op.clone(),
                value: term.value.clone(),
            };
            return render_term(&rewritten, params, ctx, depth + 1);
        }
    }
    if let Some((head, rest)) = term.field.split_once('.') {
        return render_path(head, rest, term, params, ctx, depth);
    }
    if matches!(term.op, Operator::Any | Operator::NotAny) {
        return render_any(term, params, ctx, depth);
    }
    if matches!(term.op, Operator::ChildOf | Operator::ParentOf) {
        return render_hierarchy(term, params, ctx);
    }
    // with a model in context unknown fields fail fast, and x2many
    // equality/membership goes through the relation, not a column
    if let Some(model) = ctx.model {
        if term.field != "id" {
            let field = model.field(&term.field).ok_or_else(|| {
                RusdooError::Validation(format!(
                    "unknown field on {}: {:?}",
                    model.meta.name, term.field
                ))
            })?;
            // a computed field has no column and no path to rewrite to:
            // Odoo needs an explicit `search` method for that, so say what
            // is missing instead of naming a column that does not exist
            if field.compute.is_some() && !field.stored {
                return Err(RusdooError::Validation(format!(
                    "field {:?} is computed and not stored: it cannot be searched",
                    term.field
                )));
            }
            if matches!(
                field.ty,
                FieldType::Many2many { .. } | FieldType::One2many { .. }
            ) && matches!(
                term.op,
                Operator::Eq | Operator::Neq | Operator::In | Operator::NotIn
            ) {
                return render_x2many_in(field, term, params, ctx);
            }
        }
    }

    let col = column_expr(&term.field, ctx)?;
    let value = &term.value;
    let cast = value_cast(&term.field, ctx);
    use Operator::*;

    // Odoo rewrites '='/'!=' with a collection value into 'in'/'not in'
    if value.is_array() && matches!(term.op, Eq | Neq) {
        return render_in(&col, value, term.op == Neq, params, cast);
    }

    match &term.op {
        Eq => Ok(if is_unset(value) {
            match falsy_value(&term.field, ctx) {
                Some(fv) => format!("({col} = {p} OR {col} IS NULL)", p = bind(params, fv)),
                None => format!("{col} IS NULL"),
            }
        } else {
            format!("{col} = {}{cast}", bind(params, value.clone()))
        }),
        Neq => Ok(if is_unset(value) {
            match falsy_value(&term.field, ctx) {
                Some(fv) => format!("({col} != {p} AND {col} IS NOT NULL)", p = bind(params, fv)),
                None => format!("{col} IS NOT NULL"),
            }
        } else {
            // negative operators also match unset records
            format!(
                "({col} != {p}{cast} OR {col} IS NULL)",
                p = bind(params, value.clone())
            )
        }),
        EqMaybe => Ok(if is_unset(value) {
            "TRUE".into()
        } else {
            format!("{col} = {}{cast}", bind(params, value.clone()))
        }),
        Lt => render_cmp(&col, "<", value, params, cast),
        Lte => render_cmp(&col, "<=", value, params, cast),
        Gt => render_cmp(&col, ">", value, params, cast),
        Gte => render_cmp(&col, ">=", value, params, cast),
        In | NotIn => render_in(&col, value, term.op == NotIn, params, cast),
        Like => render_like(&col, "LIKE", value, true, false, params),
        ILike => render_like(&col, "ILIKE", value, true, false, params),
        NotLike => render_like(&col, "LIKE", value, true, true, params),
        NotILike => render_like(&col, "ILIKE", value, true, true, params),
        EqLike => render_like(&col, "LIKE", value, false, false, params),
        EqILike => render_like(&col, "ILIKE", value, false, false, params),
        NotEqLike => render_like(&col, "LIKE", value, false, true, params),
        NotEqILike => render_like(&col, "ILIKE", value, false, true, params),
        ChildOf | ParentOf | Any | NotAny => unreachable!("handled above"),
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
        FieldType::Many2many {
            comodel: co_name,
            relation,
            column1,
            column2,
        } => {
            let co = comodel(registry, co_name)?;
            let sub = render_term(&inner, params, ctx.with_model(co), depth + 1)?;
            let (rel, c1, c2) = (
                quote_ident(relation)?,
                quote_ident(column1)?,
                quote_ident(column2)?,
            );
            Ok(format!(
                r#""id" IN (SELECT {c1} FROM {rel} WHERE {c2} IN (SELECT "id" FROM {} WHERE {sub}))"#,
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
        FieldType::Many2many {
            comodel: co_name,
            relation,
            column1,
            column2,
        } => {
            let co = comodel(registry, co_name)?;
            let sub = render_at(&sub_domain, params, ctx.with_model(co), depth + 1)?;
            let (rel, c1, c2) = (
                quote_ident(relation)?,
                quote_ident(column1)?,
                quote_ident(column2)?,
            );
            let inner = format!(
                r#"SELECT {c1} FROM {rel} WHERE {c2} IN (SELECT "id" FROM {} WHERE {sub})"#,
                quote_ident(&co.meta.table)?
            );
            Ok(if negative {
                format!(r#""id" NOT IN ({inner})"#)
            } else {
                format!(r#""id" IN ({inner})"#)
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
    cast: &str,
) -> Result<String, RusdooError> {
    Ok(format!(
        "{col} {sql_op} {}{cast}",
        bind(params, value.clone())
    ))
}

fn render_in(
    col: &str,
    value: &Value,
    negative: bool,
    params: &mut Vec<Value>,
    cast: &str,
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
        .map(|v| format!("{}{cast}", bind(params, v.clone())))
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

/// `["tag_ids", "in", [ids]]` -> membership through the relation table
/// (many2many) or the inverse column (one2many).
fn render_x2many_in(
    field: &Field,
    term: &Term,
    params: &mut Vec<Value>,
    ctx: Ctx,
) -> Result<String, RusdooError> {
    let ids = match &term.value {
        Value::Array(items) => items.clone(),
        single => vec![single.clone()],
    };
    let negative = matches!(term.op, Operator::Neq | Operator::NotIn);
    if ids.is_empty() {
        return Ok(if negative {
            "TRUE".into()
        } else {
            "FALSE".into()
        });
    }
    let placeholders: Vec<String> = ids.into_iter().map(|v| bind(params, v)).collect();
    let list = placeholders.join(", ");
    let inner = match &field.ty {
        FieldType::Many2many {
            relation,
            column1,
            column2,
            ..
        } => {
            let (rel, c1, c2) = (
                quote_ident(relation)?,
                quote_ident(column1)?,
                quote_ident(column2)?,
            );
            format!("SELECT {c1} FROM {rel} WHERE {c2} IN ({list})")
        }
        FieldType::One2many {
            comodel: co_name,
            inverse,
        } => {
            let registry = ctx.registry.ok_or_else(|| {
                RusdooError::Validation("one2many membership requires registry context".into())
            })?;
            let co = comodel(registry, co_name)?;
            let inv = quote_ident(inverse)?;
            // NULL inverse rows would poison NOT IN
            let guard = if negative {
                format!(" AND {inv} IS NOT NULL")
            } else {
                String::new()
            };
            format!(
                r#"SELECT {inv} FROM {} WHERE "id" IN ({list}){guard}"#,
                quote_ident(&co.meta.table)?
            )
        }
        _ => unreachable!("caller checked the field is x2many"),
    };
    Ok(if negative {
        format!(r#""id" NOT IN ({inner})"#)
    } else {
        format!(r#""id" IN ({inner})"#)
    })
}

/// `child_of`/`parent_of`: walk a `parent_id` hierarchy (Odoo's default
/// `_parent_name`) with a recursive CTE, on this model (`field == "id"`)
/// or on a many2one's comodel.
fn render_hierarchy(term: &Term, params: &mut Vec<Value>, ctx: Ctx) -> Result<String, RusdooError> {
    let (model, registry) = require_env(ctx, "hierarchical operator")?;
    let target = if term.field == "id" {
        model
    } else {
        let field = model.field(&term.field).ok_or_else(|| {
            RusdooError::Validation(format!(
                "unknown field on {}: {:?}",
                model.meta.name, term.field
            ))
        })?;
        match &field.ty {
            FieldType::Many2one { comodel: co_name } => comodel(registry, co_name)?,
            other => {
                return Err(RusdooError::Validation(format!(
                    "hierarchical operator not supported on field type {other:?}"
                )))
            }
        }
    };
    if target.field("parent_id").is_none() {
        return Err(RusdooError::Validation(format!(
            "hierarchical operator requires a parent_id field on {}",
            target.meta.name
        )));
    }
    let ids = match &term.value {
        Value::Array(items) => items.clone(),
        single => vec![single.clone()],
    };
    if ids.is_empty() {
        // Odoo resolves an empty id set to the FALSE domain
        return Ok("FALSE".into());
    }
    if !ids.iter().all(Value::is_number) {
        return Err(RusdooError::Validation(format!(
            "hierarchical operator expects record ids, got {}",
            term.value
        )));
    }
    let placeholders: Vec<String> = ids.into_iter().map(|v| bind(params, v)).collect();
    let list = placeholders.join(", ");
    // Odoo remaps self-referential traversal (parent_id on the same model)
    // to the id column: child_of on parent_id == child_of on id
    let col = if term.field != "id" && target.meta.name == model.meta.name {
        quote_ident("id")?
    } else {
        quote_ident(&term.field)?
    };
    let table = quote_ident(&target.meta.table)?;
    let cte = if term.op == Operator::ChildOf {
        format!(
            r#"WITH RECURSIVE __rusdoo_tree AS (SELECT "id" FROM {table} WHERE "id" IN ({list}) UNION SELECT __t."id" FROM {table} __t JOIN __rusdoo_tree __r ON __t."parent_id" = __r."id") SELECT "id" FROM __rusdoo_tree"#
        )
    } else {
        format!(
            r#"WITH RECURSIVE __rusdoo_tree AS (SELECT "id", "parent_id" FROM {table} WHERE "id" IN ({list}) UNION SELECT __t."id", __t."parent_id" FROM {table} __t JOIN __rusdoo_tree __r ON __t."id" = __r."parent_id") SELECT "id" FROM __rusdoo_tree"#
        )
    };
    Ok(format!("{col} IN ({cte})"))
}

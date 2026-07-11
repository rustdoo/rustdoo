//! rusdoo-qweb — the QWeb XML template engine (port of
//! `odoo/addons/base/models/ir_qweb.py`), rendering a template string to
//! HTML against a JSON context.
//!
//! Supported directives: `t-esc`/`t-out`, `t-if`/`t-else`,
//! `t-foreach`/`t-as`, `t-att-*`, `t-field`, and the transparent `<t>`
//! element, plus `t-call` (named sub-templates via `render_with`) and
//! `t-set`, `t-elif`, `t-attf-*`. Not yet ported: `t-field` widgets.

mod expr;

use quick_xml::events::Event;
use quick_xml::Reader;
use rusdoo_core::RusdooError;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;

/// Named templates available to `t-call` (raw source).
pub type Templates = HashMap<String, String>;

/// Templates parsed once for a render pass, so t-call never re-parses.
type Compiled = HashMap<String, Vec<Node>>;

/// The `t-call` targets referenced anywhere in a template — used by
/// callers to resolve (and access-check) only the templates in use.
pub fn t_call_refs(template: &str) -> Result<Vec<String>, RusdooError> {
    let nodes = parse(template)?;
    let mut refs = Vec::new();
    collect_t_calls(&nodes, &mut refs);
    Ok(refs)
}

fn collect_t_calls(nodes: &[Node], out: &mut Vec<String>) {
    for node in nodes {
        if let Node::Element(el) = node {
            if let Some(name) = el.attr("t-call") {
                out.push(name.to_string());
            }
            collect_t_calls(&el.children, out);
        }
    }
}

const MAX_RENDER_DEPTH: usize = 100;

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Element(Element),
}

#[derive(Debug, Clone)]
struct Element {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Render `template` against `context`, returning HTML.
pub fn render(template: &str, context: &Value) -> Result<String, RusdooError> {
    render_with(template, context, &Templates::new())
}

/// Render with a set of named templates available to `t-call`.
pub fn render_with(
    template: &str,
    context: &Value,
    templates: &Templates,
) -> Result<String, RusdooError> {
    let mut compiled = Compiled::new();
    for (name, source) in templates {
        compiled.insert(name.clone(), parse(source)?);
    }
    let nodes = parse(template)?;
    let mut out = String::new();
    render_nodes(&nodes, context, &mut out, 0, &compiled)?;
    Ok(out)
}

// ---------- parsing ----------

fn parse(src: &str) -> Result<Vec<Node>, RusdooError> {
    let mut reader = Reader::from_str(src);
    let mut stack: Vec<Element> = Vec::new();
    let mut roots: Vec<Node> = Vec::new();

    fn push_node(stack: &mut [Element], roots: &mut Vec<Node>, node: Node) {
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => roots.push(node),
        }
    }

    loop {
        match reader.read_event().map_err(qweb_err)? {
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_) => {}
            Event::Start(el) => stack.push(element_from(&el)?),
            Event::Empty(el) => {
                let node = Node::Element(element_from(&el)?);
                push_node(&mut stack, &mut roots, node);
            }
            Event::End(_) => {
                let done = stack
                    .pop()
                    .ok_or_else(|| RusdooError::Validation("qweb: stray end tag".into()))?;
                push_node(&mut stack, &mut roots, Node::Element(done));
            }
            // keep literal template text verbatim (entities not re-escaped)
            Event::Text(text) => {
                let raw = String::from_utf8_lossy(text.as_ref()).into_owned();
                push_node(&mut stack, &mut roots, Node::Text(raw));
            }
            Event::CData(cdata) => {
                let raw = String::from_utf8_lossy(&cdata.into_inner()).into_owned();
                push_node(&mut stack, &mut roots, Node::Text(raw));
            }
            // a general entity ref (&name;) — keep it as literal template text
            Event::GeneralRef(r) => {
                let name = String::from_utf8_lossy(r.as_ref()).into_owned();
                push_node(&mut stack, &mut roots, Node::Text(format!("&{name};")));
            }
        }
    }
    if !stack.is_empty() {
        return Err(RusdooError::Validation("qweb: unclosed element".into()));
    }
    Ok(roots)
}

fn element_from(el: &quick_xml::events::BytesStart) -> Result<Element, RusdooError> {
    let tag = String::from_utf8_lossy(el.name().as_ref()).into_owned();
    let mut attrs = Vec::new();
    for attr in el.attributes() {
        let attr = attr.map_err(qweb_err)?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let raw = String::from_utf8_lossy(&attr.value);
        let value = quick_xml::escape::unescape(&raw)
            .map_err(qweb_err)?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(Element {
        tag,
        attrs,
        children: Vec::new(),
    })
}

fn qweb_err(e: impl std::fmt::Display) -> RusdooError {
    RusdooError::Validation(format!("qweb: {e}"))
}

// ---------- rendering ----------

/// Render a sequence of sibling nodes, wiring t-if/t-else chains.
fn render_nodes(
    nodes: &[Node],
    ctx: &Value,
    out: &mut String,
    depth: usize,
    templates: &Compiled,
) -> Result<(), RusdooError> {
    if depth > MAX_RENDER_DEPTH {
        return Err(RusdooError::Validation(
            "qweb: template nested too deep".into(),
        ));
    }
    // t-set defines variables visible to later siblings; accumulate them
    // in an overlay applied on top of the incoming context
    let mut overlay: Option<Map<String, Value>> = None;
    let mut last_if: Option<bool> = None;
    for node in nodes {
        let effective: Cow<Value> = match &overlay {
            None => Cow::Borrowed(ctx),
            Some(vars) => {
                let mut merged = ctx.as_object().cloned().unwrap_or_default();
                for (k, v) in vars {
                    merged.insert(k.clone(), v.clone());
                }
                Cow::Owned(Value::Object(merged))
            }
        };
        let cx: &Value = &effective;
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Element(el) => {
                if let Some(var) = el.attr("t-set") {
                    let value = if let Some(v) = el.attr("t-value") {
                        expr::eval(v, cx)?
                    } else {
                        let mut buf = String::new();
                        render_nodes(&el.children, cx, &mut buf, depth + 1, templates)?;
                        Value::String(buf)
                    };
                    overlay
                        .get_or_insert_with(Map::new)
                        .insert(var.to_string(), value);
                    continue;
                }
                let has_foreach = el.attr("t-foreach").is_some();
                if !has_foreach && el.attr("t-else").is_some() {
                    if last_if == Some(false) {
                        render_body(el, cx, out, depth, templates)?;
                    }
                    last_if = None;
                } else if let (false, Some(cond)) = (has_foreach, el.attr("t-elif")) {
                    // only evaluated when no earlier branch in the chain won
                    if last_if == Some(false) {
                        let taken = expr::truthy(&expr::eval(cond, cx)?);
                        if taken {
                            render_body(el, cx, out, depth, templates)?;
                        }
                        last_if = Some(taken);
                    }
                } else if let (false, Some(cond)) = (has_foreach, el.attr("t-if")) {
                    let taken = expr::truthy(&expr::eval(cond, cx)?);
                    if taken {
                        render_body(el, cx, out, depth, templates)?;
                    }
                    last_if = Some(taken);
                } else {
                    render_element(el, cx, out, depth, templates)?;
                    last_if = None;
                }
            }
        }
    }
    Ok(())
}

/// Render one element, applying t-foreach then delegating to the body.
fn render_element(
    el: &Element,
    ctx: &Value,
    out: &mut String,
    depth: usize,
    templates: &Compiled,
) -> Result<(), RusdooError> {
    if let Some(foreach) = el.attr("t-foreach") {
        let as_name = el
            .attr("t-as")
            .ok_or_else(|| RusdooError::Validation("qweb: t-foreach requires t-as".into()))?;
        let items: Vec<Value> = match expr::eval(foreach, ctx)? {
            Value::Array(items) => items,
            Value::Object(map) => map.into_iter().map(|(_, v)| v).collect(),
            Value::Null => Vec::new(),
            other => {
                return Err(RusdooError::Validation(format!(
                    "qweb: t-foreach expects a collection, got {other}"
                )))
            }
        };
        let size = items.len();
        for (index, item) in items.into_iter().enumerate() {
            let mut child = ctx.as_object().cloned().unwrap_or_default();
            child.insert(as_name.to_string(), item);
            child.insert(format!("{as_name}_index"), Value::from(index));
            child.insert(format!("{as_name}_size"), Value::from(size));
            child.insert(format!("{as_name}_first"), Value::from(index == 0));
            child.insert(format!("{as_name}_last"), Value::from(index + 1 == size));
            render_body(el, &Value::Object(child), out, depth, templates)?;
        }
        return Ok(());
    }
    if let Some(cond) = el.attr("t-if") {
        if !expr::truthy(&expr::eval(cond, ctx)?) {
            return Ok(());
        }
    }
    render_body(el, ctx, out, depth, templates)
}

/// Emit the element's tag, attributes and content (t-if/t-foreach are
/// assumed already handled by the caller).
fn render_body(
    el: &Element,
    ctx: &Value,
    out: &mut String,
    depth: usize,
    templates: &Compiled,
) -> Result<(), RusdooError> {
    let transparent = el.tag == "t";
    if !transparent {
        out.push('<');
        out.push_str(&el.tag);
        for (key, value) in &el.attrs {
            if let Some(name) = key.strip_prefix("t-attf-") {
                // format-string attribute: interpolate {{ expr }} runs
                let formatted = format_attf(value, ctx)?;
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                out.push_str(&escape_attr(&formatted));
                out.push('"');
            } else if let Some(name) = key.strip_prefix("t-att-") {
                let resolved = expr::eval(value, ctx)?;
                if !matches!(resolved, Value::Null | Value::Bool(false)) {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    out.push_str(&escape_attr(&value_to_string(&resolved)));
                    out.push('"');
                }
            } else if !key.starts_with("t-") {
                out.push(' ');
                out.push_str(key);
                out.push_str("=\"");
                out.push_str(&escape_attr(value));
                out.push('"');
            }
        }
        out.push('>');
    }

    if let Some(name) = el.attr("t-call") {
        let nodes = templates.get(name).ok_or_else(|| {
            RusdooError::Validation(format!("qweb: unknown template {name:?} (t-call)"))
        })?;
        render_nodes(nodes, ctx, out, depth + 1, templates)?;
    } else if let Some(e) = el.attr("t-esc") {
        out.push_str(&escape_text(&value_to_string(&expr::eval(e, ctx)?)));
    } else if let Some(e) = el.attr("t-out") {
        out.push_str(&value_to_string(&expr::eval(e, ctx)?));
    } else if let Some(e) = el.attr("t-field") {
        out.push_str(&escape_text(&field_display(&expr::eval(e, ctx)?)));
    } else {
        render_nodes(&el.children, ctx, out, depth + 1, templates)?;
    }

    if !transparent {
        out.push_str("</");
        out.push_str(&el.tag);
        out.push('>');
    }
    Ok(())
}

/// Interpolate `{{ expr }}` runs in a `t-attf-*` attribute template.
fn format_attf(template: &str, ctx: &Value) -> Result<String, RusdooError> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| RusdooError::Validation("qweb: unterminated {{ in t-attf".into()))?;
        let value = expr::eval(after[..end].trim(), ctx)?;
        out.push_str(&value_to_string(&value));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Render a record field: a many2one read comes as `[id, display_name]`,
/// so show the name; anything else renders like its value.
fn field_display(value: &Value) -> String {
    if let Value::Array(pair) = value {
        if let [id, name] = pair.as_slice() {
            if id.is_number() {
                if let Some(display) = name.as_str() {
                    return display.to_string();
                }
            }
        }
    }
    value_to_string(value)
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

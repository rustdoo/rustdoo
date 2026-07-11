//! rusdoo-qweb — the QWeb XML template engine (port of
//! `odoo/addons/base/models/ir_qweb.py`), rendering a template string to
//! HTML against a JSON context.
//!
//! Supported directives: `t-esc`/`t-out`, `t-if`/`t-else`,
//! `t-foreach`/`t-as`, `t-att-*`, `t-field`, and the transparent `<t>`
//! element. Not yet ported: `t-call`, `t-set`, `t-elif`, `t-attf-*`.

mod expr;

use quick_xml::events::Event;
use quick_xml::Reader;
use rusdoo_core::RusdooError;
use serde_json::Value;

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
    let nodes = parse(template)?;
    let mut out = String::new();
    render_nodes(&nodes, context, &mut out, 0)?;
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
        let value = attr.unescape_value().map_err(qweb_err)?.into_owned();
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
) -> Result<(), RusdooError> {
    if depth > MAX_RENDER_DEPTH {
        return Err(RusdooError::Validation(
            "qweb: template nested too deep".into(),
        ));
    }
    let mut last_if: Option<bool> = None;
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Element(el) => {
                let has_foreach = el.attr("t-foreach").is_some();
                if !has_foreach && el.attr("t-else").is_some() {
                    if last_if == Some(false) {
                        render_body(el, ctx, out, depth)?;
                    }
                    last_if = None;
                } else if let (false, Some(cond)) = (has_foreach, el.attr("t-if")) {
                    let taken = expr::truthy(&expr::eval(cond, ctx)?);
                    if taken {
                        render_body(el, ctx, out, depth)?;
                    }
                    last_if = Some(taken);
                } else {
                    render_element(el, ctx, out, depth)?;
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
            render_body(el, &Value::Object(child), out, depth)?;
        }
        return Ok(());
    }
    if let Some(cond) = el.attr("t-if") {
        if !expr::truthy(&expr::eval(cond, ctx)?) {
            return Ok(());
        }
    }
    render_body(el, ctx, out, depth)
}

/// Emit the element's tag, attributes and content (t-if/t-foreach are
/// assumed already handled by the caller).
fn render_body(
    el: &Element,
    ctx: &Value,
    out: &mut String,
    depth: usize,
) -> Result<(), RusdooError> {
    let transparent = el.tag == "t";
    if !transparent {
        out.push('<');
        out.push_str(&el.tag);
        for (key, value) in &el.attrs {
            if let Some(name) = key.strip_prefix("t-att-") {
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

    if let Some(e) = el.attr("t-esc") {
        out.push_str(&escape_text(&value_to_string(&expr::eval(e, ctx)?)));
    } else if let Some(e) = el.attr("t-out") {
        out.push_str(&value_to_string(&expr::eval(e, ctx)?));
    } else if let Some(e) = el.attr("t-field") {
        out.push_str(&escape_text(&field_display(&expr::eval(e, ctx)?)));
    } else {
        render_nodes(&el.children, ctx, out, depth + 1)?;
    }

    if !transparent {
        out.push_str("</");
        out.push_str(&el.tag);
        out.push('>');
    }
    Ok(())
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

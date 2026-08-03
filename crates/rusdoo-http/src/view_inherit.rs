//! View inheritance, port of `ir.ui.view`'s `inherit_id` + `xpath`
//! (`odoo/addons/base/models/ir_ui_view.py`).
//!
//! A module that wants two buttons on somebody else's form should not
//! have to republish that form. It publishes a patch instead: a view
//! that names the one it extends and says where its content goes.
//!
//! What is supported is a deliberate subset of Odoo's:
//!
//! * `<xpath expr="//field[@name='partner_id']" position="after">…`
//! * the shorthand `<field name="partner_id" position="after">…`
//! * positions `after`, `before`, `inside`, `replace`, `attributes`
//!
//! An element is located by its tag and its `name`, which is how real
//! Odoo inheritance addresses things. A full XPath engine would accept
//! `//div[3]/span[last()]`, and a patch written against a position in
//! somebody else's arch breaks the first time they add a line — so the
//! subset is not only cheaper, it is the part that survives.
//!
//! A patch that matches nothing is an error, never a silent no-op: the
//! module meant to change that screen, and a button that quietly failed
//! to appear is a bug report from a user, months later.

use quick_xml::events::Event;
use quick_xml::Reader;
use rusdoo_core::RusdooError;

/// Where a patch's content goes, relative to the element it located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    After,
    Before,
    Inside,
    Replace,
    Attributes,
}

impl Position {
    fn parse(value: &str) -> Result<Position, RusdooError> {
        Ok(match value {
            "after" => Position::After,
            "before" => Position::Before,
            "inside" => Position::Inside,
            "replace" => Position::Replace,
            "attributes" => Position::Attributes,
            other => {
                return Err(RusdooError::Validation(format!(
                    "position {other:?} não existe: use after, before, inside, replace ou attributes"
                )))
            }
        })
    }
}

/// One instruction of a patch: what to find, and what to do there.
#[derive(Debug)]
struct Op {
    tag: String,
    name: Option<String>,
    position: Position,
    /// the XML the instruction carries, already serialized
    content: String,
}

/// Apply `patch` to `base`, answering the arch the client should get.
pub fn apply_inheritance(base: &str, patch: &str) -> Result<String, RusdooError> {
    let mut arch = base.to_string();
    for op in parse_patch(patch)? {
        arch = apply(&arch, &op)?;
    }
    Ok(arch)
}

/// Read the instructions out of a patch arch: every child of its root.
fn parse_patch(patch: &str) -> Result<Vec<Op>, RusdooError> {
    let mut reader = Reader::from_str(patch);
    reader.config_mut().trim_text(false);
    let mut ops = Vec::new();
    let mut depth = 0usize;
    let mut last = 0usize;
    loop {
        let before = last;
        let event = reader
            .read_event()
            .map_err(|error| RusdooError::Validation(format!("patch de view inválido: {error}")))?;
        last = reader.buffer_position() as usize;
        match event {
            Event::Start(start) => {
                depth += 1;
                if depth != 2 {
                    continue;
                }
                let (tag, name, position) = locator(&start, patch)?;
                let Some(position) = position else {
                    return Err(RusdooError::Validation(format!(
                        "o elemento {tag:?} do patch não diz `position`"
                    )));
                };
                // the instruction's own children, verbatim
                let inner_start = last;
                let end = skip_to_end(&mut reader, &start)?;
                let content = patch.get(inner_start..end).unwrap_or_default().to_string();
                last = reader.buffer_position() as usize;
                // the element was consumed whole, closing tag included:
                // without this the next instruction looks nested and is
                // skipped, and only the first patch of a view applies
                depth -= 1;
                ops.push(Op {
                    tag,
                    name,
                    position,
                    content,
                });
            }
            Event::Empty(start) if depth == 1 => {
                // a self-closing instruction carries nothing; only
                // `replace` (delete) means anything without content
                let (tag, name, position) = locator(&start, patch)?;
                let Some(position) = position else {
                    let _ = before;
                    continue;
                };
                ops.push(Op {
                    tag,
                    name,
                    position,
                    content: String::new(),
                });
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Eof => break,
            _ => {}
        }
    }
    if ops.is_empty() {
        return Err(RusdooError::Validation(
            "o patch não tem nenhuma instrução com `position`".into(),
        ));
    }
    Ok(ops)
}

/// What an instruction addresses: its tag, its `name`, its `position`.
/// `<xpath expr="//field[@name='x']">` is read into the same three.
fn locator(
    start: &quick_xml::events::BytesStart,
    source: &str,
) -> Result<(String, Option<String>, Option<Position>), RusdooError> {
    let tag = String::from_utf8_lossy(start.name().as_ref()).to_string();
    let mut name = attribute(start, "name");
    let position = match attribute(start, "position") {
        Some(value) => Some(Position::parse(&value)?),
        None => None,
    };
    if tag == "xpath" {
        let expr = attribute(start, "expr").ok_or_else(|| {
            RusdooError::Validation("um <xpath> do patch não diz `expr`".into())
        })?;
        let (xtag, xname) = parse_expr(&expr)?;
        let _ = source;
        return Ok((xtag, xname.or(name), position));
    }
    if tag == "attribute" {
        name = attribute(start, "name");
    }
    Ok((tag, name, position))
}

/// The subset of XPath this understands: `//tag`, `//tag[@name='x']`.
fn parse_expr(expr: &str) -> Result<(String, Option<String>), RusdooError> {
    let trimmed = expr.trim().trim_start_matches('/');
    let (tag, rest) = match trimmed.split_once('[') {
        Some((tag, rest)) => (tag, Some(rest)),
        None => (trimmed, None),
    };
    if tag.is_empty() || tag.contains('/') {
        return Err(RusdooError::Validation(format!(
            "expr {expr:?} não é suportado: use //tag ou //tag[@name='x']"
        )));
    }
    let Some(rest) = rest else {
        return Ok((tag.to_string(), None));
    };
    let predicate = rest.trim_end_matches(']');
    let name = predicate
        .split_once('=')
        .map(|(_, value)| value.trim().trim_matches(['\'', '"']).to_string())
        .filter(|_| predicate.starts_with("@name"));
    match name {
        Some(name) => Ok((tag.to_string(), Some(name))),
        None => Err(RusdooError::Validation(format!(
            "expr {expr:?} não é suportado: só o predicado [@name='...'] existe aqui"
        ))),
    }
}

fn attribute(start: &quick_xml::events::BytesStart, wanted: &str) -> Option<String> {
    start.attributes().flatten().find_map(|attribute| {
        (attribute.key.as_ref() == wanted.as_bytes())
            .then(|| String::from_utf8_lossy(&attribute.value).to_string())
    })
}

/// Read past the element `start` opened, answering where its content
/// ends (the offset just before its closing tag).
fn skip_to_end(
    reader: &mut Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
) -> Result<usize, RusdooError> {
    let name = start.name().as_ref().to_vec();
    let mut depth = 1usize;
    let mut content_end = reader.buffer_position() as usize;
    loop {
        let before = reader.buffer_position() as usize;
        match reader
            .read_event()
            .map_err(|error| RusdooError::Validation(format!("patch de view inválido: {error}")))?
        {
            Event::Start(inner) if inner.name().as_ref() == name => depth += 1,
            Event::End(inner) if inner.name().as_ref() == name => {
                depth -= 1;
                if depth == 0 {
                    return Ok(before);
                }
            }
            Event::Eof => return Ok(content_end),
            _ => {}
        }
        content_end = reader.buffer_position() as usize;
    }
}

/// Where an element sits in an arch: its whole span, and its content's.
struct Span {
    /// the `<tag ...>` itself
    open: (usize, usize),
    /// everything between the tags
    inner: (usize, usize),
    /// the whole element, closing tag included
    whole: (usize, usize),
}

/// Find the first element of `tag` (with `name`, when given).
fn locate(arch: &str, tag: &str, name: Option<&str>) -> Result<Span, RusdooError> {
    let mut reader = Reader::from_str(arch);
    reader.config_mut().trim_text(false);
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .map_err(|error| RusdooError::Validation(format!("arch inválido: {error}")))?;
        let after = reader.buffer_position() as usize;
        let (start, empty) = match &event {
            Event::Start(start) => (start.clone(), false),
            Event::Empty(start) => (start.clone(), true),
            Event::Eof => break,
            _ => continue,
        };
        if String::from_utf8_lossy(start.name().as_ref()) != tag {
            continue;
        }
        if let Some(wanted) = name {
            if attribute(&start, "name").as_deref() != Some(wanted) {
                continue;
            }
        }
        if empty {
            return Ok(Span {
                open: (before, after),
                inner: (after, after),
                whole: (before, after),
            });
        }
        let content_end = skip_to_end(&mut reader, &start)?;
        return Ok(Span {
            open: (before, after),
            inner: (after, content_end),
            whole: (before, reader.buffer_position() as usize),
        });
    }
    Err(RusdooError::Validation(match name {
        Some(name) => format!("o patch procura <{tag} name=\"{name}\"> e o arch não tem"),
        None => format!("o patch procura <{tag}> e o arch não tem"),
    }))
}

fn apply(arch: &str, op: &Op) -> Result<String, RusdooError> {
    let span = locate(arch, &op.tag, op.name.as_deref())?;
    let mut out = String::with_capacity(arch.len() + op.content.len());
    match op.position {
        Position::Before => {
            out.push_str(&arch[..span.whole.0]);
            out.push_str(&op.content);
            out.push_str(&arch[span.whole.0..]);
        }
        Position::After => {
            out.push_str(&arch[..span.whole.1]);
            out.push_str(&op.content);
            out.push_str(&arch[span.whole.1..]);
        }
        Position::Inside => {
            out.push_str(&arch[..span.inner.1]);
            out.push_str(&op.content);
            out.push_str(&arch[span.inner.1..]);
        }
        Position::Replace => {
            out.push_str(&arch[..span.whole.0]);
            out.push_str(&op.content);
            out.push_str(&arch[span.whole.1..]);
        }
        Position::Attributes => {
            let open = &arch[span.open.0..span.open.1];
            let patched = patch_attributes(open, &op.content)?;
            out.push_str(&arch[..span.open.0]);
            out.push_str(&patched);
            out.push_str(&arch[span.open.1..]);
        }
    }
    Ok(out)
}

/// Rewrite an opening tag from `<attribute name="x">value</attribute>`
/// instructions. An empty value removes the attribute, like Odoo.
fn patch_attributes(open: &str, content: &str) -> Result<String, RusdooError> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut changes: Vec<(String, String)> = Vec::new();
    let mut pending: Option<String> = None;
    loop {
        match reader
            .read_event()
            .map_err(|error| RusdooError::Validation(format!("patch inválido: {error}")))?
        {
            Event::Start(start) if start.name().as_ref() == b"attribute" => {
                pending = attribute(&start, "name");
            }
            Event::Text(text) => {
                if let Some(name) = pending.take() {
                    let value = text.decode().map_err(|error| {
                        RusdooError::Validation(format!("patch inválido: {error}"))
                    })?;
                    changes.push((name, value.to_string()));
                }
            }
            Event::End(end) if end.name().as_ref() == b"attribute" => {
                // an <attribute name="x"/> with no text removes it
                if let Some(name) = pending.take() {
                    changes.push((name, String::new()));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if changes.is_empty() {
        return Err(RusdooError::Validation(
            "position=\"attributes\" sem nenhum <attribute name=\"...\">".into(),
        ));
    }

    let mut reader = Reader::from_str(open);
    let start = match reader.read_event() {
        Ok(Event::Start(start)) | Ok(Event::Empty(start)) => start,
        _ => {
            return Err(RusdooError::Validation(
                "não foi possível reescrever os atributos do elemento".into(),
            ))
        }
    };
    let tag = String::from_utf8_lossy(start.name().as_ref()).to_string();
    let mut attributes: Vec<(String, String)> = start
        .attributes()
        .flatten()
        .map(|attribute| {
            (
                String::from_utf8_lossy(attribute.key.as_ref()).to_string(),
                String::from_utf8_lossy(&attribute.value).to_string(),
            )
        })
        .collect();
    for (name, value) in changes {
        attributes.retain(|(existing, _)| *existing != name);
        if !value.is_empty() {
            attributes.push((name, value));
        }
    }
    let rendered: String = attributes
        .iter()
        .map(|(name, value)| format!(" {name}=\"{}\"", escape(value)))
        .collect();
    let closing = if open.trim_end().ends_with("/>") { "/>" } else { ">" };
    Ok(format!("<{tag}{rendered}{closing}"))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORM: &str = r#"<form><group><field name="name"/><field name="email"/></group></form>"#;

    #[test]
    fn a_patch_puts_content_after_the_field_it_names() {
        let patched = apply_inheritance(
            FORM,
            r#"<data><field name="name" position="after"><field name="apelido"/></field></data>"#,
        )
        .unwrap();
        assert_eq!(
            patched,
            r#"<form><group><field name="name"/><field name="apelido"/><field name="email"/></group></form>"#
        );
    }

    #[test]
    fn the_xpath_form_says_the_same_thing() {
        let patched = apply_inheritance(
            FORM,
            r#"<data><xpath expr="//field[@name='email']" position="before"><field name="telefone"/></xpath></data>"#,
        )
        .unwrap();
        assert!(patched.contains(r#"<field name="telefone"/><field name="email"/>"#));
    }

    #[test]
    fn inside_appends_to_the_element_it_found() {
        let patched = apply_inheritance(
            FORM,
            r#"<data><xpath expr="//group" position="inside"><field name="cidade"/></xpath></data>"#,
        )
        .unwrap();
        assert!(patched.contains(r#"<field name="email"/><field name="cidade"/></group>"#));
    }

    #[test]
    fn replace_swaps_the_element_and_an_empty_patch_deletes_it() {
        let patched = apply_inheritance(
            FORM,
            r#"<data><field name="email" position="replace"><field name="contato"/></field></data>"#,
        )
        .unwrap();
        assert!(patched.contains(r#"<field name="contato"/>"#));
        assert!(!patched.contains(r#"name="email""#));

        let removed = apply_inheritance(
            FORM,
            r#"<data><field name="email" position="replace"></field></data>"#,
        )
        .unwrap();
        assert!(!removed.contains("email"), "{removed}");
    }

    #[test]
    fn attributes_rewrites_the_opening_tag() {
        let patched = apply_inheritance(
            FORM,
            r#"<data><field name="email" position="attributes"><attribute name="string">E-mail do contato</attribute><attribute name="readonly">1</attribute></field></data>"#,
        )
        .unwrap();
        assert!(patched.contains(r#"string="E-mail do contato""#), "{patched}");
        assert!(patched.contains(r#"readonly="1""#), "{patched}");
        assert!(patched.contains(r#"name="email""#), "o campo continua sendo o mesmo: {patched}");
    }

    #[test]
    fn several_instructions_apply_in_order() {
        let patched = apply_inheritance(
            FORM,
            r#"<data>
                 <field name="name" position="after"><field name="a"/></field>
                 <field name="email" position="after"><field name="b"/></field>
               </data>"#,
        )
        .unwrap();
        let a = patched.find(r#"name="a""#).unwrap();
        let b = patched.find(r#"name="b""#).unwrap();
        assert!(a < b, "{patched}");
    }

    #[test]
    fn a_patch_that_matches_nothing_is_an_error() {
        let error = apply_inheritance(
            FORM,
            r#"<data><field name="inexistente" position="after"><field name="x"/></field></data>"#,
        )
        .expect_err("um patch que não encontra o alvo não é um patch que não faz nada");
        assert!(error.to_string().contains("inexistente"), "{error}");
    }

    #[test]
    fn an_unknown_position_or_expr_is_refused() {
        assert!(apply_inheritance(
            FORM,
            r#"<data><field name="name" position="depois"><field name="x"/></field></data>"#
        )
        .is_err());
        assert!(apply_inheritance(
            FORM,
            r#"<data><xpath expr="//div[3]/span" position="after"><field name="x"/></xpath></data>"#
        )
        .is_err());
    }
}

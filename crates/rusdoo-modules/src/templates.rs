//! The XML half of a bundle, compiled into a JS module.
//!
//! Port of `generate_xml_bundle` and `xml()` in
//! `odoo/addons/base/models/assetsbundle.py`. An addon ships its OWL
//! templates as `.xml` files listed in the same bundle as its
//! JavaScript, and the client never fetches them: Odoo appends one more
//! module to the JS bundle, `<bundle>.bundle.xml`, whose body registers
//! every template by name. A bundle served without it is a client that
//! starts and then stops at `Missing template: "web.WebClient"`.
//!
//! Deviation, deliberate: the markup of each template is the source
//! text as the addon wrote it, where Odoo re-serializes a parsed tree.
//! The two differ in whitespace after the closing tag (Odoo carries the
//! element's tail) and in nothing the client can observe — and keeping
//! the source means a template is served as its author wrote it.

use quick_xml::events::Event;
use quick_xml::Reader;

/// One `.xml` file of a bundle: the URL the addon publishes it at, and
/// what it says.
pub struct TemplateFile<'a> {
    /// as the client names it — `/web/static/src/webclient/webclient.xml`
    pub url: &'a str,
    pub source: &'a str,
}

/// The module that registers the templates of `bundle`, or an empty
/// string when the bundle has none.
///
/// A file that cannot be read is not a refusal to serve the bundle: like
/// Odoo, the module becomes a `throw` and the page loads far enough to
/// say what is wrong.
pub fn template_module(bundle: &str, files: &[TemplateFile<'_>]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let body = match collect(files) {
        Ok(statements) if statements.is_empty() => return String::new(),
        Ok(statements) => statements.join("\n        "),
        Err(message) => format!("throw new Error({});", js_string(&message)),
    };
    format!(
        "\n\n/*******************************************\n\
         *  Templates                               *\n\
         *******************************************/\n\n\
         odoo.define(\"{bundle}.bundle.xml\", [\"@web/core/templates\"], function(require) {{\n    \
         \"use strict\";\n    \
         const {{ checkPrimaryTemplateParents, registerTemplate, registerTemplateExtension }} = require(\"@web/core/templates\");\n    \
         /* {bundle} */\n        {body}\n\
         }});\n"
    )
}

/// One template found in a file.
struct Found {
    name: Option<String>,
    inherit: Option<String>,
    extension: bool,
    markup: String,
}

/// The registration calls, in the order the client must run them.
fn collect(files: &[TemplateFile<'_>]) -> Result<Vec<String>, String> {
    let mut statements: Vec<String> = Vec::new();
    // which names this bundle defines, and which ones it expects somebody
    // to have defined — a mismatch is worth reporting to the console
    let mut defined: Vec<String> = Vec::new();
    let mut primary_parents: Vec<String> = Vec::new();
    let mut extension_parents: Vec<String> = Vec::new();

    for file in files {
        for found in templates_of(file.source).map_err(|error| format!("{}: {error}", file.url))? {
            let markup = js_template_literal(&found.markup);
            let url = js_template_literal(file.url);
            if found.extension {
                let Some(parent) = found.inherit else {
                    return Err(format!(
                        "{}: a template extension without t-inherit",
                        file.url
                    ));
                };
                remember(&mut extension_parents, &parent);
                statements.push(format!(
                    "registerTemplateExtension(\"{parent}\", `{url}`, `{markup}`);"
                ));
                continue;
            }
            let Some(name) = found.name else {
                // Odoo: "Template name is missing."
                return Err(format!("{}: Template name is missing.", file.url));
            };
            if let Some(parent) = &found.inherit {
                remember(&mut primary_parents, parent);
            }
            remember(&mut defined, &name);
            statements.push(format!(
                "registerTemplate(\"{name}\", `{url}`, `{markup}`);"
            ));
        }
    }

    // A primary template inheriting one this bundle does not carry is a
    // question for the client, which knows what other bundles defined.
    let missing_primary: Vec<&String> = primary_parents
        .iter()
        .filter(|parent| !defined.contains(parent))
        .collect();
    if !missing_primary.is_empty() {
        let names: Vec<String> = missing_primary.iter().map(|n| js_string(n)).collect();
        statements.push(format!(
            "checkPrimaryTemplateParents([{}]);",
            names.join(", ")
        ));
    }
    let missing_extension: Vec<&String> = extension_parents
        .iter()
        .filter(|parent| !defined.contains(parent))
        .collect();
    if !missing_extension.is_empty() {
        let names: Vec<&str> = missing_extension.iter().map(|n| n.as_str()).collect();
        statements.push(format!(
            "console.error(\"Missing (extension) parent templates: {}\");",
            names.join(", ")
        ));
    }
    Ok(statements)
}

fn remember(seen: &mut Vec<String>, name: &str) {
    if !seen.iter().any(|known| known == name) {
        seen.push(name.to_string());
    }
}

/// The templates of one file, in order.
///
/// A file's root is `<templates>` (or `<template>`) holding one element
/// per template, exactly as `XMLAsset._fetch_content` unwraps it; a file
/// whose root is the template itself is that one template.
fn templates_of(source: &str) -> Result<Vec<Found>, String> {
    let mut reader = Reader::from_str(source);
    let mut found: Vec<Found> = Vec::new();
    let mut depth = 0usize;
    // the element being captured: where its text starts, and what its
    // start tag said
    let mut capturing: Option<(usize, Found)> = None;
    // byte ranges of the comments inside it, which do not travel
    let mut comments: Vec<(usize, usize)> = Vec::new();
    let mut root_is_wrapper: Option<bool> = None;

    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event().map_err(|e| e.to_string())?;
        let after = reader.buffer_position() as usize;
        match event {
            Event::Eof => break,
            Event::Start(element) => {
                let name = String::from_utf8_lossy(element.name().as_ref()).to_string();
                if root_is_wrapper.is_none() {
                    root_is_wrapper = Some(is_wrapper(&name));
                    if root_is_wrapper == Some(true) {
                        depth += 1;
                        continue;
                    }
                }
                if capturing.is_none() && depth == wanted_depth(root_is_wrapper) {
                    capturing = Some((before, attributes_of(&element)?));
                    comments.clear();
                }
                depth += 1;
            }
            // `<t t-name="x"/>` — a template with no body, whole in one event
            Event::Empty(element) => {
                let name = String::from_utf8_lossy(element.name().as_ref()).to_string();
                if root_is_wrapper.is_none() {
                    root_is_wrapper = Some(is_wrapper(&name));
                    if root_is_wrapper == Some(true) {
                        continue;
                    }
                }
                if capturing.is_none() && depth == wanted_depth(root_is_wrapper) {
                    let template = attributes_of(&element)?;
                    found.push(finish(source, before, after, &comments, template));
                    comments.clear();
                }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if let Some((start, template)) = capturing.take() {
                    if depth == wanted_depth(root_is_wrapper) {
                        found.push(finish(source, start, after, &comments, template));
                    } else {
                        capturing = Some((start, template));
                    }
                }
            }
            Event::Comment(_) => comments.push((before, after)),
            _ => {}
        }
    }
    Ok(found)
}

/// Whether the root element is the wrapper Odoo unwraps rather than a
/// template in its own right.
fn is_wrapper(name: &str) -> bool {
    name == "templates" || name == "template"
}

/// The depth at which a template element sits: one inside a
/// `<templates>` wrapper, zero when the file is a single template.
fn wanted_depth(root_is_wrapper: Option<bool>) -> usize {
    if root_is_wrapper == Some(true) {
        1
    } else {
        0
    }
}

/// Read `t-name`, `t-inherit` and `t-inherit-mode` off a start tag.
fn attributes_of(element: &quick_xml::events::BytesStart) -> Result<Found, String> {
    let mut found = Found {
        name: None,
        inherit: None,
        extension: false,
        markup: String::new(),
    };
    let mut mode: Option<String> = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|e| e.to_string())?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).to_string();
        let raw = String::from_utf8_lossy(&attribute.value);
        let value = quick_xml::escape::unescape(&raw)
            .map_err(|e| e.to_string())?
            .into_owned();
        match key.as_str() {
            "t-name" => found.name = Some(value),
            "t-inherit" => found.inherit = Some(value),
            "t-inherit-mode" => mode = Some(value),
            _ => {}
        }
    }
    if found.inherit.is_some() {
        // Odoo defaults the mode to primary and refuses anything else
        match mode.as_deref().unwrap_or("primary") {
            "primary" => {}
            "extension" => found.extension = true,
            other => return Err(format!("Invalid inherit mode {other}")),
        }
    }
    Ok(found)
}

/// The markup of a captured element: its own source, with the comments
/// cut out and `xml:space="preserve"` on its start tag — the attribute
/// Odoo sets on every template so OWL keeps the whitespace the author
/// wrote.
fn finish(
    source: &str,
    start: usize,
    end: usize,
    comments: &[(usize, usize)],
    mut template: Found,
) -> Found {
    let mut markup = String::with_capacity(end - start);
    let mut cursor = start;
    for (from, to) in comments.iter().filter(|(from, _)| *from >= start) {
        markup.push_str(source.get(cursor..*from).unwrap_or(""));
        cursor = *to;
    }
    markup.push_str(source.get(cursor..end).unwrap_or(""));
    template.markup = with_preserved_space(&markup);
    template
}

/// Add `xml:space="preserve"` to the first start tag, unless it says so
/// already (a second one would not be XML).
fn with_preserved_space(markup: &str) -> String {
    let Some(tag_end) = end_of_start_tag(markup) else {
        return markup.to_string();
    };
    let tag = &markup[..tag_end];
    if tag.contains("xml:space") {
        return markup.to_string();
    }
    let self_closing = tag.trim_end().ends_with('/');
    let insert_at = if self_closing {
        tag.rfind('/').unwrap_or(tag_end)
    } else {
        tag_end
    };
    let mut out = String::with_capacity(markup.len() + 22);
    out.push_str(markup[..insert_at].trim_end());
    out.push_str(" xml:space=\"preserve\"");
    if self_closing {
        out.push_str("/>");
    } else {
        out.push('>');
    }
    out.push_str(&markup[tag_end + 1..]);
    out
}

/// The index of the `>` that closes the first tag, ignoring the ones
/// inside attribute values.
fn end_of_start_tag(markup: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, character) in markup.char_indices() {
        match (quote, character) {
            (None, '"') | (None, '\'') => quote = Some(character),
            (Some(open), c) if c == open => quote = None,
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

/// Text going inside a JS template literal: a backtick or a `${` would
/// end it and turn the markup into code.
fn js_template_literal(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${")
}

/// Text going inside a double-quoted JS string.
fn js_string(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

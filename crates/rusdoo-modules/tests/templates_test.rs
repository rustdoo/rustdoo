//! The XML half of a bundle, compiled into the JS module the client
//! registers its templates from. Reference:
//! `odoo/addons/base/models/assetsbundle.py::generate_xml_bundle`.

use rusdoo_modules::templates::{template_module, TemplateFile};
use std::path::Path;

fn one(url: &str, source: &str) -> String {
    template_module("web.assets_web", &[TemplateFile { url, source }])
}

#[test]
fn a_template_becomes_a_registration() {
    let js = one(
        "/demo/static/src/a.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<templates xml:space="preserve">
    <t t-name="demo.Thing"><div>hi</div></t>
</templates>"#,
    );

    assert!(
        js.contains(r#"odoo.define("web.assets_web.bundle.xml", ["@web/core/templates"]"#),
        "{js}"
    );
    assert!(
        js.contains(
            r#"registerTemplate("demo.Thing", `/demo/static/src/a.xml`, `<t t-name="demo.Thing" xml:space="preserve"><div>hi</div></t>`);"#
        ),
        "{js}"
    );
}

#[test]
fn an_extension_extends_instead_of_registering() {
    let js = one(
        "/demo/static/src/b.xml",
        r#"<templates>
    <t t-name="demo.Patch" t-inherit="web.WebClient" t-inherit-mode="extension">
        <xpath expr="//div" position="inside"><span/></xpath>
    </t>
</templates>"#,
    );

    assert!(js.contains(r#"registerTemplateExtension("web.WebClient", `/demo/static/src/b.xml`"#), "{js}");
    assert!(!js.contains("registerTemplate("), "{js}");
    // an extension of a template nobody in this bundle defines is worth
    // saying out loud, exactly as Odoo says it
    assert!(js.contains("Missing (extension) parent templates"), "{js}");
}

#[test]
fn a_primary_inherit_declares_the_parent_it_expects() {
    let js = one(
        "/demo/static/src/c.xml",
        r#"<templates>
    <t t-name="demo.Own" t-inherit="web.Other" t-inherit-mode="primary">
        <xpath expr="//div" position="replace"><span/></xpath>
    </t>
</templates>"#,
    );

    assert!(js.contains(r#"registerTemplate("demo.Own", "#), "{js}");
    assert!(js.contains(r#"checkPrimaryTemplateParents(["web.Other"]);"#), "{js}");
}

#[test]
fn a_backtick_cannot_close_the_template_literal() {
    // the template travels inside a JS template literal: a backtick or a
    // `${` in the markup would end it and turn the rest into code
    let js = one(
        "/demo/static/src/d.xml",
        r#"<templates><t t-name="demo.Sneaky"><div title="a ` and ${1+1} and \ here"/></t></templates>"#,
    );

    // every one of the three is escaped where it sits
    assert!(js.contains(r"a \` and"), "{js}");
    assert!(js.contains(r"\${1+1}"), "{js}");
    assert!(js.contains(r"\\ here"), "{js}");
}

#[test]
fn a_template_with_no_name_is_a_thrown_error() {
    // Odoo turns it into a `throw` inside the module rather than refusing
    // to serve the bundle: the page still loads and says what is wrong
    let js = one(
        "/demo/static/src/e.xml",
        r#"<templates><t><div/></t></templates>"#,
    );

    assert!(js.contains("throw new Error("), "{js}");
    assert!(js.contains("Template name is missing"), "{js}");
}

#[test]
fn a_file_that_is_not_xml_is_a_thrown_error_too() {
    let js = one("/demo/static/src/f.xml", "<templates><t t-name=\"x\"></templates>");

    assert!(js.contains("throw new Error("), "{js}");
}

#[test]
fn comments_do_not_travel_to_the_client() {
    let js = one(
        "/demo/static/src/g.xml",
        r#"<templates><t t-name="demo.Commented"><!-- a note --><div/></t></templates>"#,
    );

    assert!(!js.contains("a note"), "{js}");
    assert!(js.contains("<div/>"), "{js}");
}

#[test]
fn a_bundle_with_no_templates_adds_nothing() {
    assert_eq!(template_module("web.assets_web", &[]), "");
}

/// The template the client mounts on, out of the real tree: without it
/// OWL stops at `Missing template: "web.WebClient"` and no page renders.
#[test]
fn the_real_web_client_template_compiles() {
    let file = Path::new("../../odoo/addons/web/static/src/webclient/webclient.xml");
    if !file.exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }
    let source = std::fs::read_to_string(file).unwrap();
    let js = template_module(
        "web.assets_web",
        &[TemplateFile {
            url: "/web/static/src/webclient/webclient.xml",
            source: &source,
        }],
    );

    assert!(!js.contains("throw new Error("), "{js}");
    assert!(
        js.contains(r#"registerTemplate("web.WebClient", `/web/static/src/webclient/webclient.xml`"#),
        "{js}"
    );
    assert!(js.contains(r#"<t t-name="web.WebClient" xml:space="preserve">"#), "{js}");
}

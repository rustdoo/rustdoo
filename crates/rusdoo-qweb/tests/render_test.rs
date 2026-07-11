//! QWeb rendering: the core directives (t-esc/t-out, t-if/t-else,
//! t-foreach/t-as, t-att-*) against a JSON context.

use rusdoo_qweb::{render, render_with};
use serde_json::json;
use std::collections::HashMap;

fn r(tpl: &str, ctx: serde_json::Value) -> String {
    render(tpl, &ctx).unwrap()
}

#[test]
fn t_esc_outputs_escaped_value() {
    let out = r(r#"<p t-esc="title"/>"#, json!({"title": "A & B <ok>"}));
    assert_eq!(out, "<p>A &amp; B &lt;ok&gt;</p>");
}

#[test]
fn t_out_escapes_plain_values() {
    // Odoo's t-out escapes a plain (non-Markup) value — emitting it raw
    // was an XSS. Only Markup passes through unescaped (see t-set below).
    let out = r(r#"<div t-out="body"/>"#, json!({"body": "<b>hi</b>"}));
    assert_eq!(out, "<div>&lt;b&gt;hi&lt;/b&gt;</div>");
}

#[test]
fn t_set_body_capture_is_markup_not_reescaped() {
    // a t-set body captures rendered HTML as Markup; a later t-out/t-esc
    // must emit it raw, not double-escape it
    let out = r(
        r#"<div><t t-set="g"><b>bold</b></t><t t-out="g"/></div>"#,
        json!({}),
    );
    assert_eq!(out, "<div><b>bold</b></div>");
    // and the same capture reused through t-esc
    let out = r(
        r#"<div><t t-set="g"><i>x</i></t><t t-esc="g"/></div>"#,
        json!({}),
    );
    assert_eq!(out, "<div><i>x</i></div>");
}

#[test]
fn dotted_paths_read_into_the_context() {
    let out = r(
        r#"<span t-esc="book.name"/>"#,
        json!({"book": {"name": "Dom Casmurro"}}),
    );
    assert_eq!(out, "<span>Dom Casmurro</span>");
}

#[test]
fn t_if_skips_falsy_blocks() {
    let tpl = r#"<div><p t-if="show">yes</p></div>"#;
    assert_eq!(r(tpl, json!({"show": true})), "<div><p>yes</p></div>");
    assert_eq!(r(tpl, json!({"show": false})), "<div></div>");
}

#[test]
fn t_else_renders_the_alternative() {
    let tpl = r#"<div><t t-if="ok">A</t><t t-else="">B</t></div>"#;
    assert_eq!(r(tpl, json!({"ok": true})), "<div>A</div>");
    assert_eq!(r(tpl, json!({"ok": false})), "<div>B</div>");
}

#[test]
fn t_element_is_transparent() {
    // <t> emits no tag, only its content
    let out = r(r#"<t t-esc="x"/>"#, json!({"x": "hi"}));
    assert_eq!(out, "hi");
}

#[test]
fn t_foreach_repeats_with_the_loop_var() {
    let tpl = r#"<ul><li t-foreach="books" t-as="b" t-esc="b.name"/></ul>"#;
    let out = r(tpl, json!({"books": [{"name": "Um"}, {"name": "Dois"}]}));
    assert_eq!(out, "<ul><li>Um</li><li>Dois</li></ul>");
}

#[test]
fn t_foreach_exposes_index() {
    let tpl = r#"<t t-foreach="xs" t-as="x"><i t-esc="x_index"/></t>"#;
    let out = r(tpl, json!({"xs": [10, 20, 30]}));
    assert_eq!(out, "<i>0</i><i>1</i><i>2</i>");
}

#[test]
fn t_att_sets_a_dynamic_attribute() {
    let out = r(r#"<a t-att-href="url">link</a>"#, json!({"url": "/x"}));
    assert_eq!(out, r#"<a href="/x">link</a>"#);
}

#[test]
fn static_attributes_and_children_pass_through() {
    let out = r(
        r#"<div class="card"><span t-esc="n"/></div>"#,
        json!({"n": 7}),
    );
    assert_eq!(out, r#"<div class="card"><span>7</span></div>"#);
}

#[test]
fn conditions_use_comparisons_and_booleans() {
    let tpl = r#"<p t-if="n > 3 and ok">big</p>"#;
    assert_eq!(r(tpl, json!({"n": 5, "ok": true})), "<p>big</p>");
    assert_eq!(r(tpl, json!({"n": 2, "ok": true})), "");
    assert_eq!(r(tpl, json!({"n": 5, "ok": false})), "");
}

#[test]
fn realistic_report_fragment() {
    let tpl = r#"<div><h1 t-esc="title"/><ul t-if="books"><li t-foreach="books" t-as="b"><t t-esc="b.name"/> - <t t-esc="b.pages"/>p</li></ul></div>"#;
    let ctx = json!({
        "title": "Catálogo",
        "books": [{"name": "Dom Casmurro", "pages": 256}, {"name": "O Alienista", "pages": 96}]
    });
    let out = r(tpl, ctx);
    assert_eq!(
        out,
        "<div><h1>Catálogo</h1><ul><li>Dom Casmurro - 256p</li><li>O Alienista - 96p</li></ul></div>"
    );
}

#[test]
fn t_field_renders_field_values() {
    // plain value: escaped like t-esc
    assert_eq!(
        r(
            r#"<span t-field="p.name"/>"#,
            json!({"p": {"name": "A & B"}})
        ),
        "<span>A &amp; B</span>"
    );
    // a many2one read [id, display_name] renders the name, not the id
    assert_eq!(
        r(
            r#"<span t-field="p.company_id"/>"#,
            json!({"p": {"company_id": [1, "Rusdoo S.A."]}})
        ),
        "<span>Rusdoo S.A.</span>"
    );
    // number renders its value
    assert_eq!(
        r(r#"<b t-field="p.pages"/>"#, json!({"p": {"pages": 256}})),
        "<b>256</b>"
    );
    // unset/null renders empty
    assert_eq!(
        r(r#"<i t-field="p.missing"/>"#, json!({"p": {}})),
        "<i></i>"
    );
}

fn tpls(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn t_call_includes_a_named_template() {
    let t = tpls(&[("card", r#"<div class="card"><t t-esc="name"/></div>"#)]);
    let out = render_with(
        r#"<section><t t-call="card"/></section>"#,
        &json!({"name": "Ana"}),
        &t,
    )
    .unwrap();
    assert_eq!(out, r#"<section><div class="card">Ana</div></section>"#);
}

#[test]
fn t_call_sees_the_loop_context() {
    let t = tpls(&[("row", r#"<li t-field="i.name"/>"#)]);
    let out = render_with(
        r#"<ul><t t-foreach="items" t-as="i"><t t-call="row"/></t></ul>"#,
        &json!({"items": [{"name": "Um"}, {"name": "Dois"}]}),
        &t,
    )
    .unwrap();
    assert_eq!(out, "<ul><li>Um</li><li>Dois</li></ul>");
}

#[test]
fn t_call_to_unknown_template_errors() {
    let t = tpls(&[]);
    assert!(render_with(r#"<t t-call="ghost"/>"#, &json!({}), &t).is_err());
}

#[test]
fn nested_t_call_layout_pattern() {
    // a base layout t-calling nothing, a page t-calling the base is common
    let t = tpls(&[
        (
            "layout",
            r#"<html><body><t t-call="content"/></body></html>"#,
        ),
        ("content", r#"<h1 t-esc="title"/>"#),
    ]);
    let out = render_with(r#"<t t-call="layout"/>"#, &json!({"title": "Oi"}), &t).unwrap();
    assert_eq!(out, "<html><body><h1>Oi</h1></body></html>");
}

#[test]
fn t_set_defines_a_variable() {
    let out = r(
        r#"<div><t t-set="x" t-value="'A'"/><p t-esc="x"/></div>"#,
        json!({}),
    );
    assert_eq!(out, "<div><p>A</p></div>");
}

#[test]
fn t_set_body_captures_rendered_content() {
    let out = r(
        r#"<div><t t-set="g">Ola <t t-esc="name"/></t><p t-esc="g"/></div>"#,
        json!({"name": "Ana"}),
    );
    assert_eq!(out, "<div><p>Ola Ana</p></div>");
}

#[test]
fn t_set_is_visible_to_later_siblings_only() {
    let out = r(
        r#"<div><span t-esc="x"/><t t-set="x" t-value="1"/><b t-esc="x"/></div>"#,
        json!({}),
    );
    assert_eq!(out, "<div><span></span><b>1</b></div>");
}

#[test]
fn t_set_can_compute_from_context() {
    let out = r(
        r#"<t t-set="total" t-value="a + b"/><p t-esc="total"/>"#,
        json!({"a": 40, "b": 2}),
    );
    assert_eq!(out, "<p>42</p>");
}

#[test]
fn t_elif_chains_conditions() {
    let tpl = r#"<div><t t-if="a">A</t><t t-elif="b">B</t><t t-else="">C</t></div>"#;
    assert_eq!(r(tpl, json!({"a": true, "b": true})), "<div>A</div>");
    assert_eq!(r(tpl, json!({"a": false, "b": true})), "<div>B</div>");
    assert_eq!(r(tpl, json!({"a": false, "b": false})), "<div>C</div>");
}

#[test]
fn static_attribute_keeps_named_html_entities() {
    // real Odoo views embed &hellip; &nbsp; &mdash; in title/placeholder;
    // our 5-entity resolver must not hard-fail on them
    let out = r(r#"<span title="more&hellip;">x</span>"#, json!({}));
    assert_eq!(out, r#"<span title="more&hellip;">x</span>"#);
}

#[test]
fn static_attribute_preserves_ampersand_entity() {
    // &amp; must round-trip, not become &amp;amp; nor a bare &
    let out = r(r#"<a href="/a?x=1&amp;y=2">go</a>"#, json!({}));
    assert_eq!(out, r#"<a href="/a?x=1&amp;y=2">go</a>"#);
}

#[test]
fn orphan_t_elif_is_a_hard_error() {
    // a t-elif with no preceding t-if is a template bug — fail loudly,
    // don't silently drop it (Odoo raises SyntaxError)
    let err = render(r#"<div><p>x</p><t t-elif="1">y</t></div>"#, &json!({})).unwrap_err();
    assert!(format!("{err}").contains("t-elif"), "got: {err}");
}

#[test]
fn orphan_t_else_is_a_hard_error() {
    let err = render(r#"<div><t t-else="">y</t></div>"#, &json!({})).unwrap_err();
    assert!(format!("{err}").contains("t-else"), "got: {err}");
}

#[test]
fn t_attf_interpolates_the_attribute() {
    assert_eq!(
        r(
            r#"<a t-attf-class="btn btn-{{state}}">x</a>"#,
            json!({"state": "primary"})
        ),
        r#"<a class="btn btn-primary">x</a>"#
    );
    assert_eq!(
        r(
            r#"<div t-attf-href="/user/{{id}}/edit"/>"#,
            json!({"id": 5})
        ),
        r#"<div href="/user/5/edit"></div>"#
    );
}

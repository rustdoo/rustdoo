//! QWeb rendering: the core directives (t-esc/t-out, t-if/t-else,
//! t-foreach/t-as, t-att-*) against a JSON context.

use rusdoo_qweb::render;
use serde_json::json;

fn r(tpl: &str, ctx: serde_json::Value) -> String {
    render(tpl, &ctx).unwrap()
}

#[test]
fn t_esc_outputs_escaped_value() {
    let out = r(r#"<p t-esc="title"/>"#, json!({"title": "A & B <ok>"}));
    assert_eq!(out, "<p>A &amp; B &lt;ok&gt;</p>");
}

#[test]
fn t_out_outputs_raw() {
    let out = r(r#"<div t-out="body"/>"#, json!({"body": "<b>hi</b>"}));
    assert_eq!(out, "<div><b>hi</b></div>");
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

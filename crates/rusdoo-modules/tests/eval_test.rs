//! The `eval="..."` evaluator: ref() and Command.*, as used across the
//! real Odoo base data (e.g. `[Command.link(ref('group_user'))]`).

use rusdoo_modules::eval::eval_expr;
use serde_json::json;
use std::collections::HashMap;

fn refs(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn eval(src: &str, map: &HashMap<String, i64>) -> serde_json::Value {
    eval_expr(src, &|name: &str| map.get(name).copied()).unwrap()
}

#[test]
fn literals_pass_through() {
    let m = refs(&[]);
    assert_eq!(eval("7", &m), json!(7));
    assert_eq!(eval("[1, 2, 3]", &m), json!([1, 2, 3]));
    assert_eq!(eval("True", &m), json!(true));
    assert_eq!(eval("'hi'", &m), json!("hi"));
}

#[test]
fn ref_resolves_external_id() {
    let m = refs(&[("group_user", 42)]);
    assert_eq!(eval("ref('group_user')", &m), json!(42));
}

#[test]
fn unknown_ref_is_an_error() {
    let m = refs(&[]);
    assert!(eval_expr("ref('nope')", &|n: &str| m.get(n).copied()).is_err());
}

#[test]
fn command_link_becomes_odoo_tuple() {
    // Command.link(id) -> (4, id, 0)
    let m = refs(&[("group_user", 5)]);
    assert_eq!(
        eval("[Command.link(ref('group_user'))]", &m),
        json!([[4, 5, 0]])
    );
}

#[test]
fn command_family_maps_to_codes() {
    let m = refs(&[("g", 9)]);
    assert_eq!(eval("Command.link(ref('g'))", &m), json!([4, 9, 0]));
    assert_eq!(eval("Command.unlink(ref('g'))", &m), json!([3, 9, 0]));
    assert_eq!(eval("Command.delete(ref('g'))", &m), json!([2, 9, 0]));
    assert_eq!(eval("Command.clear()", &m), json!([5, 0, 0]));
    assert_eq!(eval("Command.set([1, 2])", &m), json!([6, 0, [1, 2]]));
    assert_eq!(
        eval("Command.create({'name': 'x'})", &m),
        json!([0, 0, {"name": "x"}])
    );
}

#[test]
fn multiple_commands_in_a_list() {
    // the real shape in base security data
    let m = refs(&[("base.user_root", 1), ("base.user_admin", 2)]);
    assert_eq!(
        eval(
            "[Command.link(ref('base.user_root')), Command.link(ref('base.user_admin'))]",
            &m
        ),
        json!([[4, 1, 0], [4, 2, 0]])
    );
}

#[test]
fn real_base_groups_expr_evaluates() {
    // straight from odoo/addons/base/security/base_groups.xml
    let m = refs(&[("group_erp_manager", 7), ("group_sanitize_override", 8)]);
    let out = eval(
        "[Command.link(ref('group_erp_manager')), Command.link(ref('group_sanitize_override'))]",
        &m,
    );
    assert_eq!(out, json!([[4, 7, 0], [4, 8, 0]]));
}

#[test]
fn wrong_arity_is_an_error() {
    let m = refs(&[]);
    let bad = |src: &str| eval_expr(src, &|n: &str| m.get(n).copied()).is_err();
    // Command.link() forgot the id -> must error, not silently [4,0,0]
    assert!(bad("Command.link()"));
    assert!(bad("Command.update(5)"));
    assert!(bad("Command.clear(1)"));
    assert!(bad("Command.link(1, 2)"));
}

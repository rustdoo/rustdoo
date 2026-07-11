//! Access control primitive, port of ir.model.access.check semantics:
//! superuser bypasses everything; a model with no ACL rules is
//! unrestricted; once a model has any rule, an operation needs a group
//! that grants it.

use rusdoo_orm::access::{AccessControl, Operation};

fn ac() -> AccessControl {
    let mut ac = AccessControl::new();
    // group 1 (users): read only on res.partner
    ac.grant("res.partner", 1, &[Operation::Read]);
    // group 2 (managers): full on res.partner
    ac.grant(
        "res.partner",
        2,
        &[
            Operation::Read,
            Operation::Write,
            Operation::Create,
            Operation::Unlink,
        ],
    );
    ac
}

#[test]
fn superuser_bypasses_all_checks() {
    let ac = ac();
    // uid 1 / superuser: allowed even with no groups
    assert!(ac
        .check("res.partner", Operation::Unlink, &[], true)
        .is_ok());
    assert!(ac.check("anything", Operation::Write, &[], true).is_ok());
}

#[test]
fn model_without_rules_denies_regular_users() {
    let ac = ac();
    // fail-closed: a model with no rule is superuser-only (Odoo default)
    assert!(ac
        .check("res.company", Operation::Write, &[], false)
        .is_err());
    // ...but the superuser still passes
    assert!(ac.check("res.company", Operation::Write, &[], true).is_ok());
}

#[test]
fn group_grants_only_its_operations() {
    let ac = ac();
    // group 1 can read but not write
    assert!(ac
        .check("res.partner", Operation::Read, &[1], false)
        .is_ok());
    assert!(ac
        .check("res.partner", Operation::Write, &[1], false)
        .is_err());
}

#[test]
fn manager_group_has_full_access() {
    let ac = ac();
    for op in [
        Operation::Read,
        Operation::Write,
        Operation::Create,
        Operation::Unlink,
    ] {
        assert!(ac.check("res.partner", op, &[2], false).is_ok());
    }
}

#[test]
fn union_of_groups_combines_permissions() {
    let ac = ac();
    // a user in both groups gets the union
    assert!(ac
        .check("res.partner", Operation::Write, &[1, 2], false)
        .is_ok());
}

#[test]
fn no_matching_group_is_access_error() {
    let ac = ac();
    // group 9 has no rule on res.partner, and the model IS restricted
    let err = ac
        .check("res.partner", Operation::Read, &[9], false)
        .unwrap_err();
    assert!(matches!(err, rusdoo_core::RusdooError::Access(_)));
    assert!(err.to_string().contains("res.partner"));
}

#[test]
fn empty_groups_on_restricted_model_is_denied() {
    let ac = ac();
    assert!(ac
        .check("res.partner", Operation::Read, &[], false)
        .is_err());
}

#[test]
fn operation_maps_from_orm_method_names() {
    assert_eq!(Operation::for_method("search"), Some(Operation::Read));
    assert_eq!(Operation::for_method("search_read"), Some(Operation::Read));
    assert_eq!(Operation::for_method("read"), Some(Operation::Read));
    assert_eq!(Operation::for_method("create"), Some(Operation::Create));
    assert_eq!(Operation::for_method("write"), Some(Operation::Write));
    assert_eq!(Operation::for_method("unlink"), Some(Operation::Unlink));
    assert_eq!(Operation::for_method("search_count"), Some(Operation::Read));
    // an unknown/custom method has no implied CRUD operation
    assert_eq!(Operation::for_method("action_confirm"), None);
}

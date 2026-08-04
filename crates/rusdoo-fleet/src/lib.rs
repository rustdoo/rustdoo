//! rusdoo-fleet — port of `odoo/addons/fleet/`: the vehicles a company
//! owns and everything that happens to them.
//!
//! The addon answers four questions, and the models follow them: what is
//! this vehicle (brand, model, category), who drives it (the assignment
//! history), how far has it gone (odometer readings), and what does it
//! cost (services, and the contracts that have to be renewed before they
//! run out).
//!
//! Two things shape the port. The first is that a contract's whole
//! reason to exist is its expiry: the renewal reminder, the countdown
//! and the nightly job that moves a contract from *queued* to *running*
//! to *expired* are the behaviour a fleet manager actually uses, and
//! they are ported in full. The second is that Odoo puts most of the
//! rest inside `create` and `write` overrides — handing a vehicle to a
//! driver, filling a vehicle in from its model, refusing an odometer
//! reading that goes backwards. This ORM has no create/write hook, so
//! each of those is a method a client calls by name. The rule the user
//! meets is the same one; the place it lives is not.
//!
//! What is deliberately *not* here is listed in
//! [`extend`]'s module documentation and in the port's report: the cost
//! analysis report (a SQL view), the mass-mail wizard (it needs the mail
//! composer), and `service_activity` (it needs `mail.activity`).

mod compute;
mod methods;
mod models;

pub use compute::DELAY_ALERT_CONTRACT_DAYS;
pub use methods::extend_methods;
pub use models::FUEL_TYPES;

use rusdoo_core::RusdooError;
use rusdoo_orm::registry::Registry;

/// The models a chatter is attached to, for a server wiring `mail` up.
///
/// Odoo puts `mail.thread` on these three: a vehicle, its services and
/// its contracts are all things people argue about in writing.
pub const THREAD_MODELS: [&str; 3] = [
    "fleet.vehicle",
    "fleet.vehicle.log.services",
    "fleet.vehicle.log.contract",
];

/// Register the fleet's models, in dependency order.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    for model in models::models() {
        reg.register(model)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusdoo_orm::fields::FieldType;
    use rusdoo_orm::methods::MethodRegistry;

    fn registry() -> Registry {
        let mut reg = rusdoo_base::registry().expect("base registers");
        extend(&mut reg).expect("fleet registers on top of base");
        reg
    }

    #[test]
    fn every_model_of_the_addon_registers() {
        let reg = registry();
        for name in [
            "fleet.vehicle",
            "fleet.vehicle.model",
            "fleet.vehicle.model.brand",
            "fleet.vehicle.model.category",
            "fleet.vehicle.state",
            "fleet.vehicle.tag",
            "fleet.service.type",
            "fleet.vehicle.odometer",
            "fleet.vehicle.log.services",
            "fleet.vehicle.log.contract",
            "fleet.vehicle.assignation.log",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
    }

    #[test]
    fn a_vehicle_cannot_exist_without_a_model() {
        let reg = registry();
        let vehicle = reg.get("fleet.vehicle").unwrap();
        assert!(vehicle.field("model_id").unwrap().required);
        // and the model cannot be deleted out from under it
        assert_eq!(
            vehicle.field("model_id").unwrap().ondelete,
            Some(rusdoo_orm::fields::OnDelete::Restrict)
        );
    }

    #[test]
    fn the_vehicle_name_is_a_real_column() {
        // it is what every list sorts and searches by: a compute resolved
        // per read would make `_order` impossible
        let reg = registry();
        let name = reg.get("fleet.vehicle").unwrap().field("name").unwrap();
        assert!(name.stored, "the name is materialized");
        assert!(name.compute.is_some(), "and it is derived, not typed");
    }

    #[test]
    fn the_renewal_flags_are_not_materialized() {
        // they depend on today's date as much as on the contracts: a
        // column would be right the night it was written and wrong the
        // next morning
        let reg = registry();
        let vehicle = reg.get("fleet.vehicle").unwrap();
        for name in [
            "contract_renewal_due_soon",
            "contract_renewal_overdue",
            "contract_state",
            "odometer",
        ] {
            let field = vehicle.field(name).unwrap();
            assert!(field.compute.is_some(), "{name} is derived");
            assert!(!field.stored, "{name} must not be a column");
        }
    }

    #[test]
    fn a_state_column_and_a_tag_may_not_be_duplicated() {
        let reg = registry();
        for (model, constraint) in [
            ("fleet.vehicle.state", "fleet_state_name_unique"),
            ("fleet.vehicle.tag", "fleet_vehicle_tag_name_uniq"),
            (
                "fleet.vehicle.model.category",
                "fleet_vehicle_model_category_name_uniq",
            ),
        ] {
            let names: Vec<&str> = reg
                .get(model)
                .unwrap()
                .sql_constraints()
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            assert_eq!(names, vec![constraint], "{model}");
        }
    }

    #[test]
    fn the_tag_relation_keeps_odoos_table_and_columns() {
        // a port that renamed them would write a different table for the
        // same relation, and neither half could read the other's data
        let reg = registry();
        let tags = reg.get("fleet.vehicle").unwrap().field("tag_ids").unwrap();
        assert!(matches!(
            tags.ty,
            FieldType::Many2many {
                ref relation,
                ref column1,
                ref column2,
                ..
            } if relation == "fleet_vehicle_vehicle_tag_rel"
                && column1 == "vehicle_tag_id"
                && column2 == "tag_id"
        ));
    }

    #[test]
    fn the_fleet_has_its_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        let mut vehicle = methods.names_for("fleet.vehicle");
        vehicle.sort_unstable();
        assert_eq!(
            vehicle,
            vec![
                "action_accept_driver_change",
                "action_archive",
                "action_assign_driver",
                "action_load_from_model",
                "action_plan_driver_change",
                "open_assignation_logs",
                "update_odometer",
            ]
        );
        let mut contract = methods.names_for("fleet.vehicle.log.contract");
        contract.sort_unstable();
        assert_eq!(
            contract,
            vec![
                "action_close",
                "action_draft",
                "action_expire",
                "action_open",
                "run_scheduler",
                "scheduler_manage_contract_expiration",
            ]
        );
    }

    #[test]
    fn the_fuel_list_is_odoos_own_and_in_its_order() {
        // a selection is a stored string: reordering is free, renaming is
        // a database nobody can read back
        assert_eq!(FUEL_TYPES[0].0, "diesel");
        assert_eq!(FUEL_TYPES[FUEL_TYPES.len() - 1].0, "electric");
        assert_eq!(FUEL_TYPES.len(), 9);
    }
}

//! The models of `odoo/addons/fleet/models/`.
//!
//! The shape is Odoo's: a catalogue (brand, model, category) that says
//! what a vehicle *is*, the vehicle itself, and the three logs that
//! record what happened to it — odometer readings, services and
//! contracts — plus the history of who drove it.

use crate::compute;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};

/// The fuels Odoo knows (`fleet_vehicle_model.py`'s `FUEL_TYPES`), in
/// its order — a selection is a stored value, and reordering it here
/// would make a database written by one version unreadable by the other.
pub const FUEL_TYPES: [(&str, &str); 9] = [
    ("diesel", "Diesel"),
    ("gasoline", "Gasoline"),
    ("full_hybrid", "Full Hybrid"),
    ("plug_in_hybrid_diesel", "Plug-in Hybrid Diesel"),
    ("plug_in_hybrid_gasoline", "Plug-in Hybrid Gasoline"),
    ("cng", "CNG"),
    ("lpg", "LPG"),
    ("hydrogen", "Hydrogen"),
    ("electric", "Electric"),
];

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn float(name: &str) -> Field {
    Field::new(name, FieldType::Float { digits: None })
}

fn integer(name: &str) -> Field {
    Field::new(name, FieldType::Integer)
}

fn boolean(name: &str) -> Field {
    Field::new(name, FieldType::Boolean)
}

fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

fn o2m(name: &str, comodel: &str, inverse: &str) -> Field {
    Field::new(
        name,
        FieldType::One2many {
            comodel: comodel.to_string(),
            inverse: inverse.to_string(),
        },
    )
}

fn selection(name: &str, options: &[(&str, &str)]) -> Field {
    Field::new(
        name,
        FieldType::Selection(
            options
                .iter()
                .map(|(value, label)| ((*value).to_string(), (*label).to_string()))
                .collect(),
        ),
    )
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The selections shared by a model and the vehicles built from it.
fn transmission(name: &str) -> Field {
    selection(name, &[("manual", "Manual"), ("automatic", "Automatic")])
}

fn power_unit(name: &str) -> Field {
    selection(name, &[("power", "kW"), ("horsepower", "Horsepower")])
        .default_value(serde_json::json!("power"))
        .required()
}

fn range_unit(name: &str) -> Field {
    selection(name, &[("km", "km"), ("mi", "mi")])
        .default_value(serde_json::json!("km"))
        .required()
}

fn fuel_type(name: &str) -> Field {
    selection(name, &FUEL_TYPES)
}

fn emission_unit(name: &str) -> Field {
    selection(name, &[("g/km", "g/km"), ("g/mi", "g/mi")])
        .default_value(serde_json::json!("g/km"))
        .computed(&["range_unit"], compute::emission_unit)
        .store()
}

/// Every model of the addon, in dependency order: a many2one may only
/// name a model the registry already has.
pub fn models() -> Vec<Model> {
    vec![
        vehicle_state(),
        vehicle_tag(),
        model_category(),
        service_type(),
        model_brand(),
        vehicle_model(),
        vehicle(),
        assignation_log(),
        odometer(),
        log_services(),
        log_contract(),
    ]
}

/// `fleet.vehicle.state` — the kanban column a vehicle sits in.
fn vehicle_state() -> Model {
    Model::new(
        meta("fleet.vehicle.state", "fleet_vehicle_state"),
        vec![
            char("name").required().translatable(),
            integer("sequence"),
            // a folded column is one the board collapses: "Downgraded" is
            // where vehicles end, not where anyone works
            boolean("fold"),
        ],
    )
    .ordered("sequence asc, id")
    .sql_constrained(
        "fleet_state_name_unique",
        r#"UNIQUE ("name")"#,
        "a vehicle state with that name already exists",
    )
}

/// `fleet.vehicle.tag` — the free labels a fleet manager sorts by.
fn vehicle_tag() -> Model {
    Model::new(
        meta("fleet.vehicle.tag", "fleet_vehicle_tag"),
        vec![char("name").required().translatable(), integer("color")],
    )
    .sql_constrained(
        "fleet_vehicle_tag_name_uniq",
        r#"UNIQUE ("name")"#,
        "a tag with that name already exists",
    )
}

/// `fleet.vehicle.model.category` — segment, van, truck.
fn model_category() -> Model {
    Model::new(
        meta("fleet.vehicle.model.category", "fleet_vehicle_model_category"),
        vec![char("name").required(), integer("sequence")],
    )
    .ordered("sequence asc, id asc")
    .sql_constrained(
        "fleet_vehicle_model_category_name_uniq",
        r#"UNIQUE ("name")"#,
        "a category with that name already exists",
    )
}

/// `fleet.service.type` — what a contract covers, or what a garage did.
fn service_type() -> Model {
    Model::new(
        meta("fleet.service.type", "fleet_service_type"),
        vec![
            char("name").required().translatable(),
            selection("category", &[("contract", "Contract"), ("service", "Service")]).required(),
        ],
    )
    .ordered("name, id")
}

/// `fleet.vehicle.model.brand` — the manufacturer.
fn model_brand() -> Model {
    Model::new(
        meta("fleet.vehicle.model.brand", "fleet_vehicle_model_brand"),
        vec![
            char("name").required(),
            boolean("active").default_value(serde_json::json!(true)),
            Field::new("image_128", FieldType::Binary),
            o2m("model_ids", "fleet.vehicle.model", "brand_id"),
            // not materialized, unlike Odoo's `store=True`: the number
            // changes when a *model* is created, and this ORM recomputes
            // a stored column only when the record that owns it is
            // written — a stored counter here would go stale and lie
            integer("model_count").computed(&["model_ids"], compute::model_count),
        ],
    )
    .ordered("name asc, id")
}

/// `fleet.vehicle.model` — a make and model, with the specifications
/// every vehicle built from it starts out with.
fn vehicle_model() -> Model {
    Model::new(
        meta("fleet.vehicle.model", "fleet_vehicle_model"),
        vec![
            char("name").required(),
            m2o("brand_id", "fleet.vehicle.model.brand")
                .required()
                .ondelete(OnDelete::Restrict),
            m2o("category_id", "fleet.vehicle.model.category"),
            Field::new(
                "vendors",
                FieldType::Many2many {
                    comodel: "res.partner".into(),
                    relation: "fleet_vehicle_model_vendors".into(),
                    column1: "model_id".into(),
                    column2: "partner_id".into(),
                },
            ),
            Field::new("image_128", FieldType::Binary).related("brand_id.image_128"),
            boolean("active").default_value(serde_json::json!(true)),
            selection("vehicle_type", &[("car", "Car"), ("bike", "Bike")])
                .default_value(serde_json::json!("car"))
                .required(),
            transmission("transmission"),
            o2m("vehicle_ids", "fleet.vehicle", "model_id"),
            integer("vehicle_count").computed(&["vehicle_ids"], compute::vehicle_count),
            // Odoo builds this selection at runtime, one entry per year
            // from 1970 to now. A selection here is fixed at compile
            // time, and a list that stops in the year the binary was
            // built is worse than a plain field.
            char("model_year"),
            char("color"),
            integer("seats"),
            integer("doors"),
            boolean("trailer_hook").default_value(serde_json::json!(false)),
            float("default_co2"),
            emission_unit("co2_emission_unit"),
            char("co2_standard"),
            fuel_type("default_fuel_type").default_value(serde_json::json!("electric")),
            float("power"),
            float("horsepower"),
            float("horsepower_tax"),
            boolean("electric_assistance").default_value(serde_json::json!(false)),
            power_unit("power_unit"),
            integer("vehicle_range"),
            range_unit("range_unit"),
            selection(
                "drive_type",
                &[
                    ("fwd", "Front-Wheel Drive (FWD)"),
                    ("awd", "All-Wheel Drive (AWD)"),
                    ("rwd", "Rear-Wheel Drive (RWD)"),
                    ("4wd", "Four-Wheel Drive (4WD)"),
                ],
            ),
        ],
    )
    .ordered("name asc, id")
}

/// `fleet.vehicle` — one car or bike.
///
/// The specification fields (`seats`, `power`, `fuel_type`, …) are plain
/// columns here where Odoo makes them computed-but-writable off the
/// model. This ORM has no such field: a compute is readonly. Writable
/// columns plus `action_load_from_model` keep the behaviour a user sees
/// — the model fills the form in, and whatever is special about this one
/// vehicle survives being typed over it.
fn vehicle() -> Model {
    Model::new(
        meta("fleet.vehicle", "fleet_vehicle"),
        vec![
            char("name")
                .computed(
                    &["model_id.brand_id.name", "model_id.name", "license_plate"],
                    compute::vehicle_name,
                )
                .store(),
            Field::new("description", FieldType::Html),
            boolean("active").default_value(serde_json::json!(true)),
            m2o("manager_id", "res.users"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            char("license_plate"),
            char("vin_sn"),
            boolean("trailer_hook").default_value(serde_json::json!(false)),
            m2o("driver_id", "res.partner"),
            m2o("future_driver_id", "res.partner"),
            m2o("model_id", "fleet.vehicle.model")
                .required()
                .ondelete(OnDelete::Restrict),
            m2o("brand_id", "fleet.vehicle.model.brand").related("model_id.brand_id"),
            selection("vehicle_type", &[("car", "Car"), ("bike", "Bike")])
                .related("model_id.vehicle_type"),
            Field::new("image_128", FieldType::Binary).related("model_id.brand_id.image_128"),
            o2m("log_drivers", "fleet.vehicle.assignation.log", "vehicle_id"),
            o2m("log_services", "fleet.vehicle.log.services", "vehicle_id"),
            o2m("log_contracts", "fleet.vehicle.log.contract", "vehicle_id"),
            // Odoo has no such one2many and searches the odometer table
            // instead. Declaring the inverse that already exists is what
            // lets the last reading and the smart button be computed
            // rather than queried once per row.
            o2m("odometer_ids", "fleet.vehicle.odometer", "vehicle_id"),
            integer("contract_count").computed(&["log_contracts.state"], compute::contract_count),
            integer("service_count").computed(&["log_services"], compute::service_count),
            integer("odometer_count").computed(&["odometer_ids"], compute::odometer_count),
            integer("history_count").computed(&["log_drivers"], compute::history_count),
            Field::new("next_assignation_date", FieldType::Date),
            Field::new("order_date", FieldType::Date),
            Field::new("acquisition_date", FieldType::Date).default_from(defaults::TODAY),
            Field::new("write_off_date", FieldType::Date),
            Field::new("contract_date_start", FieldType::Date).default_from(defaults::TODAY),
            char("color"),
            // the state is a column on the board: emptying it when the
            // column is deleted beats refusing to delete the column
            m2o("state_id", "fleet.vehicle.state").ondelete(OnDelete::SetNull),
            char("location"),
            integer("seats"),
            char("model_year"),
            integer("doors"),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "fleet.vehicle.tag".into(),
                    relation: "fleet_vehicle_vehicle_tag_rel".into(),
                    // Odoo's own column names, odd as the first one is:
                    // changing them would make the two ports write
                    // different tables for the same relation
                    column1: "vehicle_tag_id".into(),
                    column2: "tag_id".into(),
                },
            ),
            float("odometer").computed(&["odometer_ids.value"], compute::last_odometer),
            selection("odometer_unit", &[("kilometers", "km"), ("miles", "mi")])
                .default_value(serde_json::json!("kilometers"))
                .required(),
            transmission("transmission"),
            fuel_type("fuel_type"),
            power_unit("power_unit"),
            float("horsepower"),
            float("horsepower_tax"),
            float("power"),
            float("co2"),
            emission_unit("co2_emission_unit"),
            char("co2_standard"),
            m2o("category_id", "fleet.vehicle.model.category"),
            boolean("contract_renewal_due_soon").computed(
                &["log_contracts.expiration_date", "log_contracts.state"],
                compute::contract_renewal_due_soon,
            ),
            boolean("contract_renewal_overdue").computed(
                &["log_contracts.expiration_date", "log_contracts.state"],
                compute::contract_renewal_overdue,
            ),
            selection(
                "contract_state",
                &[
                    ("futur", "Incoming"),
                    ("open", "In Progress"),
                    ("expired", "Expired"),
                    ("closed", "Closed"),
                ],
            )
            .computed(
                &["log_contracts.expiration_date", "log_contracts.state"],
                compute::contract_state,
            ),
            float("car_value"),
            float("net_car_value"),
            float("residual_value"),
            boolean("plan_to_change_car").default_value(serde_json::json!(false)),
            boolean("plan_to_change_bike").default_value(serde_json::json!(false)),
            selection(
                "frame_type",
                &[
                    ("diamant", "Diamant"),
                    ("trapez", "Trapez"),
                    ("wave", "Wave"),
                ],
            ),
            boolean("electric_assistance").default_value(serde_json::json!(false)),
            float("frame_size"),
            integer("vehicle_range"),
            range_unit("range_unit"),
        ],
    )
    // Odoo's order, plus the id: two vehicles bought the same day with no
    // plate yet would otherwise come back in whatever order the last
    // update left them in
    .ordered("license_plate asc, acquisition_date asc, id")
}

/// `fleet.vehicle.assignation.log` — who drove this vehicle, and when.
fn assignation_log() -> Model {
    Model::new(
        meta(
            "fleet.vehicle.assignation.log",
            "fleet_vehicle_assignation_log",
        ),
        vec![
            m2o("vehicle_id", "fleet.vehicle")
                .required()
                .ondelete(OnDelete::Restrict),
            m2o("driver_id", "res.partner")
                .required()
                .ondelete(OnDelete::Restrict),
            Field::new("date_start", FieldType::Date),
            Field::new("date_end", FieldType::Date),
        ],
    )
    // the newest assignment first, like Odoo: the current driver is the
    // row at the top
    .ordered("date_start desc, id desc")
}

/// `fleet.vehicle.odometer` — one reading.
fn odometer() -> Model {
    Model::new(
        meta("fleet.vehicle.odometer", "fleet_vehicle_odometer"),
        vec![
            char("name")
                .computed(&["vehicle_id.name", "date"], compute::odometer_name)
                .store(),
            Field::new("date", FieldType::Date).default_from(defaults::TODAY),
            float("value"),
            m2o("vehicle_id", "fleet.vehicle")
                .required()
                .ondelete(OnDelete::Restrict),
            char("unit").related("vehicle_id.odometer_unit"),
            m2o("driver_id", "res.partner"),
        ],
    )
    .ordered("date desc, id desc")
}

/// `fleet.vehicle.log.services` — what a garage did, and what it cost.
fn log_services() -> Model {
    Model::new(
        meta("fleet.vehicle.log.services", "fleet_vehicle_log_services"),
        vec![
            boolean("active").default_value(serde_json::json!(true)),
            m2o("vehicle_id", "fleet.vehicle")
                .required()
                .ondelete(OnDelete::Restrict),
            m2o("model_id", "fleet.vehicle.model").related("vehicle_id.model_id"),
            m2o("brand_id", "fleet.vehicle.model.brand").related("vehicle_id.brand_id"),
            m2o("manager_id", "res.users").related("vehicle_id.manager_id"),
            Field::new("amount", FieldType::Monetary),
            char("description"),
            // the reading taken when the vehicle went in: the log points
            // at the odometer row rather than copying the number, so the
            // two can never disagree
            m2o("odometer_id", "fleet.vehicle.odometer").ondelete(OnDelete::SetNull),
            float("odometer").related("odometer_id.value"),
            char("odometer_unit").related("vehicle_id.odometer_unit"),
            Field::new("date", FieldType::Date).default_from(defaults::TODAY),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("purchaser_id", "res.partner"),
            char("inv_ref"),
            m2o("vendor_id", "res.partner"),
            Field::new("notes", FieldType::Text),
            m2o("service_type_id", "fleet.service.type")
                .required()
                .ondelete(OnDelete::Restrict),
            selection(
                "state",
                &[
                    ("new", "New"),
                    ("running", "Running"),
                    ("done", "Done"),
                    ("cancelled", "Cancelled"),
                ],
            )
            .default_value(serde_json::json!("new")),
        ],
    )
    .ordered("date desc, id desc")
}

/// `fleet.vehicle.log.contract` — leasing, insurance, maintenance plan:
/// something that covers the vehicle between two dates and has to be
/// renewed before the second one.
fn log_contract() -> Model {
    Model::new(
        meta("fleet.vehicle.log.contract", "fleet_vehicle_log_contract"),
        vec![
            m2o("vehicle_id", "fleet.vehicle")
                .required()
                .ondelete(OnDelete::Restrict),
            m2o("cost_subtype_id", "fleet.service.type").ondelete(OnDelete::SetNull),
            Field::new("amount", FieldType::Monetary),
            Field::new("date", FieldType::Date),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            char("name")
                .computed(
                    &["vehicle_id.name", "cost_subtype_id.name"],
                    compute::contract_name,
                )
                .store(),
            boolean("active").default_value(serde_json::json!(true)),
            m2o("user_id", "res.users"),
            Field::new("start_date", FieldType::Date).default_from(defaults::TODAY),
            // Odoo defaults this to a year after the start; a default
            // here is a constant or a framework function, and "a year
            // from now" is neither. The wizard-free path is that whoever
            // creates the contract types the date the paper says.
            Field::new("expiration_date", FieldType::Date),
            integer("days_left").computed(&["expiration_date", "state"], compute::days_left),
            boolean("expires_today")
                .computed(&["expiration_date", "state"], compute::expires_today),
            m2o("insurer_id", "res.partner"),
            m2o("purchaser_id", "res.partner").related("vehicle_id.driver_id"),
            Field::new("ins_ref", FieldType::Char { size: Some(64) }),
            selection(
                "state",
                &[
                    ("futur", "New"),
                    ("open", "Running"),
                    ("expired", "Expired"),
                    ("closed", "Cancelled"),
                ],
            )
            .default_value(serde_json::json!("open")),
            Field::new("notes", FieldType::Html),
            Field::new("cost_generated", FieldType::Monetary),
            selection(
                "cost_frequency",
                &[
                    ("no", "No"),
                    ("daily", "Daily"),
                    ("weekly", "Weekly"),
                    ("monthly", "Monthly"),
                    ("yearly", "Yearly"),
                ],
            )
            .default_value(serde_json::json!("monthly"))
            .required(),
            Field::new(
                "service_ids",
                FieldType::Many2many {
                    comodel: "fleet.service.type".into(),
                    relation: "fleet_service_type_log_contract_rel".into(),
                    column1: "contract_id".into(),
                    column2: "service_type_id".into(),
                },
            ),
        ],
    )
    // Odoo's `state desc, expiration_date`: what is running comes before
    // what is cancelled, and inside each, what runs out first
    .ordered("state desc, expiration_date, id")
}

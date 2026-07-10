//! Data file parsing: the `<record>`/`<field>` XML format and model
//! CSVs. Port of `odoo/tools/convert.py` (the declarative subset).

use crate::pyliteral::parse_py_literal;
use quick_xml::events::Event;
use quick_xml::Reader;
use rusdoo_core::RusdooError;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// element text content
    Text(String),
    /// `ref="module.xml_id"`
    Ref(String),
    /// `eval="..."` parsed as a python literal
    Eval(Value),
}

#[derive(Debug, Clone)]
pub struct DataRecord {
    /// external id, unqualified (the loading module qualifies it)
    pub xml_id: Option<String>,
    pub model: String,
    pub fields: Vec<(String, FieldValue)>,
    /// records inside `<data noupdate="1">` are never overwritten
    pub noupdate: bool,
}

fn xml_err(context: &str, e: impl std::fmt::Display) -> RusdooError {
    RusdooError::Validation(format!("xml data: {context}: {e}"))
}

fn attr_map(element: &quick_xml::events::BytesStart) -> Result<Vec<(String, String)>, RusdooError> {
    let mut attrs = Vec::new();
    for attr in element.attributes() {
        let attr = attr.map_err(|e| xml_err("bad attribute", e))?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
        let value = attr
            .unescape_value()
            .map_err(|e| xml_err("bad attribute value", e))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(attrs)
}

fn get_attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

struct PendingField {
    name: String,
    reference: Option<String>,
    eval: Option<String>,
    text: String,
}

struct PendingRecord {
    xml_id: Option<String>,
    model: String,
    fields: Vec<(String, FieldValue)>,
    noupdate: bool,
}

fn finish_field(pending: PendingField) -> Result<(String, FieldValue), RusdooError> {
    let value =
        if let Some(reference) = pending.reference {
            FieldValue::Ref(reference)
        } else if let Some(eval) = pending.eval {
            FieldValue::Eval(parse_py_literal(&eval).map_err(|e| {
                RusdooError::Validation(format!("field {:?} eval: {e}", pending.name))
            })?)
        } else {
            FieldValue::Text(pending.text.trim().to_string())
        };
    Ok((pending.name, value))
}

/// Parse an addon data XML file into records. Non-record elements
/// (menuitem, template, function, ...) are skipped — tracked gap until
/// their loaders are ported.
pub fn parse_xml_data(source: &str) -> Result<Vec<DataRecord>, RusdooError> {
    let mut reader = Reader::from_str(source);
    let mut records: Vec<DataRecord> = Vec::new();
    let mut noupdate = false;
    let mut record: Option<PendingRecord> = None;
    let mut field: Option<PendingField> = None;
    let mut skip_depth = 0usize;

    enum EventKind {
        Empty,
        Other,
    }
    loop {
        let event = reader.read_event().map_err(|e| xml_err("parse", e))?;
        let event_kind = if matches!(event, Event::Empty(_)) {
            EventKind::Empty
        } else {
            EventKind::Other
        };
        if skip_depth > 0 {
            match event {
                Event::Start(_) => skip_depth += 1,
                Event::End(_) => skip_depth -= 1,
                Event::Eof => return Err(xml_err("parse", "unexpected end of file")),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_) => {}
            Event::Start(element) | Event::Empty(element) => {
                let self_closing = matches!(event_kind, EventKind::Empty);
                let name = element.name();
                let attrs = attr_map(&element)?;
                match name.as_ref() {
                    b"odoo" | b"openerp" => {}
                    b"data" => {
                        let flag = matches!(get_attr(&attrs, "noupdate"), Some("1") | Some("True"));
                        if self_closing {
                            // empty <data/> toggles nothing
                        } else {
                            noupdate = flag;
                        }
                    }
                    b"record" => {
                        if record.is_some() {
                            return Err(xml_err("structure", "nested <record>"));
                        }
                        let model = get_attr(&attrs, "model")
                            .ok_or_else(|| xml_err("record", "missing model attribute"))?
                            .to_string();
                        let pending = PendingRecord {
                            xml_id: get_attr(&attrs, "id").map(str::to_string),
                            model,
                            fields: Vec::new(),
                            noupdate,
                        };
                        if self_closing {
                            records.push(DataRecord {
                                xml_id: pending.xml_id,
                                model: pending.model,
                                fields: pending.fields,
                                noupdate: pending.noupdate,
                            });
                        } else {
                            record = Some(pending);
                        }
                    }
                    b"field" => {
                        let Some(current) = record.as_mut() else {
                            return Err(xml_err("structure", "<field> outside <record>"));
                        };
                        let pending = PendingField {
                            name: get_attr(&attrs, "name")
                                .ok_or_else(|| xml_err("field", "missing name attribute"))?
                                .to_string(),
                            reference: get_attr(&attrs, "ref").map(str::to_string),
                            eval: get_attr(&attrs, "eval").map(str::to_string),
                            text: String::new(),
                        };
                        if self_closing {
                            current.fields.push(finish_field(pending)?);
                        } else {
                            field = Some(pending);
                        }
                    }
                    _ => {
                        if field.is_some() {
                            return Err(xml_err(
                                "field",
                                "markup inside <field> not yet supported",
                            ));
                        }
                        if record.is_some() {
                            return Err(xml_err("record", "unexpected element inside <record>"));
                        }
                        // menuitem/template/function/...: skip the subtree
                        if !self_closing {
                            skip_depth = 1;
                        }
                    }
                }
            }
            Event::Text(text) => {
                if let Some(pending) = field.as_mut() {
                    pending
                        .text
                        .push_str(&text.unescape().map_err(|e| xml_err("text", e))?);
                }
            }
            Event::CData(cdata) => {
                if let Some(pending) = field.as_mut() {
                    pending
                        .text
                        .push_str(&String::from_utf8_lossy(&cdata.into_inner()));
                }
            }
            Event::End(element) => match element.name().as_ref() {
                b"field" => {
                    let pending = field.take().ok_or_else(|| xml_err("field", "stray end"))?;
                    record
                        .as_mut()
                        .expect("field only opens inside a record")
                        .fields
                        .push(finish_field(pending)?);
                }
                b"record" => {
                    let pending = record
                        .take()
                        .ok_or_else(|| xml_err("record", "stray end"))?;
                    records.push(DataRecord {
                        xml_id: pending.xml_id,
                        model: pending.model,
                        fields: pending.fields,
                        noupdate: pending.noupdate,
                    });
                }
                b"data" => noupdate = false,
                _ => {}
            },
        }
    }
    Ok(records)
}

/// Model CSVs: the file name is the model, `id` holds the external id,
/// columns named `x:id` (or `x/id`) are references.
pub fn parse_csv_data(model: &str, source: &str) -> Result<Vec<DataRecord>, RusdooError> {
    let mut reader = csv::Reader::from_reader(source.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| RusdooError::Validation(format!("csv {model}: {e}")))?
        .clone();
    let mut records = Vec::new();
    for row in reader.records() {
        let row = row.map_err(|e| RusdooError::Validation(format!("csv {model}: {e}")))?;
        let mut xml_id = None;
        let mut fields = Vec::new();
        for (header, value) in headers.iter().zip(row.iter()) {
            if header == "id" {
                if !value.is_empty() {
                    xml_id = Some(value.to_string());
                }
            } else if let Some(name) = header.strip_suffix(":id").or(header.strip_suffix("/id")) {
                if !value.is_empty() {
                    fields.push((name.to_string(), FieldValue::Ref(value.to_string())));
                }
            } else if !value.is_empty() {
                fields.push((header.to_string(), FieldValue::Text(value.to_string())));
            }
        }
        records.push(DataRecord {
            xml_id,
            model: model.to_string(),
            fields,
            noupdate: false,
        });
    }
    Ok(records)
}

//! Fidelity probe against the REAL Odoo 19 `base` addon: parse its
//! manifest and every data file with our parsers, and report exactly
//! how much loads. Not an assertion of completeness — a measurement.

use rusdoo_modules::data::{parse_csv_data, parse_xml_data};
use rusdoo_modules::manifest::parse_manifest;
use std::path::Path;

#[test]
fn measure_base_addon_parse_coverage() {
    let base = Path::new("../../odoo/odoo/addons/base");
    if !base.exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }
    let manifest_src = std::fs::read_to_string(base.join("__manifest__.py")).unwrap();
    let manifest = parse_manifest(&manifest_src, "base").expect("base manifest parses");
    println!("\n=== base addon fidelity ===");
    println!(
        "manifest: v{} — {} data files",
        manifest.version,
        manifest.data.len()
    );

    let (mut ok, mut fail, mut records) = (0usize, 0usize, 0usize);
    let mut failures: Vec<(String, String)> = Vec::new();
    for data_file in &manifest.data {
        let path = base.join(data_file);
        let Ok(src) = std::fs::read_to_string(&path) else {
            failures.push((data_file.clone(), "file missing".into()));
            fail += 1;
            continue;
        };
        let parsed = if data_file.ends_with(".xml") {
            parse_xml_data(&src).map(|r| r.len())
        } else {
            let model = Path::new(data_file).file_stem().unwrap().to_str().unwrap();
            parse_csv_data(model, &src).map(|r| r.len())
        };
        match parsed {
            Ok(n) => {
                ok += 1;
                records += n;
            }
            Err(e) => {
                fail += 1;
                let msg = e.to_string();
                let short = msg
                    .split(':')
                    .next_back()
                    .unwrap_or(&msg)
                    .trim()
                    .to_string();
                failures.push((data_file.clone(), short));
            }
        }
    }
    println!(
        "parsed: {ok}/{} files ok ({records} records), {fail} failed",
        manifest.data.len()
    );
    // group failure reasons
    use std::collections::BTreeMap;
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for (_, why) in &failures {
        *reasons.entry(why.clone()).or_default() += 1;
    }
    if !reasons.is_empty() {
        println!("failure reasons:");
        for (why, count) in &reasons {
            println!("  {count:>3}x  {why}");
        }
        println!("first few failing files:");
        for (f, why) in failures.iter().take(6) {
            println!("  {f}  ->  {why}");
        }
    }
    println!("===========================\n");
}

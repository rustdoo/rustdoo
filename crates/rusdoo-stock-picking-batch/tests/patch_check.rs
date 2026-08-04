use std::fs;
fn arch(path: &str, id: &str) -> String {
    let text = fs::read_to_string(path).unwrap();
    let head = text
        .find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("no {id}"));
    let rest = &text[head..];
    let start = rest.find("<field name=\"arch\" type=\"xml\">").unwrap()
        + "<field name=\"arch\" type=\"xml\">".len();
    let end = rest[start..].find("</field>").unwrap();
    rest[start..start + end].to_string()
}

#[test]
fn the_patches_apply_to_stocks_real_views() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let base_file = format!("{root}/addons/stock/views/stock_picking_views.xml");
    let patch_file = format!("{root}/addons/stock_picking_batch/views/stock_picking_views.xml");
    for (base_id, patch_id) in [
        ("view_picking_form", "view_picking_form_batch"),
        ("view_picking_list", "view_picking_list_batch"),
        ("view_picking_search", "view_picking_search_batch"),
    ] {
        let merged = rusdoo_http::view_inherit::apply_inheritance(
            &arch(&base_file, base_id),
            &arch(&patch_file, patch_id),
        )
        .unwrap_or_else(|e| panic!("{patch_id}: {e}"));
        assert!(merged.contains("batch_id"), "{patch_id}: {merged}");
    }
}

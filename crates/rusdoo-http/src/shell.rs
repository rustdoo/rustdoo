//! The page that boots the web client: a port of the head of Odoo's
//! `web.layout` + `web.webclient_bootstrap`
//! (`addons/web/views/webclient_templates.xml`).
//!
//! Three things have to be in the document, in this order, or the client
//! never starts: the `odoo` global, the session it reads at module level,
//! and only then the bundle. Every transpiled ES6 module in the bundle
//! opens with `odoo.define(...)`.

use crate::dispatch::OrmService;
use crate::session::Session;
use serde_json::Value;

/// The bundles the backend page can be loaded from, most faithful first.
///
/// Odoo's `web.webclient_bootstrap` calls `web.assets_web`, which includes
/// `web.assets_backend` and adds `main.js`/`start.js` — the contribution
/// bundle alone has no entry point. A tree without Odoo's own `web` addon
/// declares only `web.assets_backend`, which is where this port's own
/// client lives, so that stays the fallback.
const CLIENT_BUNDLES: [&str; 2] = ["web.assets_web", "web.assets_backend"];

/// The shell, or `None` when no installed addon contributes a client —
/// which is what keeps a server booted without the `web` addon serving
/// its plain index instead.
pub(crate) async fn client_shell(service: &OrmService, session: Option<&Session>) -> Option<String> {
    let bundles = service.assets.bundles();
    let bundle = CLIENT_BUNDLES.into_iter().find(|name| {
        bundles
            .files_with_extension(name, &["js", "mjs"])
            .next()
            .is_some()
    })?;
    let has_styles = bundles
        .files_with_extension(bundle, &["css", "scss", "less"])
        .next()
        .is_some();

    let session_info = service.session_info(session).await;
    let csrf = session.map(|s| s.csrf_token.as_str()).unwrap_or_default();

    let mut shell = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>rusdoo</title>",
    );
    // `web.layout`, verbatim in shape: the global carries the CSRF token
    // and the debug flag, and nothing else. Serving assets per file in
    // debug mode is not ported, so the flag stays off.
    shell.push_str(&format!(
        "<script id=\"web.layout.odooscript\" type=\"text/javascript\">\
         var odoo = {{ csrf_token: \"{}\", debug: \"\" }};\
         </script>",
        script_string(csrf)
    ));
    // `web.webclient_bootstrap`: the session the client reads before its
    // first render, and the menus it awaits. Odoo appends the `lang`
    // override with a bare `&`, which cannot be a query separator on a
    // URL with no `?`; ours is spelled the way a server can read it.
    shell.push_str(&format!(
        "<script type=\"text/javascript\">\
         {{\n\
         odoo.__session_info__ = {};\n\
         const {{ user_context }} = odoo.__session_info__;\n\
         const lang = new URLSearchParams(document.location.search).get(\"lang\");\n\
         let menuURL = \"/web/webclient/load_menus\";\n\
         if (lang) {{ user_context.lang = lang; menuURL += `?lang=${{lang}}`; }}\n\
         odoo.reloadMenus = () => fetch(menuURL, {{ cache: \"no-store\" }}).then((res) => res.json());\n\
         odoo.loadMenusPromise = odoo.reloadMenus();\n\
         }}\
         </script>",
        script_json(&session_info)
    ));
    if has_styles {
        shell.push_str(&format!(
            "<link rel=\"stylesheet\" href=\"/web/assets/{bundle}.css\">"
        ));
    }
    shell.push_str(&format!(
        "<script defer src=\"/web/assets/{bundle}.js\"></script>\
         </head><body><div id=\"rusdoo-app\"></div></body></html>"
    ));
    Some(shell)
}

/// JSON for a `<script>` body. A script element is raw text: an HTML
/// escape would land in the parsed JSON, so the escape has to be a
/// JavaScript one. `<` is enough — no `</script>` and no `<!--`
/// can survive it — and it is still valid JSON.
fn script_json(value: &Value) -> String {
    value.to_string().replace('<', "\\u003c")
}

/// The same, for a bare string interpolated inside a JS string literal.
fn script_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('<', "\\u003c")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_value_cannot_close_the_script_tag() {
        let payload = json!({"name": "</script><img src=x onerror=alert(1)>"});
        let rendered = script_json(&payload);
        assert!(!rendered.contains("</script>"), "{rendered}");
        assert!(!rendered.contains('<'), "{rendered}");
        // and it is still the same JSON to a parser
        let back: Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(back, payload);
    }

    #[test]
    fn a_token_cannot_break_out_of_its_literal() {
        let hostile = "a\" ; alert(1); var x = \"</script>";
        let rendered = script_string(hostile);
        assert!(!rendered.contains('<'), "{rendered}");
        // quoted, it is one closed string literal holding the original —
        // JSON reads the same escapes JavaScript does
        let literal = format!("\"{rendered}\"");
        let back: Value = serde_json::from_str(&literal).expect("one closed literal");
        assert_eq!(back, json!(hostile));
    }
}

//! Turning an addon's ES6 JavaScript into the module system its client
//! actually loads, port of `odoo/tools/js_transpiler.py`.
//!
//! An Odoo addon writes `/** @odoo-module */` and then plain `import` and
//! `export`. What reaches the browser is not that: it is
//! `odoo.define(name, deps, function (require) { ... })`, and the server
//! is what rewrites one into the other while it builds the bundle.
//!
//! This is the piece without which no modern addon's JavaScript loads at
//! all. An addon whose `import` statements were served as written would
//! be a syntax error in a bundle, taking every file after it down too.
//!
//! Deliberately the same design as Odoo's, including its limits: this is
//! regular expressions and not a JavaScript parser. Odoo says so itself
//! — *"one can only expect to cover as much edge cases as possible with
//! reasonable limitations"* — and matching its behaviour matters more
//! than being right where it is wrong, because the files being
//! transformed are the ones that were written against it.
//!
//! One difference, and it is mechanical. Odoo writes a quoted path as
//! `(?P<quote>["'`])([^"'`]+)(?P=quote)`, a backreference this regex
//! engine does not have. The same thing spelled without one is an
//! alternation over the three quote characters, each alternative
//! forbidding all three inside — which is what the original means.

use rusdoo_core::RusdooError;
use regex::{Captures, Regex};
use std::sync::LazyLock;

/// A quoted path, as Odoo's `(?P<quote>["'`])([^"'`]+)(?P=quote)` — see
/// the module docs on why it is spelled out rather than backreferenced.
const QUOTED: &str = r#"("[^"'`]+"|'[^"'`]+'|`[^"'`]+`)"#;

/// The same, allowing an empty path, for the `/index` rewrite.
const QUOTED_INDEX: &str = r#"("[^"'`]*/index/?"|'[^"'`]*/index/?'|`[^"'`]*/index/?`)"#;

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a transpiler pattern is written here, not read from data")
}

/// `/module/.../static/(src|tests|lib)/rest`, the shape every asset URL
/// has and the only thing the module name can be derived from.
static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?x) ^ /? (?P<module>\S+?) /(?:[\S/]*/)? static/ (?P<type>src|tests|lib) (?P<url>/[\S/]*) $")
});

/// The `@odoo-module` comment, with the options it may carry.
static ODOO_MODULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?xs) ^ \s* /(?:\*|/) .*? @odoo-module
          (?P<ignore>\s+ignore)?
          (?:\s+alias=(?P<alias>[^\s*]+))?
          (?:\s+default=(?P<default>[\w$]+))?",
    )
});

/// The dotted name an addon's file is known by inside the client.
///
/// `web/static/src/one/two.js` becomes `@web/one/two`: the module's name
/// with an `@`, mapped onto its `static/src`. A file under `static/lib`
/// or `static/tests` is reached through `..`, which is how the client
/// tells the three apart without a second naming scheme.
pub fn url_to_module_path(url: &str) -> Result<String, RusdooError> {
    let found = URL_RE.captures(url).ok_or_else(|| {
        RusdooError::Validation(format!(
            "the js file {url:?} must be under '/static/src', '/static/lib' or '/static/tests'"
        ))
    })?;
    let module = &found["module"];
    let mut path = found["url"].to_string();
    // a directory holding an `index.js` is reached by its own name: the
    // client imports `@web/views`, not `@web/views/index`
    if path.ends_with("/index.js") {
        path.truncate(path.len() - "/index.js".len());
    } else if path.ends_with("/index") {
        path.truncate(path.len() - "/index".len());
    } else if path.ends_with(".js") {
        path.truncate(path.len() - 3);
    }
    Ok(match &found["type"] {
        "src" => format!("@{module}{path}"),
        "lib" => format!("@{module}/../lib{path}"),
        _ => format!("@{module}/../tests{path}"),
    })
}

/// Whether this file is one the client loads as a module.
///
/// Anything under an addon's `static/src` or `static/tests` is, whether
/// or not it says so — that is Odoo's rule, and it is why an addon does
/// not have to annotate every file. `@odoo-module ignore` opts one out.
pub fn is_odoo_module(url: &str, content: &str) -> bool {
    let annotation = ODOO_MODULE_RE.captures(content);
    if annotation
        .as_ref()
        .is_some_and(|found| found.name("ignore").is_some())
    {
        return false;
    }
    if let Some(addon) = url.split('/').nth(1) {
        if url.starts_with(&format!("/{addon}/static/src"))
            || url.starts_with(&format!("/{addon}/static/tests"))
        {
            return true;
        }
    }
    annotation.is_some()
}

/// One addon file, as the module the client defines and requires.
pub fn transpile(url: &str, content: &str) -> Result<String, RusdooError> {
    let module_path = url_to_module_path(url)?;
    // computed before anything is rewritten: the annotation is read off
    // the source as the addon wrote it
    let legacy = aliased_define(&module_path, content);

    let mut dependencies: Vec<String> = Vec::new();
    let mut out = content.to_string();
    // the order is Odoo's, and it matters: a default-and-named import has
    // to be recognised before the plain default one would swallow half
    // of it, and the wrapping comes after everything it wraps
    out = convert_legacy_default_import(&out);
    out = convert_basic_import(&out);
    out = convert_default_and_named_import(&out);
    out = convert_default_and_star_import(&out);
    out = convert_default_import(&out);
    out = convert_star_import(&out);
    out = convert_unnamed_relative_import(&out);
    out = convert_from_export(&out);
    out = convert_star_from_export(&out);
    out = remove_index(&out);
    out = convert_relative_require(url, &mut dependencies, &out)?;
    out = convert_export_function(&out);
    out = convert_export_class(&out);
    out = convert_variable_export(&out);
    out = convert_object_export(&out);
    out = convert_default_export(&out);
    out = wrap_with_qunit_module(url, &out);
    out = wrap_with_odoo_define(&module_path, &dependencies, &out);
    out = convert_t(url, &out)?;
    if let Some(alias) = legacy {
        out.push_str(&alias);
    }
    Ok(out)
}

// -- the wrapping ---------------------------------------------------------

fn wrap_with_odoo_define(module_path: &str, dependencies: &[String], content: &str) -> String {
    let deps = dependencies
        .iter()
        .map(|dep| format!("'{dep}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "odoo.define('{module_path}', [{deps}], function (require) {{\n\
         'use strict';\n\
         let __exports = {{}};\n\
         {content}\n\
         return __exports;\n\
         }});\n"
    )
}

static QUNIT_RE: LazyLock<Regex> = LazyLock::new(|| compile(r"QUnit\.(test|debug|only)\("));

fn wrap_with_qunit_module(url: &str, content: &str) -> String {
    if url.contains("tests") && QUNIT_RE.is_match(content) {
        if let Some(found) = URL_RE.captures(url) {
            return format!(
                "QUnit.module(\"{}\", function() {{{content}}});",
                &found["module"]
            );
        }
    }
    content.to_string()
}

/// The second, smaller module that lets legacy `require("web.Widget")`
/// still reach a file that has been renamed to a path.
fn aliased_define(module_path: &str, content: &str) -> Option<String> {
    let found = ODOO_MODULE_RE.captures(content)?;
    let alias = found.name("alias")?.as_str();
    // `default=` anything means the legacy name is the whole module;
    // without it, the legacy system expected a default export
    let body = if found.name("default").is_some() {
        format!("return require('{module_path}');")
    } else {
        format!("return require('{module_path}')[Symbol.for(\"default\")];")
    };
    Some(format!(
        "\nodoo.define(`{alias}`, ['{module_path}'], function (require) {{\n\
         {}{body}\n\
         {}}});\n",
        " ".repeat(24),
        " ".repeat(24)
    ))
}

// -- exports --------------------------------------------------------------

static EXPORT_FCT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+ (?P<type>(?:async\s+)?function)\s+ (?P<identifier>[\w$]+)")
});

fn convert_export_function(content: &str) -> String {
    EXPORT_FCT_RE
        .replace_all(content, "${space}__exports.$identifier = $identifier; $type $identifier")
        .into_owned()
}

static EXPORT_CLASS_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+ (?P<type>class)\s+ (?P<identifier>[\w$]+)")
});

fn convert_export_class(content: &str) -> String {
    EXPORT_CLASS_RE
        .replace_all(
            content,
            "${space}const $identifier = __exports.$identifier = $type $identifier",
        )
        .into_owned()
}

static EXPORT_VAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+ (?P<type>let|const|var)\s+ (?P<identifier>[\w$]+)")
});

fn convert_variable_export(content: &str) -> String {
    EXPORT_VAR_RE
        .replace_all(content, "$space$type $identifier = __exports.$identifier")
        .into_owned()
}

static EXPORT_OBJECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s* (?P<object>\{[\w$\s,]+\})")
});

fn convert_object_export(content: &str) -> String {
    EXPORT_OBJECT_RE
        .replace_all(content, |found: &Captures| {
            format!(
                "{}Object.assign(__exports, {})",
                &found["space"],
                rewrite_object(&found["object"], convert_as, ", ")
            )
        })
        .into_owned()
}

static EXPORT_FROM_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"(?xm) ^ (?P<space>[^\S\n]*) export\s* (?P<object>\{{[\w$\s,]+\}})\s* from\s* (?P<path>{QUOTED})"
    ))
});

fn convert_from_export(content: &str) -> String {
    EXPORT_FROM_RE
        .replace_all(content, |found: &Captures| {
            format!(
                "{}{{const {} = require({});Object.assign(__exports, {})}}",
                &found["space"],
                rewrite_object(&found["object"], remove_as, ","),
                &found["path"],
                rewrite_object(&found["object"], convert_as, ", "),
            )
        })
        .into_owned()
}

static EXPORT_STAR_FROM_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"(?xm) ^ (?P<space>[^\S\n]*) export\s*\*\s*from\s* (?P<path>{QUOTED})"
    ))
});

fn convert_star_from_export(content: &str) -> String {
    EXPORT_STAR_FROM_RE
        .replace_all(content, "${space}Object.assign(__exports, require($path))")
        .into_owned()
}

static EXPORT_FCT_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+default\s+ (?P<type>(?:async\s+)?function)\s+ (?P<identifier>[\w$]+)")
});

static EXPORT_CLASS_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+default\s+ (?P<type>class)\s+ (?P<identifier>[\w$]+)")
});

static EXPORT_DEFAULT_VAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+default\s+ (?P<type>let|const|var)\s+ (?P<identifier>[\w$]+)\s*")
});

static EXPORT_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) export\s+default (?:\s+[\w$]+\s*=)?")
});

fn convert_default_export(content: &str) -> String {
    let out = EXPORT_FCT_DEFAULT_RE.replace_all(
        content,
        "${space}__exports[Symbol.for(\"default\")] = $identifier; $type $identifier",
    );
    let out = EXPORT_CLASS_DEFAULT_RE.replace_all(
        &out,
        "${space}const $identifier = __exports[Symbol.for(\"default\")] = $type $identifier",
    );
    let out = EXPORT_DEFAULT_VAR_RE.replace_all(
        &out,
        "$space$type $identifier = __exports[Symbol.for(\"default\")]",
    );
    EXPORT_DEFAULT_RE
        .replace_all(&out, "${space}__exports[Symbol.for(\"default\")] =")
        .into_owned()
}

// -- imports --------------------------------------------------------------

static IMPORT_BASIC_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"(?xm) ^ (?P<space>[^\S\n]*) import\s+ (?P<object>\{{[\s\w$,]+\}})\s* from\s* (?P<path>{QUOTED})"
    ))
});

fn convert_basic_import(content: &str) -> String {
    IMPORT_BASIC_RE
        .replace_all(content, |found: &Captures| {
            format!(
                "{}const {} = require({})",
                &found["space"],
                found["object"].replace(" as ", ": "),
                &found["path"]
            )
        })
        .into_owned()
}

/// A legacy path is a dotted name (`web.Widget`), not a path and not an
/// `@module/...`: that is what the leading `[^@\."'`]` says.
static IMPORT_LEGACY_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?xm) ^ (?P<space>[^\S\n]*) import\s+ (?P<identifier>[\w$]+)\s* from\s*
           (?P<path>"[^@\."'`][^"'`]*"|'[^@\."'`][^"'`]*'|`[^@\."'`][^"'`]*`)"#,
    )
});

fn convert_legacy_default_import(content: &str) -> String {
    IMPORT_LEGACY_DEFAULT_RE
        .replace_all(content, "${space}const $identifier = require($path)")
        .into_owned()
}

static IMPORT_DEFAULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"(?xm) ^ (?P<space>[^\S\n]*) import\s+ (?P<identifier>[\w$]+)\s* from\s* (?P<path>{QUOTED})"
    ))
});

fn convert_default_import(content: &str) -> String {
    IMPORT_DEFAULT_RE
        .replace_all(
            content,
            "${space}const $identifier = require($path)[Symbol.for(\"default\")]",
        )
        .into_owned()
}

static IS_PATH_LEGACY_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r#"^("[^@\."'`][^"'`]*"|'[^@\."'`][^"'`]*'|`[^@\."'`][^"'`]*`)"#)
});

static IMPORT_DEFAULT_AND_NAMED_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"(?xm) ^ (?P<space>[^\S\n]*) import\s+ (?P<default_export>[\w$]+)\s*,\s*
          (?P<named_exports>\{{[\s\w$,]+\}})\s* from\s* (?P<path>{QUOTED})"
    ))
});

fn convert_default_and_named_import(content: &str) -> String {
    IMPORT_DEFAULT_AND_NAMED_RE
        .replace_all(content, |found: &Captures| {
            let named = found["named_exports"].replace(" as ", ": ");
            let space = &found["space"];
            let default = &found["default_export"];
            let path = &found["path"];
            if IS_PATH_LEGACY_RE.is_match(path) {
                // a legacy module has no default slot: the module *is*
                // the default, and the named parts come off it
                return format!(
                    "{space}const {default} = require({path});\n{space}const {named} = {default}"
                );
            }
            format!(
                "{space}const {{ [Symbol.for(\"default\")]: {default},{} = require({path})",
                &named[1..]
            )
        })
        .into_owned()
}

static IMPORT_STAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?xm) ^ (?P<space>[^\S\n]*) import\s+\*\s+as\s+ (?P<identifier>[\w$]+) \s*from\s* (?P<path>[^;\n]+)")
});

fn convert_star_import(content: &str) -> String {
    IMPORT_STAR_RE
        .replace_all(content, "${space}const $identifier = require($path)")
        .into_owned()
}

static IMPORT_DEFAULT_AND_STAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?xm) ^ (?P<space>[^\S\n]*) import\s+ (?P<default_export>[\w$]+)\s*,\s*
          \*\s+as\s+ (?P<alias>[\w$]+) \s*from\s* (?P<path>[^;\n]+)",
    )
});

fn convert_default_and_star_import(content: &str) -> String {
    IMPORT_DEFAULT_AND_STAR_RE
        .replace_all(
            content,
            "${space}const $alias = require($path);\n${space}const $default_export = $alias[Symbol.for(\"default\")]",
        )
        .into_owned()
}

static IMPORT_UNNAMED_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?xm) ^ (?P<space>[^\S\n]*) import\s+ (?P<path>[^;\n]+)"));

fn convert_unnamed_relative_import(content: &str) -> String {
    IMPORT_UNNAMED_RE
        .replace_all(content, "${space}require($path)")
        .into_owned()
}

// -- requires -------------------------------------------------------------

static URL_INDEX_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r"(?xm) require\s*\(\s* (?P<path>{QUOTED_INDEX}) \s*\)"
    ))
});

fn remove_index(content: &str) -> String {
    URL_INDEX_RE
        .replace_all(content, |found: &Captures| {
            let path = &found["path"];
            let cut = path.rfind("/index").expect("the pattern matched on it");
            // the quote character the path opened with closes it again
            format!("require({}{})", &path[..cut], &path[..1])
        })
        .into_owned()
}

static RELATIVE_REQUIRE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // no `(?x)` here, so every character counts: a stray space after the
    // flags would be a space the line has to start with
    compile(&format!(r"(?m)^[^/*\n]*require\((?P<path>{QUOTED})\)"))
});

/// Rewrite `require("./thing")` to the module path it means, and record
/// every module this file requires.
///
/// The dependency list is the second half of what `odoo.define` needs:
/// the client loads a module only once everything it named is loaded, so
/// a missing name here is a module that never runs.
fn convert_relative_require(
    url: &str,
    dependencies: &mut Vec<String>,
    content: &str,
) -> Result<String, RusdooError> {
    let mut out = content.to_string();
    let found: Vec<String> = RELATIVE_REQUIRE_RE
        .captures_iter(content)
        .map(|found| found["path"].to_string())
        .collect();
    for quoted in found {
        let quote = &quoted[..1];
        let path = &quoted[1..quoted.len() - 1];
        let module_path = if path.starts_with('.') && path.contains('/') {
            let resolved = relative_to_module_path(url, path)?;
            out = out.replace(
                &format!("require({quote}{path}{quote})"),
                &format!("require(\"{resolved}\")"),
            );
            resolved
        } else {
            path.to_string()
        };
        if !dependencies.contains(&module_path) {
            dependencies.push(module_path);
        }
    }
    Ok(out)
}

fn relative_to_module_path(url: &str, relative: &str) -> Result<String, RusdooError> {
    let url_parts: Vec<&str> = url.split('/').collect();
    let relative_parts: Vec<&str> = relative.split('/').collect();
    let back = relative_parts.iter().filter(|part| **part == "..").count() + 1;
    if back > url_parts.len() {
        return Err(RusdooError::Validation(format!(
            "{url}: the relative import {relative:?} climbs past the addons root"
        )));
    }
    let mut joined: Vec<&str> = url_parts[..url_parts.len() - back].to_vec();
    joined.extend(
        relative_parts
            .iter()
            .filter(|part| **part != ".." && **part != "."),
    );
    url_to_module_path(&joined.join("/"))
}

// -- translation ----------------------------------------------------------

static GETTEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?xm) ^ \s*const\s*\{ (?:\s*\w*\s*,)* \s*_t\s* (?:,\s*\w*\s*)*,?\s*
           \}\s*=\s*require\("@web/core/l10n/translation"\); $"#,
    )
});

static T_FN_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?xm) ^ \s*const\s*\{ (?:\s*\w*\s*,)* \s*appTranslateFn\s* (?:,\s*\w*\s*)*,?\s*
           \}\s*=\s*require\("@web/core/l10n/translation"\); $"#,
    )
});

/// Bind `_t` to the module it is used in.
///
/// A translated string has to say which addon it came from, or two
/// addons that translate the same English word both get whichever
/// translation loaded last.
fn convert_t(url: &str, content: &str) -> Result<String, RusdooError> {
    if url.ends_with(".test.js") {
        return Ok(content.to_string());
    }
    let module = URL_RE
        .captures(url)
        .ok_or_else(|| RusdooError::Validation(format!("not an asset url: {url:?}")))?["module"]
        .to_string();
    let already_named = T_FN_RE.is_match(content);
    Ok(GETTEXT_RE
        .replace_all(content, |found: &Captures| {
            let whole = &found[0];
            let renamed = if already_named {
                whole.replace("_t", "__not_defined__")
            } else {
                whole.replace("_t", "appTranslateFn")
            };
            format!(
                "{renamed}const _t = (str, ...args) => appTranslateFn(str, \"{module}\", ...args);"
            )
        })
        .into_owned())
}

// -- the little shared pieces ---------------------------------------------

/// `a, b, c as x` rewritten member by member, and put back in braces.
fn rewrite_object(object: &str, each: fn(&str) -> String, join: &str) -> String {
    let inner = &object[1..object.len() - 1];
    format!(
        "{{{}}}",
        inner.split(',').map(each).collect::<Vec<_>>().join(join)
    )
}

/// `c as x` becomes `x: c` — what an object literal spells.
fn convert_as(value: &str) -> String {
    match value.split_once(" as ") {
        Some((from, to)) => format!("{to}: {from}"),
        None => value.to_string(),
    }
}

/// `c as x` becomes `c` — the name on the source's side.
fn remove_as(value: &str) -> String {
    match value.split_once(" as ") {
        Some((from, _)) => from.to_string(),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases are Odoo's own, from
    /// `test_assetsbundle/tests/test_js_transpiler.py`, byte for byte.
    /// Matching its output is the whole requirement: the files being
    /// transformed were written against that behaviour, so being right
    /// where it is wrong would still break them.
    fn check(url: &str, input: &str, expected: &str) {
        let got = transpile(url, input).expect("it transpiles");
        assert_eq!(got, expected, "\n--- got ---\n{got}\n--- want ---\n{expected}");
    }

    #[test]
    fn an_alias_gets_a_second_module_pointing_at_the_first() {
        check(
            "/test_assetsbundle/static/src/alias.js",
            "/** @odoo-module alias=test_assetsbundle.Alias **/",
            // written out rather than continued: Odoo indents the alias
            // body with 24 spaces, and a `\` continuation would eat them
            "odoo.define('@test_assetsbundle/alias', [], function (require) {
'use strict';
let __exports = {};
/** @odoo-module alias=test_assetsbundle.Alias **/
return __exports;
});

odoo.define(`test_assetsbundle.Alias`, ['@test_assetsbundle/alias'], function (require) {
                        return require('@test_assetsbundle/alias')[Symbol.for(\"default\")];
                        });
",
        );
    }

    #[test]
    fn default_false_makes_the_alias_the_whole_module() {
        for annotation in [
            "/** @odoo-module alias=test_assetsbundle.Alias default=False **/",
            "/** @odoo-module alias=test_assetsbundle.Alias default=0 **/",
            "/** @odoo-module alias=test_assetsbundle.Alias default=false **/",
        ] {
            check(
                "/test_assetsbundle/static/src/alias.js",
                annotation,
                &format!(
                    "odoo.define('@test_assetsbundle/alias', [], function (require) {{
'use strict';
let __exports = {{}};
{annotation}
return __exports;
}});

odoo.define(`test_assetsbundle.Alias`, ['@test_assetsbundle/alias'], function (require) {{
                        return require('@test_assetsbundle/alias');
                        }});
"
                ),
            );
        }
    }

    #[test]
    fn exported_classes_keep_their_name_and_gain_a_slot() {
        check(
            "/test_assetsbundle/static/src/classes.js",
            "export default class Nice {}\n\nclass Vehicule {}\n\nexport class Car extends Vehicule {}\n\nexport class Boat extends Vehicule {}\n\nexport const Ferrari = class Ferrari extends Car {};\n",
            "odoo.define('@test_assetsbundle/classes', [], function (require) {\n\
             'use strict';\n\
             let __exports = {};\n\
             const Nice = __exports[Symbol.for(\"default\")] = class Nice {}\n\
             \n\
             class Vehicule {}\n\
             \n\
             const Car = __exports.Car = class Car extends Vehicule {}\n\
             \n\
             const Boat = __exports.Boat = class Boat extends Vehicule {}\n\
             \n\
             const Ferrari = __exports.Ferrari = class Ferrari extends Car {};\n\
             \n\
             return __exports;\n\
             });\n",
        );
    }

    #[test]
    fn a_url_becomes_the_name_the_client_requires() {
        assert_eq!(
            url_to_module_path("/web/static/src/one/two/three.js").unwrap(),
            "@web/one/two/three"
        );
        // a directory with an index is reached by the directory's name
        assert_eq!(
            url_to_module_path("/web/static/src/views/index.js").unwrap(),
            "@web/views"
        );
        // lib and tests are reached through `..`, which is how the
        // client tells the three trees apart
        assert_eq!(
            url_to_module_path("/web/static/lib/owl/owl.js").unwrap(),
            "@web/../lib/owl/owl"
        );
        assert_eq!(
            url_to_module_path("/web/static/tests/helpers.js").unwrap(),
            "@web/../tests/helpers"
        );
        // a file outside the three is refused by name rather than
        // silently given a module path nobody could require
        assert!(url_to_module_path("/web/somewhere/else.js").is_err());
    }

    #[test]
    fn everything_under_static_src_is_a_module_unless_it_opts_out() {
        assert!(is_odoo_module("/web/static/src/thing.js", "const a = 1;"));
        assert!(is_odoo_module("/web/static/tests/thing.js", "const a = 1;"));
        // outside those, only an annotation makes it one
        assert!(!is_odoo_module("/web/static/lib/vendor.js", "const a = 1;"));
        assert!(is_odoo_module(
            "/web/static/lib/vendor.js",
            "/** @odoo-module **/\nconst a = 1;"
        ));
        // and `ignore` opts a file out even where the rule would include it
        assert!(!is_odoo_module(
            "/web/static/src/legacy.js",
            "/** @odoo-module ignore **/\nvar a = 1;"
        ));
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    /// `export function` and a bare `export default`.
    ///
    /// These were written `$space__exports` in the replacement, which
    /// this engine reads as one variable named `space__exports` — no
    /// such group, so it expanded to nothing and the line came out
    /// starting with `.name = ...`. A syntax error at that point takes
    /// down every module after it in the bundle, which is how a browser
    /// found it and the unit tests did not.
    #[test]
    fn an_exported_function_keeps_its_exports_prefix() {
        let out = transpile(
            "/demo/static/src/thing.js",
            "export function greet() {}\nexport default function main() {}\nexport default 42;\n",
        )
        .expect("it transpiles");
        assert!(
            out.contains("__exports.greet = greet; function greet"),
            "the named export kept its prefix: {out}"
        );
        assert!(
            out.contains("__exports[Symbol.for(\"default\")] = main; function main"),
            "so did the default one: {out}"
        );
        assert!(
            out.contains("__exports[Symbol.for(\"default\")] = 42;"),
            "and the bare default: {out}"
        );
        // nothing may come out starting at a dot
        assert!(
            !out.lines().any(|line| line.trim_start().starts_with(".")),
            "a line begins with a dot, which is a syntax error: {out}"
        );
    }
}

/**
 * O editor de um campo: o input que corresponde ao tipo, e como ler dele
 * o valor que vai para o servidor.
 *
 * Formulário e linhas x2many usam o mesmo widget — uma célula de linha e
 * um campo de formulário são a mesma coisa em lugares diferentes.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill, formatValue, parseInput, debounce } = rusdoo.utils;
    const { callKw } = rusdoo.rpc;

    /** Sugestões buscadas por vez num campo relacional. */
    const SUGGESTION_LIMIT = 8;

    /**
     * O valor cru de um campo num registro lido: many2one vira o id,
     * vazio é sempre `false` (a convenção do Odoo), nunca `null` — que
     * apareceria escrito na tela.
     */
    function rawValue(record, name, meta) {
        const raw = record ? record[name] : undefined;
        if (raw === undefined || raw === null) {
            return false;
        }
        if (meta && meta.type === "many2one") {
            return typeof raw === "object" ? raw.id : raw;
        }
        return raw;
    }

    /**
     * Monta o editor de um campo.
     *
     * @returns {{node: Node, read: function(): Promise<*>}} o nó a inserir
     *   e a leitura do valor atual, assíncrona porque um many2one pode
     *   precisar resolver o texto digitado num id.
     */
    function build(meta, record, name, options) {
        const settings = options || {};
        const onChange = settings.onChange || function () {};
        const value = rawValue(record, name, meta);
        if (settings.readonly || meta.readonly) {
            const node = el("div", { class: "o_field o_readonly" }, formatValue(record ? record[name] : false, meta));
            return { node: node, read: async () => undefined };
        }
        switch (meta.type) {
            case "boolean":
                return checkbox(value, onChange);
            case "text":
            case "html":
                return textarea(value, onChange);
            case "selection":
                return selection(meta, value, onChange);
            case "many2one":
                return many2one(meta, record, name, onChange, settings);
            case "date":
                return input({ type: "date", value: value || "" }, meta, onChange);
            case "datetime":
                return input(
                    {
                        type: "datetime-local",
                        value: value ? String(value).replace(" ", "T").slice(0, 16) : "",
                    },
                    meta,
                    onChange
                );
            case "integer":
            case "float":
            case "monetary":
                return input(
                    {
                        type: "number",
                        step: meta.type === "integer" ? "1" : "any",
                        value: value === false ? "" : String(value),
                    },
                    meta,
                    onChange
                );
            default:
                return input(
                    { type: "text", value: value === false ? "" : String(value) },
                    meta,
                    onChange
                );
        }
    }

    function dress(node, meta) {
        node.classList.add("o_input");
        if (meta.required) {
            node.setAttribute("required", "required");
        }
        return node;
    }

    function checkbox(value, onChange) {
        const node = dress(
            el("input", { type: "checkbox", checked: Boolean(value), onchange: onChange }),
            {}
        );
        return { node: node, read: async () => node.checked };
    }

    function textarea(value, onChange) {
        const node = el("textarea", { rows: "3", onchange: onChange });
        node.value = value === false ? "" : String(value);
        dress(node, {});
        return { node: node, read: async () => (node.value === "" ? false : node.value) };
    }

    function selection(meta, value, onChange) {
        const node = dress(
            el(
                "select",
                { onchange: onChange },
                [el("option", { value: "" }, "")].concat(
                    (meta.selection || []).map((pair) =>
                        el("option", { value: pair[0], selected: pair[0] === value }, pair[1])
                    )
                )
            ),
            meta
        );
        return { node: node, read: async () => (node.value === "" ? false : node.value) };
    }

    function input(attrs, meta, onChange) {
        const node = dress(el("input", Object.assign({ onchange: onChange }, attrs)), meta);
        return { node: node, read: async () => parseInput(node.value, meta) };
    }

    /**
     * Many2one: um input com sugestões de `name_search`. O que é gravado
     * é sempre um id escolhido da lista — texto que não casa com registro
     * nenhum vira erro na hora de salvar, não um vínculo inventado.
     */
    function many2one(meta, record, name, onChange, settings) {
        const current = record ? record[name] : null;
        const listId =
            "o_m2o_" + String(meta.relation).replace(/\./g, "_") + "_" + name + "_" + (settings.key || "0");
        const suggestions = el("datalist", { id: listId });
        const node = dress(
            el("input", {
                type: "text",
                list: listId,
                autocomplete: "off",
                value: current && current.display_name ? current.display_name : "",
                onchange: onChange,
                oninput: debounce(async (event) => {
                    try {
                        const pairs = await callKw(meta.relation, "name_search", [], {
                            name: event.target.value,
                            limit: SUGGESTION_LIMIT,
                        });
                        fill(
                            suggestions,
                            pairs.map((pair) => el("option", { value: pair[1] }))
                        );
                    } catch (error) {
                        (settings.onError || function () {})(error);
                    }
                }, 250),
            }),
            meta
        );
        const known = current && current.display_name ? current.display_name : "";
        const knownId = current && current.id ? current.id : false;
        return {
            node: el("span", { class: "o_m2o" }, [node, suggestions]),
            read: async () => {
                const typed = node.value.trim();
                if (!typed) {
                    return false;
                }
                if (typed === known && knownId) {
                    return knownId;
                }
                const pairs = await callKw(meta.relation, "name_search", [], {
                    name: typed,
                    limit: SUGGESTION_LIMIT,
                });
                const exact = pairs.find((pair) => pair[1] === typed);
                if (!exact) {
                    throw new Error(
                        "campo " + (meta.string || name) + ": escolha um registro da lista"
                    );
                }
                return exact[0];
            },
        };
    }

    rusdoo.fieldWidget = { build: build, rawValue: rawValue };
})((window.rusdoo = window.rusdoo || {}));

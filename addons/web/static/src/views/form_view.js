/**
 * A view de formulário: um registro, os campos que o arch declara, e o
 * caminho de escrita do cliente (`web_save`).
 *
 * Só o que mudou vai no salvamento: reenviar o registro inteiro
 * sobrescreveria campos que outra pessoa alterou no meio da edição.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill, fieldLabel, formatValue, parseInput, debounce, parseArch } = rusdoo.utils;
    const { callKw } = rusdoo.rpc;

    /** Sugestões buscadas por vez num campo many2one. */
    const SUGGESTION_LIMIT = 8;

    /** Um campo relacional guarda o id; o resto guarda o valor cru. */
    function valueOf(record, name, meta) {
        const raw = record[name];
        if (meta && meta.type === "many2one") {
            return raw && typeof raw === "object" ? raw.id : raw || false;
        }
        return raw === undefined ? false : raw;
    }

    class FormView {
        constructor(config) {
            this.model = config.model;
            this.fields = config.fields || {};
            this.resId = config.resId || null;
            this.title = config.title || this.model;
            this.onBack = config.onBack || function () {};
            this.onSaved = config.onSaved || function () {};
            this.onError = config.onError || function () {};

            this.archRoot = parseArch(config.arch);
            this.record = {};
            this.inputs = new Map();
            this.dirty = new Set();
            this.root = el("div", { class: "o_form_view" });
        }

        /** Os campos do arch que o modelo realmente tem. */
        fieldNames() {
            return Array.from(this.archRoot.getElementsByTagName("field"))
                .map((node) => node.getAttribute("name"))
                .filter((name) => name && this.fields[name]);
        }

        async load() {
            const names = this.fieldNames();
            if (this.resId) {
                const records = await callKw(this.model, "web_read", [[this.resId]], {
                    specification: rusdoo.specFor(names, this.fields),
                });
                if (!records.length) {
                    throw new Error("registro " + this.resId + " não encontrado");
                }
                this.record = records[0];
            } else {
                // um registro novo começa nos defaults do servidor, que é
                // quem conhece os default_value dos campos
                this.record = await callKw(this.model, "default_get", [names], {});
                this.record.id = null;
            }
            this.dirty.clear();
        }

        /** O input de um campo, ou o texto quando ele é somente leitura. */
        renderField(name, archNode) {
            const meta = this.fields[name];
            const value = valueOf(this.record, name, meta);
            const readonly = meta.readonly || archNode.getAttribute("readonly") === "1";
            if (readonly) {
                return el("div", { class: "o_field o_readonly" }, formatValue(this.record[name], meta));
            }
            const onChange = () => this.dirty.add(name);
            let input;
            switch (meta.type) {
                case "boolean":
                    input = el("input", { type: "checkbox", checked: Boolean(value), onchange: onChange });
                    break;
                case "text":
                case "html":
                    input = el("textarea", { rows: "4", onchange: onChange }, null);
                    input.value = value === false ? "" : String(value);
                    break;
                case "selection":
                    input = el(
                        "select",
                        { onchange: onChange },
                        [el("option", { value: "" }, "")].concat(
                            (meta.selection || []).map((pair) =>
                                el("option", { value: pair[0], selected: pair[0] === value }, pair[1])
                            )
                        )
                    );
                    break;
                case "many2one":
                    input = this.renderMany2one(name, meta, onChange);
                    break;
                case "date":
                    input = el("input", { type: "date", value: value || "", onchange: onChange });
                    break;
                case "datetime":
                    input = el("input", {
                        type: "datetime-local",
                        value: value ? String(value).replace(" ", "T").slice(0, 16) : "",
                        onchange: onChange,
                    });
                    break;
                case "integer":
                case "float":
                case "monetary":
                    input = el("input", {
                        type: "number",
                        step: meta.type === "integer" ? "1" : "any",
                        value: value === false ? "" : String(value),
                        onchange: onChange,
                    });
                    break;
                case "one2many":
                case "many2many":
                    // as linhas embutidas ainda não são editáveis aqui: o
                    // valor é mostrado, não fabricado
                    return el("div", { class: "o_field o_readonly" }, formatValue(this.record[name], meta));
                default:
                    input = el("input", {
                        type: "text",
                        value: value === false ? "" : String(value),
                        onchange: onChange,
                    });
            }
            input.classList.add("o_input");
            if (meta.required) {
                input.setAttribute("required", "required");
            }
            this.inputs.set(name, input);
            // um many2one traz junto a lista de sugestões que o alimenta
            return el("div", { class: "o_field" }, [input, input.suggestions || null]);
        }

        /**
         * Many2one: um input com sugestões vindas de `name_search`. O que
         * é salvo é sempre um id escolhido da lista — um texto que não
         * casa com nenhum registro vira erro no salvamento.
         */
        renderMany2one(name, meta, onChange) {
            const current = this.record[name];
            const listId = "o_m2o_" + this.model.replace(/\./g, "_") + "_" + name;
            const datalist = el("datalist", { id: listId });
            const input = el("input", {
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
                            datalist,
                            pairs.map((pair) => el("option", { value: pair[1], "data-id": pair[0] }))
                        );
                    } catch (error) {
                        this.onError(error);
                    }
                }, 250),
            });
            input.dataset.selectedId = current && current.id ? String(current.id) : "";
            input.dataset.selectedLabel = input.value;
            input.dataset.relation = meta.relation;
            // a lista viaja com o input: quem o renderiza a insere ao lado
            input.suggestions = datalist;
            return input;
        }

        /** O valor a gravar de um campo, ou `undefined` se não mudou. */
        async valueToSave(name) {
            const meta = this.fields[name];
            const input = this.inputs.get(name);
            if (!input || !this.dirty.has(name)) {
                return undefined;
            }
            if (meta.type === "boolean") {
                return input.checked;
            }
            if (meta.type === "many2one") {
                const typed = input.value.trim();
                if (!typed) {
                    return false;
                }
                if (typed === input.dataset.selectedLabel && input.dataset.selectedId) {
                    return Number(input.dataset.selectedId);
                }
                // resolve o texto digitado num id de verdade
                const pairs = await callKw(meta.relation, "name_search", [], {
                    name: typed,
                    limit: SUGGESTION_LIMIT,
                });
                const exact = pairs.find((pair) => pair[1] === typed);
                if (!exact) {
                    throw new Error(
                        "campo " + fieldLabel(name, meta) + ": escolha um registro da lista"
                    );
                }
                return exact[0];
            }
            return parseInput(input.value, meta);
        }

        async save() {
            const values = {};
            for (const name of this.inputs.keys()) {
                const value = await this.valueToSave(name);
                if (value !== undefined) {
                    values[name] = value;
                }
            }
            const names = this.fieldNames();
            const ids = this.resId ? [this.resId] : [];
            const saved = await callKw(this.model, "web_save", [ids, values], {
                specification: rusdoo.specFor(names, this.fields),
            });
            const record = Array.isArray(saved) ? saved[0] : saved;
            this.record = record || this.record;
            this.resId = this.record.id || this.resId;
            this.dirty.clear();
            this.onSaved(this.resId);
        }

        async remove() {
            if (!this.resId || !window.confirm("Excluir este registro?")) {
                return;
            }
            await callKw(this.model, "unlink", [[this.resId]], {});
            this.onBack();
        }

        /** Executa uma ação do formulário reportando o erro na tela. */
        run(action) {
            return async () => {
                try {
                    await action();
                } catch (error) {
                    this.onError(error);
                }
            };
        }

        renderControlPanel() {
            return el("div", { class: "o_control_panel" }, [
                el("h2", { class: "o_breadcrumb" }, [
                    el("a", { href: "#", onclick: (event) => {
                        event.preventDefault();
                        this.onBack();
                    } }, this.title),
                    " / ",
                    this.resId ? "#" + this.resId : "Novo",
                ]),
                el("div", { class: "o_cp_actions" }, [
                    el(
                        "button",
                        { class: "btn btn-primary", onclick: this.run(() => this.save()) },
                        "Salvar"
                    ),
                    el(
                        "button",
                        {
                            class: "btn btn-ghost",
                            onclick: this.run(async () => {
                                await this.load();
                                this.render();
                            }),
                        },
                        "Descartar"
                    ),
                    this.resId
                        ? el(
                              "button",
                              { class: "btn btn-danger", onclick: this.run(() => this.remove()) },
                              "Excluir"
                          )
                        : null,
                ]),
            ]);
        }

        /** Percorre o arch preservando os `<group>` como colunas. */
        renderSheet() {
            const groups = Array.from(this.archRoot.getElementsByTagName("group"));
            const containers = groups.length ? groups : [this.archRoot];
            const rendered = containers.map((container) => {
                const rows = Array.from(container.getElementsByTagName("field"))
                    .filter((node) => node.getAttribute("name") && this.fields[node.getAttribute("name")])
                    .map((node) => {
                        const name = node.getAttribute("name");
                        const meta = this.fields[name];
                        return el("div", { class: "o_form_row" }, [
                            el(
                                "label",
                                { class: meta.required ? "o_required" : null },
                                fieldLabel(name, meta, node.getAttribute("string"))
                            ),
                            this.renderField(name, node),
                        ]);
                    });
                return el("div", { class: "o_group" }, rows);
            });
            return el("div", { class: "o_form_sheet" }, rendered);
        }

        render() {
            this.inputs.clear();
            fill(this.root, [this.renderControlPanel(), this.renderSheet()]);
            return this.root;
        }
    }

    rusdoo.FormView = FormView;
})((window.rusdoo = window.rusdoo || {}));

/**
 * A view de lista: as colunas que o arch declara, com busca, ordenação e
 * paginação — tudo resolvido no servidor, que é quem aplica ACL, regras
 * de registro e o domínio da ação.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill, fieldLabel, formatValue, debounce, parseArch, archFields } = rusdoo.utils;
    const { callKw } = rusdoo.rpc;

    /** Registros por página, como o padrão do Odoo. */
    const PAGE_SIZE = 80;

    /** Tipos em que uma busca por texto (`ilike`) faz sentido. */
    const SEARCHABLE = ["char", "text", "html"];

    /**
     * A specification de leitura de uma lista de campos: relacionais
     * pedem o `display_name`, o resto vem cru.
     */
    function specFor(names, fields) {
        const spec = {};
        for (const name of names) {
            const meta = fields[name];
            if (meta && (meta.type === "many2one" || meta.type === "one2many" || meta.type === "many2many")) {
                spec[name] = { fields: { display_name: {} } };
            } else {
                spec[name] = {};
            }
        }
        return spec;
    }

    class ListView {
        /**
         * @param {object} config model, arch, fields (fields_get), domain
         *   da ação, title, e os callbacks de abrir/criar registro.
         */
        constructor(config) {
            this.model = config.model;
            this.fields = config.fields || {};
            this.actionDomain = config.domain || [];
            this.title = config.title || this.model;
            this.onOpen = config.onOpen || function () {};
            this.onCreate = config.onCreate || null;
            this.onError = config.onError || function () {};

            const root = parseArch(config.arch);
            this.columns = archFields(root)
                .filter((field) => field.name && this.fields[field.name])
                .map((field) => ({
                    name: field.name,
                    label: fieldLabel(field.name, this.fields[field.name], field.label),
                    meta: this.fields[field.name],
                }));

            this.offset = 0;
            this.order = null;
            this.query = "";
            this.records = [];
            this.length = 0;
            this.root = el("div", { class: "o_list_view" });
        }

        /** O domínio efetivo: o da ação, mais o que foi digitado na busca. */
        domain() {
            const searchable = this.columns.filter((column) =>
                SEARCHABLE.includes(column.meta.type)
            );
            if (!this.query || searchable.length === 0) {
                return this.actionDomain;
            }
            // OR entre as colunas textuais: em domínio prefixado, n termos
            // pedem n-1 operadores '|' antes deles
            const terms = searchable.map((column) => [column.name, "ilike", this.query]);
            const or = new Array(terms.length - 1).fill("|");
            return this.actionDomain.concat(or, terms);
        }

        async load() {
            const names = this.columns.map((column) => column.name);
            const result = await callKw(this.model, "web_search_read", [], {
                domain: this.domain(),
                specification: specFor(names, this.fields),
                limit: PAGE_SIZE,
                offset: this.offset,
                order: this.order || undefined,
            });
            this.records = result.records || [];
            this.length = result.length || 0;
        }

        /** Recarrega e redesenha, reportando o erro em vez de engoli-lo. */
        async refresh() {
            try {
                await this.load();
                this.render();
            } catch (error) {
                this.onError(error);
            }
        }

        /** Ordena por uma coluna, invertendo se já era a ordenada. */
        sortBy(name) {
            const current = this.order || "";
            this.order = current === name + " asc" ? name + " desc" : name + " asc";
            this.offset = 0;
            this.refresh();
        }

        page(delta) {
            const next = this.offset + delta * PAGE_SIZE;
            if (next < 0 || next >= this.length) {
                return;
            }
            this.offset = next;
            this.refresh();
        }

        renderControlPanel() {
            const search = el("input", {
                type: "search",
                class: "o_searchview",
                placeholder: "Buscar…",
                value: this.query,
                oninput: debounce((event) => {
                    this.query = event.target.value.trim();
                    this.offset = 0;
                    this.refresh();
                }, 250),
            });
            const first = this.length === 0 ? 0 : this.offset + 1;
            const last = Math.min(this.offset + this.records.length, this.length);
            return el("div", { class: "o_control_panel" }, [
                el("h2", { class: "o_breadcrumb" }, this.title),
                el("div", { class: "o_cp_actions" }, [
                    this.onCreate
                        ? el(
                              "button",
                              { class: "btn btn-primary", onclick: () => this.onCreate() },
                              "Novo"
                          )
                        : null,
                    search,
                    el("span", { class: "o_pager" }, first + "-" + last + " / " + this.length),
                    el(
                        "button",
                        {
                            class: "btn btn-ghost",
                            disabled: this.offset === 0,
                            onclick: () => this.page(-1),
                        },
                        "‹"
                    ),
                    el(
                        "button",
                        {
                            class: "btn btn-ghost",
                            disabled: this.offset + this.records.length >= this.length,
                            onclick: () => this.page(1),
                        },
                        "›"
                    ),
                ]),
            ]);
        }

        renderTable() {
            const header = el(
                "tr",
                {},
                this.columns.map((column) =>
                    el(
                        "th",
                        {
                            class: this.order && this.order.startsWith(column.name + " ")
                                ? "o_sorted"
                                : null,
                            onclick: () => this.sortBy(column.name),
                        },
                        column.label
                    )
                )
            );
            const rows = this.records.map((record) =>
                el(
                    "tr",
                    { class: "o_data_row", onclick: () => this.onOpen(record.id) },
                    this.columns.map((column) =>
                        el("td", {}, formatValue(record[column.name], column.meta))
                    )
                )
            );
            if (rows.length === 0) {
                rows.push(
                    el("tr", {}, [
                        el(
                            "td",
                            { class: "o_nocontent", colspan: String(this.columns.length || 1) },
                            "Nenhum registro."
                        ),
                    ])
                );
            }
            return el("table", { class: "o_list_table" }, [
                el("thead", {}, header),
                el("tbody", {}, rows),
            ]);
        }

        render() {
            fill(this.root, [this.renderControlPanel(), this.renderTable()]);
            return this.root;
        }
    }

    rusdoo.ListView = ListView;
    rusdoo.specFor = specFor;
})((window.rusdoo = window.rusdoo || {}));

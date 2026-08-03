/** @odoo-module ignore **/
// Não é um módulo ES6: este cliente é IIFE e se instala em
// `window.rusdoo` ao carregar. Envolvê-lo num `odoo.define` faria
// o corpo só rodar quando alguém o requisitasse, e ninguém requisita.
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
     * Os filtros de uma view de busca: `<filter name string domain/>`.
     * O domínio vem em JSON — é o que o servidor aceita, e o que evita
     * ter de interpretar Python no navegador.
     */
    function filtersOf(arch) {
        if (!arch) {
            return { filters: [], fields: [], groupbys: [] };
        }
        let root;
        try {
            root = rusdoo.utils.parseArch(arch);
        } catch (error) {
            return { filters: [], fields: [], groupbys: [] };
        }
        const groupbys = Array.from(root.getElementsByTagName("groupby"))
            .map((node) => ({
                name: node.getAttribute("name"),
                label: node.getAttribute("string") || node.getAttribute("name"),
            }))
            .filter((entry) => entry.name);
        const filters = Array.from(root.getElementsByTagName("filter"))
            .map((node) => {
                let domain = [];
                try {
                    domain = JSON.parse(node.getAttribute("domain") || "[]");
                } catch (error) {
                    // um filtro cujo domínio não dá para ler não vira um
                    // filtro que não filtra nada: ele fica de fora
                    return null;
                }
                return {
                    name: node.getAttribute("name") || "",
                    label: node.getAttribute("string") || node.getAttribute("name") || "",
                    domain: domain,
                };
            })
            .filter(Boolean);
        const fields = Array.from(root.getElementsByTagName("field"))
            .map((node) => node.getAttribute("name"))
            .filter(Boolean);
        return { filters: filters, fields: fields, groupbys: groupbys };
    }

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
            this.viewTypes = config.viewTypes || [];
            this.onSwitch = config.onSwitch || function () {};

            const root = parseArch(config.arch);
            this.columns = archFields(root)
                .filter((field) => field.name && this.fields[field.name])
                .map((field) => ({
                    name: field.name,
                    label: fieldLabel(field.name, this.fields[field.name], field.label),
                    meta: this.fields[field.name],
                }));

            const search = filtersOf(config.searchArch);
            this.filters = search.filters;
            // os campos que a busca por texto percorre: os da view de
            // busca quando existe, senão as colunas textuais da lista
            this.searchFields = search.fields.filter((name) => this.fields[name]);
            this.active = new Set();
            this.groupbys = (search.groupbys || []).filter((entry) => this.fields[entry.name]);
            // um agrupamento por vez: o servidor aceita vários, mas uma
            // lista com dois níveis abertos é mais árvore do que lista
            this.groupBy = null;
            this.groups = [];
            this.opened = new Map();

            this.offset = 0;
            this.order = null;
            this.query = "";
            this.records = [];
            this.length = 0;
            this.root = el("div", { class: "o_list_view" });
        }

        /**
         * O domínio efetivo: o da ação, mais os filtros ligados, mais o
         * que foi digitado. Os três são combinados com E — um filtro
         * restringe o que já estava restrito.
         */
        domain() {
            let domain = this.actionDomain.slice();
            for (const filter of this.filters) {
                if (this.active.has(filter.name)) {
                    domain = domain.concat(filter.domain);
                }
            }
            const names = this.searchFields.length
                ? this.searchFields
                : this.columns
                      .filter((column) => SEARCHABLE.includes(column.meta.type))
                      .map((column) => column.name);
            if (!this.query || names.length === 0) {
                return domain;
            }
            // OU entre os campos de texto: em domínio prefixado, n termos
            // pedem n-1 operadores '|' antes deles
            const terms = names.map((name) => [name, "ilike", this.query]);
            const or = new Array(terms.length - 1).fill("|");
            return domain.concat(or, terms);
        }

        setGroupBy(name) {
            this.groupBy = this.groupBy === name ? null : name;
            this.offset = 0;
            this.refresh();
        }

        toggleFilter(name) {
            if (this.active.has(name)) {
                this.active.delete(name);
            } else {
                this.active.add(name);
            }
            this.offset = 0;
            this.refresh();
        }

        /**
         * A barra de busca com facetas.
         *
         * Cada coisa que restringe a lista — o texto digitado, um filtro
         * ligado, um agrupamento — vira uma pastilha dentro da barra,
         * com um × que a remove. É a diferença entre ver *o que* está
         * filtrando e ter que adivinhar por que a lista está curta.
         */
        renderSearchBar() {
            const facets = [];
            if (this.query) {
                facets.push(
                    this.facet("Buscar", this.query, () => {
                        this.query = "";
                        this.offset = 0;
                        this.refresh();
                    })
                );
            }
            for (const filter of this.filters) {
                if (!this.active.has(filter.name)) {
                    continue;
                }
                facets.push(
                    this.facet("Filtro", filter.label, () => this.toggleFilter(filter.name))
                );
            }
            if (this.groupBy) {
                const entry = this.groupbys.find((g) => g.name === this.groupBy);
                facets.push(
                    this.facet("Agrupar", entry ? entry.label : this.groupBy, () =>
                        this.setGroupBy(this.groupBy)
                    )
                );
            }
            const input = el("input", {
                type: "text",
                class: "o_searchview_input",
                placeholder: facets.length ? "" : "Buscar…",
                value: this.query,
                oninput: debounce((event) => {
                    this.query = event.target.value.trim();
                    this.offset = 0;
                    this.refresh();
                }, 250),
            });
            return el("div", { class: "o_searchview" }, [
                el("div", { class: "o_searchview_facets" }, facets),
                input,
                this.renderSearchMenu(),
            ]);
        }

        /** Uma pastilha: o que ela é, o que ela vale, e como sair dela. */
        facet(kind, label, remove) {
            return el("div", { class: "o_facet" }, [
                el("span", { class: "o_facet_kind" }, kind),
                el("span", { class: "o_facet_value" }, String(label)),
                el(
                    "button",
                    { type: "button", class: "o_facet_remove", title: "Remover", onclick: remove },
                    "×"
                ),
            ]);
        }

        /** O menu de filtros e agrupamentos da view de busca. */
        renderSearchMenu() {
            if (!this.filters.length && !this.groupbys.length) {
                return null;
            }
            const items = [];
            if (this.filters.length) {
                items.push(el("div", { class: "o_search_section" }, "Filtros"));
                for (const filter of this.filters) {
                    items.push(
                        el(
                            "button",
                            {
                                type: "button",
                                class:
                                    "o_search_option" +
                                    (this.active.has(filter.name) ? " o_search_option_on" : ""),
                                onclick: () => this.toggleFilter(filter.name),
                            },
                            filter.label
                        )
                    );
                }
            }
            if (this.groupbys.length) {
                items.push(el("div", { class: "o_search_section" }, "Agrupar por"));
                for (const entry of this.groupbys) {
                    items.push(
                        el(
                            "button",
                            {
                                type: "button",
                                class:
                                    "o_search_option" +
                                    (this.groupBy === entry.name ? " o_search_option_on" : ""),
                                onclick: () => this.setGroupBy(entry.name),
                            },
                            entry.label
                        )
                    );
                }
            }
            const menu = el("div", { class: "o_search_menu" }, items);
            const toggle = el(
                "button",
                { type: "button", class: "o_search_toggle", title: "Filtros" },
                "▾"
            );
            const wrap = el("div", { class: "o_search_dropdown" }, [toggle, menu]);
            toggle.addEventListener("click", (event) => {
                event.stopPropagation();
                wrap.classList.toggle("o_open");
            });
            // um clique fora fecha: um menu que fica aberto atrapalha a
            // própria lista que ele acabou de filtrar
            document.addEventListener(
                "click",
                () => wrap.classList.remove("o_open"),
                { once: true }
            );
            menu.addEventListener("click", (event) => event.stopPropagation());
            return wrap;
        }

        /** O rótulo de um grupo: um many2one vem como [id, nome]. */
        groupLabel(group) {
            const meta = this.fields[this.groupBy] || {};
            const value = group[this.groupBy];
            if (Array.isArray(value)) {
                return value[1] !== undefined ? String(value[1]) : "Não definido";
            }
            if (value === false || value === null || value === undefined) {
                return "Não definido";
            }
            if (meta.type === "selection") {
                const option = (meta.selection || []).find((pair) => pair[0] === value);
                return option ? option[1] : String(value);
            }
            return formatValue(value, meta);
        }

        /** Uma lista agrupada: um cabeçalho por grupo, aberto sob demanda. */
        renderGroups() {
            const rows = [];
            this.groups.forEach((group, index) => {
                const count = group.__count || 0;
                const sums = this.sumColumns()
                    .map((name) => group[name + ":sum"])
                    .filter((value) => value !== undefined && value !== null && value !== false)
                    .map((value) => formatValue(value, { type: "float" }));
                rows.push(
                    el(
                        "tr",
                        {
                            class: "o_group_row",
                            onclick: () =>
                                this.toggleGroup(index).catch((error) => this.onError(error)),
                        },
                        [
                            el("td", { colspan: String(Math.max(this.columns.length, 1)) }, [
                                el("span", { class: "o_group_caret" }, this.opened.has(index) ? "▾" : "▸"),
                                this.groupLabel(group),
                                el("span", { class: "o_group_count" }, "(" + count + ")"),
                                sums.length
                                    ? el("span", { class: "o_group_sum" }, sums.join(" · "))
                                    : null,
                            ]),
                        ]
                    )
                );
                for (const record of this.opened.get(index) || []) {
                    rows.push(
                        el(
                            "tr",
                            { class: "o_data_row", onclick: () => this.onOpen(record.id) },
                            this.columns.map((column) =>
                                el("td", {}, formatValue(record[column.name], column.meta))
                            )
                        )
                    );
                }
            });
            if (!rows.length) {
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
                el(
                    "thead",
                    {},
                    el(
                        "tr",
                        {},
                        this.columns.map((column) => el("th", {}, column.label))
                    )
                ),
                el("tbody", {}, rows),
            ]);
        }

        /** As colunas numéricas que um grupo soma. */
        sumColumns() {
            return this.columns
                .filter((column) =>
                    ["integer", "float", "monetary"].includes(column.meta.type)
                )
                .map((column) => column.name);
        }

        /** Os grupos do domínio atual, com contagem e somas. */
        async loadGroups() {
            const aggregates = this.sumColumns().map((name) => name + ":sum");
            const result = await callKw(this.model, "web_read_group", [], {
                domain: this.domain(),
                groupby: [this.groupBy],
                aggregates: aggregates,
            });
            this.groups = result.groups || [];
            this.length = result.length || this.groups.length;
        }

        /** Abre (ou fecha) um grupo, lendo os registros dele. */
        async toggleGroup(index) {
            if (this.opened.has(index)) {
                this.opened.delete(index);
                this.render();
                return;
            }
            const group = this.groups[index];
            const extra = group.__extra_domain || [];
            const names = this.columns.map((column) => column.name);
            const result = await callKw(this.model, "web_search_read", [], {
                domain: this.domain().concat(extra),
                specification: specFor(names, this.fields),
                limit: PAGE_SIZE,
                order: this.order || undefined,
            });
            this.opened.set(index, result.records || []);
            this.render();
        }

        async load() {
            if (this.groupBy) {
                this.opened.clear();
                await this.loadGroups();
                return;
            }
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
                    this.renderSearchBar(),
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
                    rusdoo.viewSwitcher(this.viewTypes, "list", this.onSwitch),
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
            fill(this.root, [
                this.renderControlPanel(),
                this.groupBy ? this.renderGroups() : this.renderTable(),
            ]);
            return this.root;
        }
    }

    rusdoo.ListView = ListView;
    rusdoo.specFor = specFor;
})((window.rusdoo = window.rusdoo || {}));

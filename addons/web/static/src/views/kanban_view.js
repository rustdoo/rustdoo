/**
 * A view kanban: o quadro. Uma coluna por grupo, um cartão por registro.
 *
 * O agrupamento é o mesmo `web_read_group` da lista, e cada coluna é
 * preenchida com o `__extra_domain` que o servidor devolveu — o cliente
 * nunca remonta o domínio de um grupo, que é como um quadro acaba
 * mostrando o cartão na coluna errada.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill, fieldLabel, formatValue, debounce, parseArch } = rusdoo.utils;
    const { callKw } = rusdoo.rpc;

    /** Cartões carregados por coluna. Um quadro não é uma listagem. */
    const COLUMN_LIMIT = 20;

    /** Sem agrupamento, quantos cartões o quadro mostra de uma vez. */
    const FLAT_LIMIT = 40;

    class KanbanView {
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
            this.cardFields = Array.from(root.getElementsByTagName("field"))
                .map((node) => ({
                    name: node.getAttribute("name"),
                    label: node.getAttribute("string"),
                }))
                .filter((entry) => entry.name && this.fields[entry.name]);
            // o arch diz por onde o quadro abre; sem isso ele é uma
            // parede única de cartões, que ainda é um quadro
            this.groupBy = root.getAttribute("default_group_by") || null;
            if (this.groupBy && !this.fields[this.groupBy]) {
                this.groupBy = null;
            }

            this.query = "";
            this.columns = [];
            this.records = [];
            this.root = el("div", { class: "o_kanban_view" });
        }

        /** Os campos textuais que a busca do quadro percorre. */
        domain() {
            const names = this.cardFields
                .filter((entry) => ["char", "text"].includes(this.fields[entry.name].type))
                .map((entry) => entry.name);
            if (!this.query || !names.length) {
                return this.actionDomain;
            }
            const terms = names.map((name) => [name, "ilike", this.query]);
            return this.actionDomain.concat(new Array(terms.length - 1).fill("|"), terms);
        }

        specification() {
            return rusdoo.specFor(
                this.cardFields.map((entry) => entry.name),
                this.fields
            );
        }

        async load() {
            const domain = this.domain();
            if (!this.groupBy) {
                const result = await callKw(this.model, "web_search_read", [], {
                    domain: domain,
                    specification: this.specification(),
                    limit: FLAT_LIMIT,
                });
                this.columns = [];
                this.records = result.records || [];
                return;
            }
            const grouped = await callKw(this.model, "web_read_group", [], {
                domain: domain,
                groupby: [this.groupBy],
                aggregates: [],
            });
            const groups = grouped.groups || [];
            this.columns = [];
            for (const group of groups) {
                const page = await callKw(this.model, "web_search_read", [], {
                    domain: domain.concat(group.__extra_domain || []),
                    specification: this.specification(),
                    limit: COLUMN_LIMIT,
                });
                this.columns.push({
                    label: this.groupLabel(group),
                    count: group.__count || 0,
                    records: page.records || [],
                });
            }
        }

        async refresh() {
            try {
                await this.load();
                this.render();
            } catch (error) {
                this.onError(error);
            }
        }

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

        renderCard(record) {
            const [title, ...rest] = this.cardFields;
            return el(
                "div",
                { class: "o_kanban_card", onclick: () => this.onOpen(record.id) },
                [
                    title
                        ? el(
                              "div",
                              { class: "o_kanban_title" },
                              formatValue(record[title.name], this.fields[title.name])
                          )
                        : null,
                ].concat(
                    // um campo vazio não vira uma linha com um rótulo e
                    // nada depois: o cartão mostra o que o registro tem
                    rest
                        .map((entry) => ({
                            entry: entry,
                            text: formatValue(record[entry.name], this.fields[entry.name]),
                        }))
                        .filter((cell) => cell.text !== "")
                        .map((cell) =>
                            el("div", { class: "o_kanban_field" }, [
                                el(
                                    "span",
                                    { class: "o_kanban_label" },
                                    fieldLabel(
                                        cell.entry.name,
                                        this.fields[cell.entry.name],
                                        cell.entry.label
                                    )
                                ),
                                cell.text,
                            ])
                        )
                )
            );
        }

        renderControlPanel() {
            const search = el("input", {
                type: "search",
                class: "o_searchview",
                placeholder: "Buscar…",
                value: this.query,
                oninput: debounce((event) => {
                    this.query = event.target.value.trim();
                    this.refresh();
                }, 250),
            });
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
                    rusdoo.viewSwitcher(this.viewTypes, "kanban", this.onSwitch),
                ]),
            ]);
        }

        render() {
            const board = this.groupBy
                ? el(
                      "div",
                      { class: "o_kanban_board" },
                      this.columns.map((column) =>
                          el("div", { class: "o_kanban_column" }, [
                              el("div", { class: "o_kanban_column_head" }, [
                                  column.label,
                                  el("span", { class: "o_group_count" }, "(" + column.count + ")"),
                              ]),
                              el(
                                  "div",
                                  { class: "o_kanban_cards" },
                                  column.records.map((record) => this.renderCard(record))
                              ),
                          ])
                      )
                  )
                : el(
                      "div",
                      { class: "o_kanban_cards o_kanban_flat" },
                      this.records.length
                          ? this.records.map((record) => this.renderCard(record))
                          : el("div", { class: "o_nocontent" }, "Nenhum registro.")
                  );
            fill(this.root, [this.renderControlPanel(), board]);
            return this.root;
        }
    }

    /**
     * Os botões que trocam de view. Uma view só não precisa de botão:
     * um seletor de uma opção é decoração.
     */
    function viewSwitcher(types, current, onSwitch) {
        if (!types || types.length < 2) {
            return null;
        }
        const labels = { list: "Lista", kanban: "Quadro", form: "Formulário" };
        return el(
            "span",
            { class: "o_view_switcher" },
            types
                .filter((type) => type !== "form")
                .map((type) =>
                    el(
                        "button",
                        {
                            class: type === current ? "btn btn-ghost o_active" : "btn btn-ghost",
                            type: "button",
                            onclick: () => onSwitch(type),
                        },
                        labels[type] || type
                    )
                )
        );
    }

    rusdoo.KanbanView = KanbanView;
    rusdoo.viewSwitcher = viewSwitcher;
})((window.rusdoo = window.rusdoo || {}));

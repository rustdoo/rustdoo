/** @odoo-module ignore **/
// Não é um módulo ES6: este cliente é IIFE e se instala em
// `window.rusdoo` ao carregar. Envolvê-lo num `odoo.define` faria
// o corpo só rodar quando alguém o requisitasse, e ninguém requisita.
/**
 * As linhas de um campo one2many/many2many dentro de um formulário: a
 * tabela editável que faz um pedido ser um pedido.
 *
 * O que sai daqui são os comandos que o ORM entende — `[0, 0, valores]`
 * para uma linha nova, `[1, id, valores]` para uma alterada, `[2, id, 0]`
 * para uma removida. Só as linhas tocadas viram comando: reenviar todas
 * sobrescreveria o que outra pessoa mudou no meio da edição.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill, fieldLabel } = rusdoo.utils;

    /** Comandos x2many do Odoo (`odoo/orm/commands.py`). */
    const CREATE = 0;
    const UPDATE = 1;
    const DELETE = 2;

    /** Quantas linhas o formulário lê de uma vez. */
    const LINE_LIMIT = 100;

    /** Os `<field>` diretamente dentro do `<list>` embutido no campo. */
    function columnsOf(archNode) {
        const sub = archNode.getElementsByTagName("list")[0] || archNode.getElementsByTagName("tree")[0];
        if (!sub) {
            return [];
        }
        return Array.from(sub.getElementsByTagName("field"))
            .map((node) => ({
                name: node.getAttribute("name"),
                label: node.getAttribute("string"),
                readonly: node.getAttribute("readonly") === "1",
            }))
            .filter((column) => column.name);
    }

    /**
     * A specification de leitura das linhas: uma coluna relacional pede
     * o `display_name`, senão a célula mostraria um id — ou nada.
     */
    function specOf(archNode, comodelFields) {
        const fields = {};
        for (const column of columnsOf(archNode)) {
            const meta = (comodelFields || {})[column.name];
            fields[column.name] =
                meta && (meta.type === "many2one" || meta.type === "one2many" || meta.type === "many2many")
                    ? { fields: { display_name: {} } }
                    : {};
        }
        return { fields: fields, limit: LINE_LIMIT };
    }

    class X2ManyField {
        /**
         * @param {object} config name, meta (fields_get do campo),
         *   archNode (o `<field>` do formulário), records (as linhas já
         *   lidas) e onError.
         */
        constructor(config) {
            this.name = config.name;
            this.meta = config.meta;
            this.comodel = config.meta.relation;
            this.columns = columnsOf(config.archNode);
            this.onError = config.onError || function () {};
            // linha existente -> {record, widgets}; linhas novas ficam com
            // id null e são criadas no salvamento
            this.rows = (config.records || []).map((record) => ({ record: record, isNew: false }));
            this.removed = [];
            this.comodelFields = config.comodelFields || {};
            if (!this.columns.length) {
                // sem sub-view no arch: mostra o nome do registro
                this.columns = [{ name: "display_name" }];
            }
            this.root = el("div", { class: "o_x2many" });
        }

        addRow() {
            this.rows.push({ record: {}, isNew: true });
            this.render();
        }

        removeRow(index) {
            const row = this.rows[index];
            if (!row.isNew && row.record.id) {
                this.removed.push(row.record.id);
            }
            this.rows.splice(index, 1);
            this.render();
        }

        /** Os comandos a enviar, ou `undefined` se nada mudou. */
        async read() {
            const commands = [];
            for (const row of this.rows) {
                const values = {};
                let touched = row.isNew;
                for (const [name, widget] of row.widgets || []) {
                    if (!row.dirty || !row.dirty.has(name)) {
                        continue;
                    }
                    const value = await widget.read();
                    if (value !== undefined) {
                        values[name] = value;
                        touched = true;
                    }
                }
                if (!touched) {
                    continue;
                }
                commands.push(
                    row.isNew ? [CREATE, 0, values] : [UPDATE, row.record.id, values]
                );
            }
            for (const id of this.removed) {
                commands.push([DELETE, id, 0]);
            }
            return commands.length ? commands : undefined;
        }

        renderRow(row, index) {
            row.widgets = new Map();
            row.dirty = row.dirty || new Set();
            const cells = this.columns.map((column) => {
                const meta = (this.comodelFields || {})[column.name];
                if (!meta) {
                    return el("td", {}, "");
                }
                const widget = rusdoo.fieldWidget.build(meta, row.record, column.name, {
                    readonly: column.readonly,
                    key: this.name + "_" + index,
                    onChange: () => row.dirty.add(column.name),
                    onError: this.onError,
                });
                row.widgets.set(column.name, widget);
                return el("td", {}, widget.node);
            });
            cells.push(
                el("td", { class: "o_x2many_trash" }, [
                    el(
                        "button",
                        {
                            class: "btn btn-ghost",
                            type: "button",
                            title: "Remover linha",
                            onclick: () => this.removeRow(index),
                        },
                        "×"
                    ),
                ])
            );
            return el("tr", {}, cells);
        }

        render() {
            const header = el(
                "tr",
                {},
                this.columns
                    .map((column) =>
                        el(
                            "th",
                            {},
                            fieldLabel(
                                column.name,
                                (this.comodelFields || {})[column.name],
                                column.label
                            )
                        )
                    )
                    .concat(el("th", {}, ""))
            );
            const body = this.rows.map((row, index) => this.renderRow(row, index));
            fill(this.root, [
                el("table", { class: "o_list_table o_x2many_table" }, [
                    el("thead", {}, header),
                    el("tbody", {}, body),
                ]),
                el(
                    "button",
                    {
                        class: "btn btn-ghost o_x2many_add",
                        type: "button",
                        onclick: () => this.addRow(),
                    },
                    "Adicionar linha"
                ),
            ]);
            return this.root;
        }
    }

    rusdoo.X2ManyField = X2ManyField;
    rusdoo.x2manySpec = specOf;
})((window.rusdoo = window.rusdoo || {}));

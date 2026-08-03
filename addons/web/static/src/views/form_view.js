/**
 * A view de formulário: um registro, os campos que o arch declara, e o
 * caminho de escrita do cliente (`web_save`).
 *
 * Só o que mudou vai no salvamento: reenviar o registro inteiro
 * sobrescreveria campos que outra pessoa alterou no meio da edição.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill, fieldLabel, parseArch } = rusdoo.utils;
    const { callKw } = rusdoo.rpc;

    /** Um `<field>` que está dentro de outro é coluna de linha x2many,
     *  não campo do formulário. */
    function isTopLevel(node) {
        for (let parent = node.parentNode; parent; parent = parent.parentNode) {
            if (parent.tagName === "field") {
                return false;
            }
        }
        return true;
    }

    class FormView {
        constructor(config) {
            this.model = config.model;
            this.fields = config.fields || {};
            this.resId = config.resId || null;
            this.title = config.title || this.model;
            this.onBack = config.onBack || function () {};
            this.onSaved = config.onSaved || function () {};
            // um método pode devolver uma ação ("abra esta fatura"); quem
            // sabe navegar é o cliente, não o formulário
            this.onAction = config.onAction || function () {};
            this.onError = config.onError || function () {};

            this.archRoot = parseArch(config.arch);
            this.record = {};
            this.widgets = new Map();
            this.x2many = new Map();
            // campo x2many -> fields_get do comodelo, pedido uma vez: a
            // view principal só traz os metadados do próprio modelo
            this.lineFields = new Map();
            this.dirty = new Set();
            // <chatter/> no arch: o registro carrega uma discussão
            this.chatter = this.archRoot.getElementsByTagName("chatter").length
                ? new rusdoo.Chatter({
                      model: this.model,
                      resId: this.resId,
                      onError: this.onError,
                  })
                : null;
            this.root = el("div", { class: "o_form_view" });
        }

        /** Os `<field>` do formulário (sem as colunas das linhas). */
        fieldNodes() {
            return Array.from(this.archRoot.getElementsByTagName("field"))
                .filter(isTopLevel)
                .filter((node) => {
                    const name = node.getAttribute("name");
                    return name && this.fields[name];
                });
        }

        /** A specification de leitura: relacionais pedem o que a view mostra. */
        specification() {
            const spec = {};
            for (const node of this.fieldNodes()) {
                const name = node.getAttribute("name");
                const meta = this.fields[name];
                if (meta.type === "one2many" || meta.type === "many2many") {
                    spec[name] = rusdoo.x2manySpec(node, this.lineFields.get(name));
                } else if (meta.type === "many2one") {
                    spec[name] = { fields: { display_name: {} } };
                } else {
                    spec[name] = {};
                }
            }
            return spec;
        }

        /** Os metadados dos comodelos das linhas, antes da primeira leitura:
         *  a specification depende deles. */
        async loadLineFields() {
            for (const node of this.fieldNodes()) {
                const name = node.getAttribute("name");
                const meta = this.fields[name];
                if (meta.type !== "one2many" && meta.type !== "many2many") {
                    continue;
                }
                if (!this.lineFields.has(name)) {
                    this.lineFields.set(name, await callKw(meta.relation, "fields_get", [], {}));
                }
            }
        }

        async load() {
            await this.loadLineFields();
            if (this.chatter) {
                this.chatter.resId = this.resId;
                // o histórico é do servidor: um erro nele não impede o
                // formulário de abrir
                await this.chatter.load().catch((error) => this.onError(error));
            }
            const names = this.fieldNodes().map((node) => node.getAttribute("name"));
            if (this.resId) {
                const records = await callKw(this.model, "web_read", [[this.resId]], {
                    specification: this.specification(),
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

        /** O editor de um campo: widget simples, ou a tabela de linhas. */
        renderField(name, archNode) {
            const meta = this.fields[name];
            if (meta.type === "one2many" || meta.type === "many2many") {
                return this.renderLines(name, meta, archNode);
            }
            const widget = rusdoo.fieldWidget.build(meta, this.record, name, {
                readonly: archNode.getAttribute("readonly") === "1",
                key: this.resId || "new",
                onChange: () => this.dirty.add(name),
                onError: this.onError,
            });
            this.widgets.set(name, widget);
            return el("div", { class: "o_field" }, widget.node);
        }

        renderLines(name, meta, archNode) {
            const lines = new rusdoo.X2ManyField({
                name: name,
                meta: meta,
                archNode: archNode,
                comodelFields: this.lineFields.get(name),
                records: Array.isArray(this.record[name]) ? this.record[name] : [],
                onError: this.onError,
            });
            this.x2many.set(name, lines);
            lines.render();
            return el("div", { class: "o_field o_field_lines" }, lines.root);
        }

        /** Os valores a gravar: os campos tocados e as linhas alteradas. */
        async valuesToSave() {
            const values = {};
            for (const [name, widget] of this.widgets) {
                if (!this.dirty.has(name)) {
                    continue;
                }
                const value = await widget.read();
                if (value !== undefined) {
                    values[name] = value;
                }
            }
            for (const [name, lines] of this.x2many) {
                const commands = await lines.read();
                if (commands) {
                    values[name] = commands;
                }
            }
            return values;
        }

        async save() {
            const values = await this.valuesToSave();
            const ids = this.resId ? [this.resId] : [];
            const saved = await callKw(this.model, "web_save", [ids, values], {
                specification: this.specification(),
            });
            const record = Array.isArray(saved) ? saved[0] : saved;
            this.record = record || this.record;
            this.resId = this.record.id || this.resId;
            this.dirty.clear();
            // relê para trazer computados, linhas criadas e ids novos
            await this.load();
            this.render();
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

        /**
         * Os `<button type="object">` do arch: um método do modelo que o
         * servidor executa sobre este registro. Um registro ainda não
         * salvo não tem sobre o que agir, então o botão salva antes.
         */
        renderButtons() {
            return Array.from(this.archRoot.getElementsByTagName("button"))
                .filter((node) => node.getAttribute("name"))
                .map((node) => {
                    const name = node.getAttribute("name");
                    const label = node.getAttribute("string") || name;
                    const kind = node.getAttribute("class") || "btn-ghost";
                    return el(
                        "button",
                        {
                            class: "btn " + kind,
                            onclick: this.run(async () => {
                                if (!this.resId) {
                                    await this.save();
                                }
                                const answer = await callKw(this.model, name, [[this.resId]], {});
                                // o servidor pode responder com uma ação
                                // em vez de um booleano: nesse caso a
                                // navegação é a resposta
                                if (answer && answer.type === "ir.actions.act_window") {
                                    this.onAction(answer);
                                    return;
                                }
                                // o método mudou o registro no servidor:
                                // o que está na tela é o que ele deixou
                                await this.load();
                                this.render();
                            }),
                        },
                        label
                    );
                });
        }

        /**
         * `<report name="módulo.id"/>` no arch: o documento impresso
         * deste registro, aberto numa aba. Um registro ainda não salvo
         * não tem o que imprimir.
         */
        renderReports() {
            return Array.from(this.archRoot.getElementsByTagName("report"))
                .filter((node) => node.getAttribute("name"))
                .map((node) =>
                    el(
                        "button",
                        {
                            class: "btn btn-ghost",
                            disabled: !this.resId,
                            onclick: () =>
                                window.open(
                                    "/report/html/" +
                                        encodeURIComponent(node.getAttribute("name")) +
                                        "/" +
                                        this.resId,
                                    "_blank"
                                ),
                        },
                        node.getAttribute("string") || "Imprimir"
                    )
                );
        }

        renderControlPanel() {
            return el("div", { class: "o_control_panel" }, [
                el("h2", { class: "o_breadcrumb" }, [
                    el(
                        "a",
                        {
                            href: "#",
                            onclick: (event) => {
                                event.preventDefault();
                                this.onBack();
                            },
                        },
                        this.title
                    ),
                    " / ",
                    this.resId ? "#" + this.resId : "Novo",
                ]),
                el("div", { class: "o_cp_actions" }, [
                    ...this.renderReports(),
                    ...this.renderButtons(),
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
                    .filter(isTopLevel)
                    .filter((node) => this.fields[node.getAttribute("name")])
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
            // campos fora de qualquer <group> (as linhas de um pedido, em
            // geral) ocupam a largura toda, abaixo dos grupos
            const loose = this.fieldNodes().filter(
                (node) => !node.closest || !node.closest("group")
            );
            const wide = groups.length
                ? loose.map((node) => {
                      const name = node.getAttribute("name");
                      const meta = this.fields[name];
                      return el("div", { class: "o_form_wide" }, [
                          el("h3", {}, fieldLabel(name, meta, node.getAttribute("string"))),
                          this.renderField(name, node),
                      ]);
                  })
                : [];
            return el("div", { class: "o_form_sheet" }, rendered.concat(wide));
        }

        render() {
            this.widgets.clear();
            this.x2many.clear();
            const parts = [this.renderControlPanel(), this.renderSheet()];
            if (this.chatter) {
                parts.push(this.chatter.render());
            }
            fill(this.root, parts);
            return this.root;
        }
    }

    rusdoo.FormView = FormView;
})((window.rusdoo = window.rusdoo || {}));

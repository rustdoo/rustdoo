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
    const { callKw, canPrintPdf } = rusdoo.rpc;

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
            // num diálogo o formulário é o assistente inteiro: os botões
            // dele são os do arch, e não Salvar/Excluir de um registro
            // que o usuário nunca vai procurar depois
            this.inDialog = Boolean(config.dialog);
            this.onError = config.onError || function () {};

            this.archRoot = parseArch(config.arch);
            // a aba aberta e o campo que virou barra de status: ambos são
            // decididos ao desenhar, e zerados a cada desenho
            this.activePage = null;
            this.statusbarField = null;
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
                // a imagem é servida por URL e cacheada por uma hora; sem
                // a data da última escrita no endereço, trocar a foto de
                // um produto não trocaria a foto na tela
                if (meta.type === "binary") {
                    spec.write_date = {};
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
                model: this.model,
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
                                // o método lê o registro no servidor: o
                                // que está na tela e ainda não foi salvo
                                // não existe para ele
                                if (!this.resId || this.dirty.size || this.x2many.size) {
                                    await this.save();
                                }
                                const answer = await callKw(this.model, name, [[this.resId]], {});
                                // o servidor pode responder com uma ação
                                // em vez de um booleano: nesse caso a
                                // navegação é a resposta
                                // uma ação (abrir outra tela) ou um "fechei":
                                // as duas são navegação, e quem navega é
                                // o cliente
                                if (
                                    answer &&
                                    (answer.type === "ir.actions.act_window" ||
                                        answer.type === "ir.actions.act_window_close")
                                ) {
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
                                    // PDF quando o servidor sabe converter,
                                    // HTML quando não: o mesmo documento,
                                    // e a página o usuário imprime pelo
                                    // browser. Nunca pedir o PDF a um
                                    // servidor que respondería 503.
                                    (canPrintPdf() ? "/report/pdf/" : "/report/html/") +
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
            if (this.inDialog) {
                return el("div", { class: "o_control_panel" }, [
                    el("div", { class: "o_cp_actions" }, [
                        ...this.renderButtons(),
                        el(
                            "button",
                            { class: "btn btn-ghost", onclick: () => this.onBack() },
                            "Fechar"
                        ),
                    ]),
                ]);
            }
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
                    // os botões de ação do arch vão para a barra de status,
                    // dentro da folha, quando o registro tem estado
                    ...(this.hasStatusbar() ? [] : this.renderButtons()),
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

        /**
         * Desenha o arch na ordem em que ele foi escrito.
         *
         * A versão anterior varria o documento atrás de `<group>` e
         * jogava o resto no fim: a tela saía com os campos certos na
         * ordem errada, e um `<notebook>` sumia. Aqui cada nó do arch
         * vira o que ele diz ser, e um nó que este cliente ainda não
         * desenha é ignorado explicitamente — não descartado por acaso.
         */
        renderSheet() {
            const body = [];
            const statusbar = this.renderStatusbar();
            if (statusbar) {
                body.push(statusbar);
            }
            for (const node of Array.from(this.archRoot.childNodes)) {
                const drawn = this.renderNode(node);
                if (drawn) {
                    body.push(drawn);
                }
            }
            return el("div", { class: "o_form_sheet_bg" }, [
                el("div", { class: "o_form_sheet" }, body),
            ]);
        }

        /**
         * A barra de status: os estágios pelos quais o registro passa.
         *
         * O `state` de um documento não é um campo como os outros — é
         * onde ele está na vida dele, e ver isso de relance é metade do
         * motivo de abrir o formulário.
         */
        renderStatusbar() {
            if (!this.hasStatusbar()) {
                return null;
            }
            this.statusbarField = "state";
            const meta = this.fields.state;
            const options = meta.selection || [];
            const current = this.record ? this.record.state : null;
            const currentIndex = options.findIndex(([value]) => value === current);
            const stages = options
                // um estágio terminal (cancelado) não fica no caminho dos
                // outros: só aparece quando é onde o registro está
                .filter(([value]) => value !== "cancel" || value === current)
                .map(([value, label]) => {
                    const index = options.findIndex(([v]) => v === value);
                    const done = currentIndex >= 0 && index < currentIndex;
                    return el(
                        "span",
                        {
                            class:
                                "o_stage" +
                                (value === current ? " o_stage_active" : "") +
                                (done ? " o_stage_done" : ""),
                        },
                        label
                    );
                });
            return el("div", { class: "o_form_statusbar" }, [
                el("div", { class: "o_statusbar_buttons" }, this.renderButtons()),
                el("div", { class: "o_statusbar_status" }, stages),
            ]);
        }

        /** Se o arch declara um `state`, ele ganha barra de status. */
        hasStatusbar() {
            return Boolean(
                this.fields.state &&
                    this.fieldNodes().some((n) => n.getAttribute("name") === "state")
            );
        }

        /** Um nó do arch, desenhado conforme o que ele é. */
        renderNode(node) {
            if (!node.tagName) {
                return null;
            }
            switch (node.tagName.toLowerCase()) {
                case "group":
                    return this.renderGroup(node);
                case "notebook":
                    return this.renderNotebook(node);
                case "separator":
                    return el(
                        "div",
                        { class: "o_separator" },
                        node.getAttribute("string") || ""
                    );
                case "field":
                    return this.renderWideField(node);
                // desenhados fora da folha: os botões na barra de status,
                // o chatter abaixo dela, o relatório no painel de controle
                case "button":
                case "report":
                case "chatter":
                    return null;
                default:
                    return null;
            }
        }

        /**
         * Um `<group>`: rótulo à esquerda, valor à direita.
         *
         * Grupos irmãos ficam lado a lado, que é a assinatura de um
         * formulário do Odoo — duas colunas de pares, não uma lista
         * comprida que obriga a rolar.
         */
        renderGroup(node) {
            const children = Array.from(node.childNodes).filter((n) => n.tagName);
            const inner = children.filter((n) => n.tagName.toLowerCase() === "group");
            if (inner.length) {
                return el(
                    "div",
                    { class: "o_group_pair" },
                    inner.map((child) => this.renderGroup(child))
                );
            }
            const rows = children
                .map((child) => {
                    if (child.tagName.toLowerCase() !== "field") {
                        return this.renderNode(child);
                    }
                    const name = child.getAttribute("name");
                    const meta = this.fields[name];
                    if (!meta) {
                        return null;
                    }
                    // um x2many dentro de um grupo ainda quer a largura
                    // toda: uma tabela espremida em meia coluna não é
                    // legível
                    if (meta.type === "one2many" || meta.type === "many2many") {
                        return this.renderWideField(child);
                    }
                    return el("div", { class: "o_form_row" }, [
                        el(
                            "label",
                            { class: meta.required ? "o_required" : null },
                            fieldLabel(name, meta, child.getAttribute("string"))
                        ),
                        this.renderField(name, child),
                    ]);
                })
                .filter(Boolean);
            const title = node.getAttribute("string");
            return el(
                "div",
                { class: "o_group" },
                (title ? [el("div", { class: "o_group_title" }, title)] : []).concat(rows)
            );
        }

        /** Um `<notebook>`: as abas do formulário. */
        renderNotebook(node) {
            const pages = Array.from(node.childNodes).filter(
                (n) => n.tagName && n.tagName.toLowerCase() === "page"
            );
            if (!pages.length) {
                return null;
            }
            const active = this.activePage && pages.some((p, i) => this.pageKey(p, i) === this.activePage)
                ? this.activePage
                : this.pageKey(pages[0], 0);
            this.activePage = active;
            const tabs = pages.map((page, index) => {
                const key = this.pageKey(page, index);
                return el(
                    "button",
                    {
                        type: "button",
                        class: "o_notebook_tab" + (key === active ? " o_active" : ""),
                        onclick: () => {
                            this.activePage = key;
                            this.render();
                        },
                    },
                    page.getAttribute("string") || "Página " + (index + 1)
                );
            });
            const shown = pages.find((page, index) => this.pageKey(page, index) === active);
            const content = Array.from(shown.childNodes)
                .map((child) => this.renderNode(child))
                .filter(Boolean);
            return el("div", { class: "o_notebook" }, [
                el("div", { class: "o_notebook_headers" }, tabs),
                el("div", { class: "o_notebook_page" }, content),
            ]);
        }

        pageKey(page, index) {
            return page.getAttribute("string") || String(index);
        }

        /** Um campo fora de grupo: rótulo em cima, campo na largura toda. */
        renderWideField(node) {
            const name = node.getAttribute("name");
            const meta = this.fields[name];
            if (!meta) {
                return null;
            }
            // o `state` já aparece na barra de status
            if (name === this.statusbarField) {
                return null;
            }
            // sem rótulo quando o arch não pediu um: uma tabela de linhas
            // dentro de uma aba chamada "Linhas da fatura" não precisa de
            // um título dizendo "Line Ids" logo acima
            const declared = node.getAttribute("string");
            return el(
                "div",
                { class: "o_form_wide" },
                (declared ? [el("h3", {}, declared)] : []).concat([
                    this.renderField(name, node),
                ])
            );
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

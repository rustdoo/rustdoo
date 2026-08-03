/**
 * O chatter: o histórico de conversa preso a um registro, que no Odoo
 * acompanha quase todo modelo de negócio.
 *
 * O corpo de uma mensagem é sempre texto — inserido como nó de texto,
 * nunca como HTML. Um cliente que renderizasse o que outro usuário
 * escreveu como marcação daria a ele o navegador de todo mundo.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill } = rusdoo.utils;
    const { callKw } = rusdoo.rpc;

    /** Quantas mensagens o painel pede de uma vez. */
    const FETCH_LIMIT = 30;

    class Chatter {
        /**
         * @param {object} config model, resId e onError. Um registro
         *   ainda não salvo não tem histórico: o painel só aparece
         *   depois de existir.
         */
        constructor(config) {
            this.model = config.model;
            this.resId = config.resId || null;
            this.onError = config.onError || function () {};
            this.messages = [];
            this.attachments = [];
            this.root = el("div", { class: "o_chatter" });
        }

        async load() {
            if (!this.resId) {
                this.messages = [];
                this.attachments = [];
                return;
            }
            this.messages = await callKw(this.model, "message_fetch", [[this.resId]], {
                limit: FETCH_LIMIT,
            });
            // os anexos são registros como quaisquer outros: quem não
            // pode lê-los recebe erro, e o painel diz isso em vez de
            // esconder que existem
            this.attachments = await callKw("ir.attachment", "search_read", [
                [
                    ["res_model", "=", this.model],
                    ["res_id", "=", this.resId],
                ],
            ], { fields: ["id", "name", "file_size"] });
        }

        /** Envia os arquivos escolhidos e recarrega o painel. */
        async upload(files) {
            const form = new FormData();
            form.append("model", this.model);
            form.append("id", String(this.resId));
            for (const file of files) {
                form.append("ufile", file, file.name);
            }
            const response = await fetch("/web/binary/upload_attachment", {
                method: "POST",
                credentials: "same-origin",
                body: form,
            });
            const answer = await response.json();
            if (!response.ok || answer.error) {
                throw new Error(answer.error || "falha no envio");
            }
            await this.load();
            this.render();
        }

        async post(body) {
            await callKw(this.model, "message_post", [[this.resId]], { body: body });
            await this.load();
            this.render();
        }

        renderComposer() {
            const input = el("textarea", {
                class: "o_input o_composer",
                rows: "2",
                placeholder: "Escreva um comentário…",
            });
            const send = el(
                "button",
                {
                    class: "btn btn-primary",
                    type: "button",
                    onclick: async () => {
                        const body = input.value.trim();
                        if (!body) {
                            return;
                        }
                        try {
                            await this.post(body);
                        } catch (error) {
                            this.onError(error);
                        }
                    },
                },
                "Enviar"
            );
            return el("div", { class: "o_composer_box" }, [input, send]);
        }

        /** Tamanho legível: um anexo não se mede em bytes na tela. */
        renderSize(size) {
            const bytes = Number(size) || 0;
            if (bytes >= 1024 * 1024) {
                return (bytes / 1024 / 1024).toFixed(1) + " MB";
            }
            if (bytes >= 1024) {
                return Math.round(bytes / 1024) + " kB";
            }
            return bytes + " B";
        }

        renderAttachments() {
            const input = el("input", {
                type: "file",
                multiple: "multiple",
                class: "o_attach_input",
                onchange: async (event) => {
                    const files = Array.from(event.target.files || []);
                    if (!files.length) {
                        return;
                    }
                    try {
                        await this.upload(files);
                    } catch (error) {
                        this.onError(error);
                    }
                },
            });
            return el("div", { class: "o_attachments" }, [
                el("div", { class: "o_attach_head" }, [
                    el("strong", {}, "Anexos"),
                    input,
                ]),
                el(
                    "div",
                    { class: "o_attach_list" },
                    this.attachments.length
                        ? this.attachments.map((attachment) =>
                              el(
                                  "a",
                                  {
                                      class: "o_attachment",
                                      href: "/web/content/" + attachment.id,
                                      target: "_blank",
                                      rel: "noopener",
                                  },
                                  [
                                      attachment.name,
                                      el(
                                          "span",
                                          { class: "o_attach_size" },
                                          this.renderSize(attachment.file_size)
                                      ),
                                  ]
                              )
                          )
                        : el("span", { class: "o_nocontent" }, "Nenhum anexo.")
                ),
            ]);
        }

        renderMessage(message) {
            return el("div", { class: "o_message" }, [
                el("div", { class: "o_message_head" }, [
                    el("strong", {}, message.author || ""),
                    el("span", { class: "o_message_date" }, message.date || ""),
                ]),
                message.subject ? el("div", { class: "o_message_subject" }, message.subject) : null,
                // texto, sempre: o corpo é o que a pessoa escreveu, não
                // marcação que o navegador de quem lê deva executar
                el("div", { class: "o_message_body" }, message.body || ""),
            ]);
        }

        render() {
            if (!this.resId) {
                fill(this.root, null);
                return this.root;
            }
            fill(this.root, [
                el("h3", {}, "Discussão"),
                this.renderAttachments(),
                this.renderComposer(),
                el(
                    "div",
                    { class: "o_messages" },
                    this.messages.length
                        ? this.messages.map((message) => this.renderMessage(message))
                        : el("div", { class: "o_nocontent" }, "Nenhuma mensagem ainda.")
                ),
            ]);
            return this.root;
        }
    }

    rusdoo.Chatter = Chatter;
})((window.rusdoo = window.rusdoo || {}));

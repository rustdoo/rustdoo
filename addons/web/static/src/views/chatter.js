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
            this.root = el("div", { class: "o_chatter" });
        }

        async load() {
            if (!this.resId) {
                this.messages = [];
                return;
            }
            this.messages = await callKw(this.model, "message_fetch", [[this.resId]], {
                limit: FETCH_LIMIT,
            });
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

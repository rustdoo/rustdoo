/**
 * Camada RPC: o envelope JSON-RPC 2.0 que o servidor fala
 * (`/web/dataset/call_kw`, `/web/action/load`, `/web/session/*`).
 *
 * Um erro do servidor vira uma exceção com a mensagem dele — nunca um
 * `undefined` que quebra três telas depois, longe da causa.
 */
(function (rusdoo) {
    "use strict";

    /** Código do Odoo para sessão ausente ou expirada. */
    const SESSION_EXPIRED = 100;

    class RpcError extends Error {
        constructor(message, code) {
            super(message);
            this.name = "RpcError";
            this.code = code;
        }
        get isSessionExpired() {
            return this.code === SESSION_EXPIRED;
        }
    }

    let requestId = 0;

    /** POST de um envelope JSON-RPC, devolvendo `result` ou lançando. */
    async function rpc(url, params) {
        requestId += 1;
        let response;
        try {
            response = await fetch(url, {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                // o cookie de sessão é HttpOnly: quem o envia é o browser
                credentials: "same-origin",
                body: JSON.stringify({
                    jsonrpc: "2.0",
                    method: "call",
                    id: requestId,
                    params: params || {},
                }),
            });
        } catch (error) {
            throw new RpcError("servidor inacessível: " + error.message, 0);
        }
        if (!response.ok) {
            throw new RpcError("HTTP " + response.status, response.status);
        }
        const payload = await response.json();
        if (payload.error) {
            const message =
                (payload.error.data && payload.error.data.message) ||
                payload.error.message ||
                "erro desconhecido";
            throw new RpcError(message, payload.error.code);
        }
        return payload.result;
    }

    /** GET de uma rota que responde JSON puro (load_menus). */
    async function getJson(url) {
        const response = await fetch(url, { credentials: "same-origin" });
        if (!response.ok) {
            throw new RpcError("HTTP " + response.status, response.status);
        }
        return response.json();
    }

    /**
     * O contexto do usuário, como o servidor o devolveu no boot.
     *
     * Toda chamada o carrega, que é o que faz o idioma da sessão valer:
     * sem isso o servidor responderia sempre no idioma de origem, por
     * mais que o usuário tivesse escolhido outro.
     */
    let userContext = {};

    function setUserContext(context) {
        userContext = context && typeof context === "object" ? context : {};
    }

    /**
     * Se este servidor sabe converter um relatório em PDF.
     *
     * O cliente não tem como enxergar um binário no PATH de quem hospeda,
     * então quem responde é o servidor, uma vez, no login — e não a cada
     * clique no botão de imprimir.
     */
    let canPrintPdf = false;

    /** `call_kw`: um método do ORM sobre um modelo. */
    function callKw(model, method, args, kwargs) {
        const extra = kwargs || {};
        // um contexto que a chamada trouxe vence o da sessão, chave a
        // chave — é assim que um `with_context` pontual funciona
        const context = Object.assign({}, userContext, extra.context || {});
        return rpc("/web/dataset/call_kw", {
            model: model,
            method: method,
            args: args || [],
            kwargs: Object.assign({}, extra, { context: context }),
        });
    }

    rusdoo.rpc = {
        RpcError: RpcError,
        rpc: rpc,
        getJson: getJson,
        callKw: callKw,
        setUserContext: setUserContext,
        userContext: () => Object.assign({}, userContext),
        canPrintPdf: () => canPrintPdf,
        sessionInfo: async () => {
            const info = await rpc("/web/session/get_session_info", {});
            setUserContext(info && info.user_context);
            canPrintPdf = Boolean(info && info.can_print_pdf);
            return info;
        },
        authenticate: async (login, password) => {
            const answer = await rpc("/web/session/authenticate", {
                login: login,
                password: password,
            });
            // o idioma do usuário vale a partir do login, não do próximo
            // recarregamento da página
            const info = await rpc("/web/session/get_session_info", {});
            setUserContext(info && info.user_context);
            canPrintPdf = Boolean(info && info.can_print_pdf);
            return answer;
        },
        destroy: () => rpc("/web/session/destroy", {}),
        loadMenus: () => getJson("/web/webclient/load_menus"),
        loadAction: (actionId) => rpc("/web/action/load", { action_id: actionId }),
    };
})((window.rusdoo = window.rusdoo || {}));

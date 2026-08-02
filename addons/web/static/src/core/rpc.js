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

    /** `call_kw`: um método do ORM sobre um modelo. */
    function callKw(model, method, args, kwargs) {
        return rpc("/web/dataset/call_kw", {
            model: model,
            method: method,
            args: args || [],
            kwargs: kwargs || {},
        });
    }

    rusdoo.rpc = {
        RpcError: RpcError,
        rpc: rpc,
        getJson: getJson,
        callKw: callKw,
        sessionInfo: () => rpc("/web/session/get_session_info", {}),
        authenticate: (login, password) =>
            rpc("/web/session/authenticate", { login: login, password: password }),
        destroy: () => rpc("/web/session/destroy", {}),
        loadMenus: () => getJson("/web/webclient/load_menus"),
        loadAction: (actionId) => rpc("/web/action/load", { action_id: actionId }),
    };
})((window.rusdoo = window.rusdoo || {}));

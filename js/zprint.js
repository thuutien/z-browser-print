/*
 * zprint.js - browser client for Z Browser Print, the local print agent.
 *
 *   const zp = new ZPrint();
 *   const printer = await zp.defaultPrinter();
 *   await zp.write(printer.id, "^XA^FO50,50^A0N,40,40^FDHello^FS^XZ");
 *   const status = await zp.status(printer.id);   // { ready, paper_out, ... }
 */
(function (global) {
  "use strict";

  class ZPrintError extends Error {
    constructor(message, status) {
      super(message);
      this.name = "ZPrintError";
      /** HTTP status, or 0 when the agent could not be reached. */
      this.status = status;
    }
  }

  class ZPrint {
    constructor(baseUrl = "http://127.0.0.1:9180") {
      this.baseUrl = baseUrl.replace(/\/$/, "");
    }

    async _request(path, options = {}) {
      let res;
      try {
        res = await fetch(this.baseUrl + path, options);
      } catch (e) {
        throw new ZPrintError("Z Browser Print is not running or not reachable", 0);
      }
      if (!res.ok) {
        let message = await res.text();
        try {
          message = JSON.parse(message).error || message;
        } catch (_) {
          // plain text error
        }
        throw new ZPrintError(message || res.statusText, res.status);
      }
      return res;
    }

    _printerPath(id, action) {
      return `/v1/printers/${encodeURIComponent(id)}/${action}`;
    }

    /** Resolves true when the agent is installed and running. */
    async isAvailable() {
      try {
        await this.version();
        return true;
      } catch (_) {
        return false;
      }
    }

    async version() {
      return (await this._request("/v1/version")).json();
    }

    async printers() {
      return (await this._request("/v1/printers")).json();
    }

    async defaultPrinter() {
      return (await this._request("/v1/printers/default")).json();
    }

    /** Send raw data (string, Uint8Array or Blob) to the printer. */
    async write(printerId, data) {
      await this._request(this._printerPath(printerId, "write"), {
        method: "POST",
        headers: { "Content-Type": "text/plain" },
        body: data,
      });
    }

    /** Send a query such as "~HQES" and return the printer's reply as text. */
    async read(printerId, query, timeoutMs = 3000) {
      const res = await this._request(
        this._printerPath(printerId, "read") + `?timeout_ms=${timeoutMs}`,
        { method: "POST", headers: { "Content-Type": "text/plain" }, body: query }
      );
      return res.text();
    }

    /** Parsed ~HS status: { ready, paper_out, paused, head_open, ribbon_out, ... }. */
    async status(printerId) {
      return (await this._request(this._printerPath(printerId, "status"))).json();
    }
  }

  global.ZPrint = ZPrint;
  global.ZPrintError = ZPrintError;
})(typeof window !== "undefined" ? window : globalThis);

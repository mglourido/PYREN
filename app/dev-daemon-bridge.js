/**
 * Dev-server bridge to pyren-daemon.
 *
 * Outside Tauri there is no `invoke`, so `vite dev` in a browser used to
 * have no way to reach the daemon at all: every call failed and the UI fell
 * back to synthetic numbers. That made the browser useless for anything
 * beyond layout work - vitals showed drifting fake gauges next to empty
 * disk and process panels.
 *
 * This plugin gives the browser the same door the Tauri shell uses. It
 * accepts one JSON request per POST, forwards it down the daemon's Unix
 * socket as a single line, and returns the reply line. Dev only
 * (`apply: "serve"`), so it is never part of a packaged build.
 */

// @ts-expect-error node:net is available here; this file runs in Node, not
// the browser, and the project carries no @types/node (see vite.config.js).
import net from "node:net";

/** @import { Plugin } from "vite" */

const ENDPOINT = "/__daemon";
const TIMEOUT_MS = 5000;

/**
 * Where the daemon might be, most-likely first. Matches the resolution in
 * `daemon/crates/core/src/client.rs`: the installed systemd unit listens on
 * `/run/pyren/daemon.sock`, an unprivileged `cargo run` on
 * `/tmp/pyren-daemon.sock`, and knowing only the second means never finding
 * a properly installed daemon.
 */
function socketCandidates() {
  // @ts-expect-error process is a nodejs global
  const configured = process.env.PYREN_SOCKET;
  // An explicit setting is a decision, not a hint.
  return configured ? [configured] : ["/run/pyren/daemon.sock", "/tmp/pyren-daemon.sock"];
}

/** @returns {Plugin} */
export function daemonBridge() {
  return {
    name: "pyren-daemon-bridge",
    apply: "serve",

    configureServer(server) {
      server.middlewares.use(ENDPOINT, (/** @type {any} */ req, /** @type {any} */ res) => {
        if (req.method !== "POST") {
          res.statusCode = 405;
          res.end();
          return;
        }

        let body = "";
        req.on("data", (/** @type {unknown} */ chunk) => (body += chunk));
        req.on("end", () => forward(body, res));
      });
    },
  };
}

/** @param {string} body @param {any} res */
/** @param {string} body @param {any} res @param {string[]} [remaining] */
function forward(body, res, remaining = socketCandidates()) {
  const [path, ...rest] = remaining;
  const socket = net.createConnection(path);
  let reply = "";
  let answered = false;

  // The socket can fail, time out and close; only the first of those gets
  // to write a response.
  const answer = (/** @type {number} */ status, /** @type {string} */ payload) => {
    if (answered) return;
    answered = true;
    socket.destroy();
    res.statusCode = status;
    res.setHeader("content-type", "application/json");
    res.end(payload);
  };

  socket.setTimeout(TIMEOUT_MS, () => answer(504, json({ error: "daemon timed out" })));
  socket.on("error", (/** @type {Error} */ e) => {
    // Try the next candidate before giving up; only the last failure is
    // worth reporting, and it names the path it actually tried.
    if (rest.length > 0 && !answered) {
      answered = true;
      socket.destroy();
      forward(body, res, rest);
      return;
    }
    answer(502, json({ error: `cannot reach pyren-daemon at ${path}: ${e.message}` }));
  });
  socket.on("connect", () => socket.write(`${body.trim()}\n`));
  socket.on("data", (/** @type {unknown} */ chunk) => {
    reply += chunk;
    // The protocol is one JSON document per line, so the first newline ends
    // the reply even when the daemon keeps the connection open.
    const end = reply.indexOf("\n");
    if (end !== -1) answer(200, reply.slice(0, end));
  });
  socket.on("close", () => answer(502, json({ error: "daemon closed the connection" })));
}

/** @param {unknown} value */
function json(value) {
  return JSON.stringify(value);
}

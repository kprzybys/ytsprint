// ytsprint-jsd: a long-running deno process that keeps YouTube players loaded in memory.
// It saves ~0.45 s per play (process start + ~300 ms loading the 4 MB player).
//
// Sandbox: deno runs with access to its socket only (no other file, no network, no subprocesses).
// The YouTube player is remote code; here it cannot read or write any file. Everything it needs —
// the ejs solver scripts and the player — arrives over the socket, and only when jsd asks for it.
//
// Protocol (Unix socket, one JSON line each way; one request per connection):
//   -> {"player_key": "<player id>-ejs<tag>", "ejs_tag": "<tag>", "requests": [{"type": "n"|"sig", "challenges": [...]}],
//       "ejs"?: {"lib": "...", "core": "..."}, "raw"?: "<base.js>", "pre"?: "<prepared player>"}
//   <- {"type": "need", "what": ["ejs"?, "player"?]}           send the request again with these fields
//    | {"type": "result", "responses": [...], "preprocessed_player"?: "..."}   (the latter after preparing `raw`)
//    | {"type": "error", "kind": "bad_prepared" | "ejs" | "protocol", "error": "..."}

const SOCK = Deno.args[0];
const MAX_PLAYERS = 2;
const CONN_DEADLINE_MS = 15000;
const KEY_RE = /^[0-9a-f]{8,16}-ejs[0-9a-f]{8,40}$/;
const TAG_RE = /^[0-9a-f]{8,40}$/;

let ejsTag = "";
let jsc = null;
const players = new Map(); // player_key -> {s: {n, sig}, pre: prepared source (returned when a client lost its copy)}

function loadEjs(tag, ejs) {
  // the ejs scripts define `var jsc` (and `lib`) at top level; indirect eval = global scope
  (0, eval)(`${ejs.lib}\nObject.assign(globalThis, lib);\n${ejs.core}\nglobalThis.__ytsprint_jsc = jsc;`);
  jsc = globalThis.__ytsprint_jsc;
  ejsTag = tag;
  players.clear(); // players prepared by another ejs version are not reused
}

// ejs-internal (yt-dlp/ejs solvers.ts getFromPrepared): the prepared player sets _result.n/_result.sig.
function loadPrepared(pre) {
  const r = { n: null, sig: null };
  Function("_result", pre)(r);
  return r;
}

function handle(req) {
  if (typeof req.player_key !== "string" || !KEY_RE.test(req.player_key)) {
    return { type: "error", kind: "protocol", error: `bad player_key: ${req.player_key}` };
  }
  if (typeof req.ejs_tag !== "string" || !TAG_RE.test(req.ejs_tag) || !Array.isArray(req.requests)) {
    return { type: "error", kind: "protocol", error: "bad ejs_tag or requests" };
  }
  const need = [];
  if (ejsTag !== req.ejs_tag && !req.ejs) need.push("ejs");
  const known = ejsTag === req.ejs_tag && players.has(req.player_key);
  if (!known && !req.raw && !req.pre) need.push("player");
  if (need.length) return { type: "need", what: need };

  if (ejsTag !== req.ejs_tag) loadEjs(req.ejs_tag, req.ejs);

  // `raw` means the client has no prepared copy (new player or a deleted cache file): it gets one back
  let entry = players.get(req.player_key);
  let s = entry?.s;
  let prepared = req.raw && entry ? entry.pre : undefined;
  if (!s) {
    let pre = req.pre;
    if (req.raw) {
      const out = jsc({ type: "player", player: req.raw, requests: [], output_preprocessed: true });
      if (out.type !== "result" || !out.preprocessed_player) {
        return { type: "error", kind: "ejs", error: `ejs: ${out.error ?? "no preprocessed_player"}` };
      }
      pre = prepared = out.preprocessed_player;
    }
    try {
      s = loadPrepared(pre);
    } catch (e) {
      return { type: "error", kind: req.raw ? "ejs" : "bad_prepared", error: `loading the prepared player: ${e}` };
    }
    if (players.size >= MAX_PLAYERS) players.delete(players.keys().next().value);
    players.set(req.player_key, { s, pre });
  }

  const responses = req.requests.map((r) => {
    const f = s[r.type];
    if (!f) return { type: "error", error: `Failed to extract ${r.type} function` };
    try {
      return { type: "result", data: Object.fromEntries(r.challenges.map((c) => [c, f(c)])) };
    } catch (e) {
      return { type: "error", error: `${e}` };
    }
  });
  const out = { type: "result", responses };
  if (prepared) out.preprocessed_player = prepared;
  return out;
}

async function serve(conn) {
  // a client that never finishes its request must not hold the connection forever
  const deadline = setTimeout(() => {
    try { conn.close(); } catch { /* closed */ }
  }, CONN_DEADLINE_MS);
  const dec = new TextDecoder();
  const enc = new TextEncoder();
  const chunk = new Uint8Array(1 << 16);
  const parts = [];
  let out;
  try {
    for (;;) {
      const n = await conn.read(chunk);
      if (n === null) break;
      const text = dec.decode(chunk.subarray(0, n), { stream: true });
      parts.push(text);
      if (text.includes("\n")) break;
    }
    out = handle(JSON.parse(parts.join("")));
  } catch (e) {
    out = { type: "error", kind: "protocol", error: e instanceof Error ? `${e.message}\n${e.stack}` : `${e}` };
  }
  try {
    const data = enc.encode(JSON.stringify(out) + "\n");
    for (let off = 0; off < data.length;) off += await conn.write(data.subarray(off));
  } catch { /* client gone */ }
  clearTimeout(deadline);
  try { conn.close(); } catch { /* closed by the deadline */ }
}

const listener = Deno.listen({ transport: "unix", path: SOCK });
console.error(`ytsprint-jsd: listening on ${SOCK}`);
// connections are served concurrently; handle() itself is synchronous, so state changes do not interleave
for await (const conn of listener) serve(conn);

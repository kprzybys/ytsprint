// ytsprint-jsd: a long-running deno process that keeps YouTube players loaded in memory.
// It saves ~0.45 s per play (process start + ~300 ms loading the 4 MB player).
// The ejs solver (yt-dlp) prepares the player once; after that only the extracted n/sig functions run.
//
// Protocol (Unix socket, one JSON line each way):
//   -> {"ejs_dir", "player_id", "pre_path", "raw_path"?, "requests": [{"type": "n"|"sig", "challenges": [...]}]}
//   <- {"type": "result", "responses": [...]} | {"type": "error", "error": "..."}
// raw_path: the player has not been prepared yet; jsd prepares it and writes pre_path (cache for later).

const SOCK = Deno.args[0];
const MAX_PLAYERS = 2;

// ejs comes with yt-dlp; the client says where it is (ytsprint finds it). Reloaded after a change.
let ejsKey = "";
function loadEjs(dir) {
  const m = Math.max(...["lib.min.js", "core.min.js"].map((f) => Deno.statSync(`${dir}/${f}`).mtime.getTime()));
  const key = `${dir}@${m}`;
  if (key === ejsKey) return;
  const lib = Deno.readTextFileSync(`${dir}/lib.min.js`);
  const core = Deno.readTextFileSync(`${dir}/core.min.js`);
  (0, eval)(`${lib}\nObject.assign(globalThis, lib);\n${core}\nglobalThis.jsc = jsc;`);
  ejsKey = key;
  players.clear(); // a new ejs version may prepare players differently
}

const players = new Map(); // pre_path (player id + ejs version) -> {n, sig}

// ejs-internal (yt-dlp/ejs solvers.ts getFromPrepared): the prepared player sets _result.n/_result.sig.
function loadPrepared(pre) {
  const r = { n: null, sig: null };
  Function("_result", pre)(r);
  return r;
}

function solvers(req) {
  // raw_path = ytsprint has no prepared file: always prepare and write it (otherwise ytsprint would fetch base.js every time)
  let s = req.raw_path ? null : players.get(req.pre_path);
  if (s) return s;
  let pre;
  if (req.raw_path) {
    const out = globalThis.jsc({
      type: "player",
      player: Deno.readTextFileSync(req.raw_path),
      requests: [],
      output_preprocessed: true,
    });
    if (out.type !== "result" || !out.preprocessed_player) throw new Error(`ejs: ${out.error ?? "no preprocessed_player"}`);
    pre = out.preprocessed_player;
    Deno.writeTextFileSync(req.pre_path, pre);
  } else {
    pre = Deno.readTextFileSync(req.pre_path);
  }
  s = loadPrepared(pre);
  if (players.size >= MAX_PLAYERS) players.delete(players.keys().next().value);
  players.set(req.pre_path, s);
  return s;
}

function handle(req) {
  loadEjs(req.ejs_dir);
  const s = solvers(req);
  const responses = req.requests.map((r) => {
    const f = s[r.type];
    if (!f) return { type: "error", error: `Failed to extract ${r.type} function` };
    try {
      return { type: "result", data: Object.fromEntries(r.challenges.map((c) => [c, f(c)])) };
    } catch (e) {
      return { type: "error", error: `${e}` };
    }
  });
  return { type: "result", responses };
}

try { Deno.removeSync(SOCK); } catch { /* none */ }
const listener = Deno.listen({ transport: "unix", path: SOCK });
console.error(`ytsprint-jsd: listening on ${SOCK}`);

const enc = new TextEncoder();
const dec = new TextDecoder();
for await (const conn of listener) {
  // requests one at a time: jsd is meant for a single user, a request takes ~1 ms
  let buf = "";
  const chunk = new Uint8Array(65536);
  let out;
  try {
    for (;;) {
      const n = await conn.read(chunk);
      if (n === null) break;
      buf += dec.decode(chunk.subarray(0, n), { stream: true });
      if (buf.includes("\n")) break;
    }
    out = handle(JSON.parse(buf));
  } catch (e) {
    out = { type: "error", error: e instanceof Error ? `${e.message}\n${e.stack}` : `${e}` };
  }
  try {
    const data = enc.encode(JSON.stringify(out) + "\n");
    for (let off = 0; off < data.length;) off += await conn.write(data.subarray(off));
  } catch { /* client gone */ }
  conn.close();
}

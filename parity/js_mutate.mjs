// Parity harness: apply the JS AUTO_CACHE mutation pipeline exactly as server.js
// does, reading a request body as JSON on stdin and writing the mutated body to
// stdout. Used by tests/parity.rs to prove the Rust port is byte-equivalent.
//
//   node parity/js_mutate.mjs [tailTtl]   # tailTtl = 5m | 1h (default 5m)

import {
  injectBreakpointIfAbsent,
  normalizeTailBreakpoints,
  rewriteCacheControl,
  stripIntermediateMessageBreakpoints,
} from "./cache.js";

const tailTtl = process.argv[2] || "5m";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const body = JSON.parse(input);

// Same order as src/server.js when AUTO_CACHE is on.
stripIntermediateMessageBreakpoints(body);
const { tailBlocks } = injectBreakpointIfAbsent(body, { tailTtl });
const clientTail = normalizeTailBreakpoints(body, tailTtl);
const skip = new Set([...tailBlocks, ...clientTail]);
const counter = { rewritten: 0, alreadySet: 0, skipped: 0 };
rewriteCacheControl(body, counter, skip);

process.stdout.write(JSON.stringify(body));

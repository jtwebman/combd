// Node (V8) and Bun (JSC) baseline for the JS spike: the same app.js and the
// same requests. Run with `node --expose-gc bench.mjs` or `bun bench.mjs`.
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const name = process.versions.bun ? 'bun' : 'node';
const here = (f) => new URL(f, import.meta.url);
const src = readFileSync(here('./app.js'), 'utf8');
const reqs = readFileSync(here('./requests.txt'), 'utf8').trim().split('\n');
const quiet = { log() {} };
const gc = () => (globalThis.gc ? globalThis.gc() : globalThis.Bun?.gc(true));
const us = (ns) => (Number(ns) / 1e3).toFixed(1);
const time = (f) => {
  const t = process.hrtime.bigint();
  f();
  return process.hrtime.bigint() - t;
};
const log = console.log.bind(console);
const line = (s) => log(`js       ${name.padEnd(9)} ${s}`);

// One isolated instance per tenant: a fresh vm context with its own globals.
function context() {
  let n = 0;
  const ctx = vm.createContext({ combd: { counter: () => ++n }, console: quiet });
  vm.runInContext(src, ctx);
  return ctx;
}

let total = 0n;
for (let i = 0; i < 200; i++) total += time(() => context().handle(reqs[0]));
line(`cold (new vm context + app + first request) ${us(total / 200n)} us`);

// Warm requests run in the main realm, where the JIT is at its best.
let n = 0;
globalThis.combd = { counter: () => ++n };
globalThis.console.log = quiet.log;
(0, eval)(src);
for (const r of reqs) {
  const label = r.slice(0, 22);
  const iters = r.startsWith('fib') ? 50 : 5000;
  for (let i = 0; i < 500; i++) handle(r);
  let t = 0n;
  for (let i = 0; i < iters; i++) t += time(() => handle(r));
  const out = handle(r);
  line(`${label.padEnd(22)} ${us(t / BigInt(iters)).padStart(9)} us   -> ${out.length} bytes: ${out.slice(0, 40)}`);
}

gc();
const before = process.memoryUsage().rss;
const keep = Array.from({ length: 1000 }, context);
gc();
const after = process.memoryUsage().rss;
line(`memory per vm context ${((after - before) / 1000 / 1024).toFixed(1)} KB (a context, not a full isolate: a lower bound) [${keep.length}]`);

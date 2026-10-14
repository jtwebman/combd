// Writes Node's exact answer to every request in requests.txt: the oracle the
// Rust cell and the JS-to-Rust compiler must match byte for byte.
//   node langs/js/expected.mjs > langs/js/expected.txt
import { readFileSync } from 'node:fs';

const here = (f) => new URL(f, import.meta.url);
let n = 0;
globalThis.combd = { counter: () => ++n };
globalThis.console.log = () => {};
(0, eval)(readFileSync(here('./app.js'), 'utf8'));
for (const r of readFileSync(here('./requests.txt'), 'utf8').trim().split('\n')) {
  process.stdout.write(handle(r) + '\n');
}

// Appended after langs/js/app.js to make one bundle: the standard Workers
// entry point around the app's `handle`. workerd, celld, and Bun all run it.
let combdCount = 0;
globalThis.combd = { counter: () => ++combdCount };

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const body = request.method === 'POST' ? await request.text() : '';
    const out = handle(body ? `api ${request.method} ${url.pathname} ${body}` : `api ${request.method} ${url.pathname}`);
    const sp = out.indexOf(' ');
    return new Response(out.slice(sp + 1), {
      status: Number(out.slice(0, sp)),
      headers: { 'content-type': 'application/json' },
    });
  },
};

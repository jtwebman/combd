// The combd JS test cell: a tiny Hono-style app (a router with path params,
// JSON in and out) plus the same five commands every language cell answers.
// Plain JS on purpose: real Hono needs Request/Response/URL, which come later.

const routes = [];
function compile(path) {
  const keys = [];
  const re = new RegExp('^' + path.replace(/:(\w+)/g, (_, k) => (keys.push(k), '([^/]+)')) + '$');
  return { re, keys };
}
const app = {
  get: (path, h) => routes.push(['GET', compile(path), h]),
  post: (path, h) => routes.push(['POST', compile(path), h]),
};

const users = Array.from({ length: 100 }, (_, i) => ({
  id: i,
  name: `user${i}`,
  email: `user${i}@example.com`,
  tags: ['alpha', 'beta', 'gamma'].slice(0, (i % 3) + 1),
  score: (i * 7) % 13,
}));

app.get('/api/users/:id', (c) => {
  const u = users[Number(c.params.id) % users.length];
  return { status: 200, body: { ...u, greeting: `Hello, ${u.name}!` } };
});

app.get('/api/users', () => ({
  status: 200,
  body: { count: users.length, users: users.slice(0, 20).map((u) => ({ id: u.id, name: u.name, score: u.score })) },
}));

app.post('/api/orders', (c) => {
  const order = JSON.parse(c.body);
  const total = order.items.reduce((sum, it) => sum + it.price * it.qty, 0);
  const skus = order.items.map((it) => it.sku.toUpperCase());
  return { status: 201, body: { id: order.id, total: Math.round(total * 100) / 100, count: skus.length, skus } };
});

// CPU-heavy on purpose: the bad neighbor in fairness tests.
app.get('/api/fib/:n', (c) => {
  const n = Math.min(Number(c.params.n), 40);
  return { status: 200, body: { n, fib: fib(n) } };
});

function dispatch(method, path, body) {
  for (const [m, { re, keys }, h] of routes) {
    if (m !== method) continue;
    const hit = re.exec(path);
    if (!hit) continue;
    const params = {};
    keys.forEach((k, i) => (params[k] = hit[i + 1]));
    const r = h({ params, body });
    return `${r.status} ${JSON.stringify(r.body)}`;
  }
  return '404 {"error":"not found"}';
}

function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

globalThis.handle = (req) => {
  const sp = req.indexOf(' ');
  const cmd = sp < 0 ? req : req.slice(0, sp);
  const rest = sp < 0 ? '' : req.slice(sp + 1);
  switch (cmd) {
    case 'hello':
      return 'hello #' + combd.counter();
    case 'fib':
      return String(fib(Number(rest)));
    case 'print':
      console.log('hi from js');
      return 'printed';
    case 'spin':
      for (;;) {}
    case 'panic':
      throw new Error('boom');
    case 'api': {
      const [method, path, ...body] = rest.split(' ');
      return dispatch(method, path, body.join(' '));
    }
    default:
      return 'unknown';
  }
};

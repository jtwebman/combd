// Node with its own http server and no Request/Response wrapping: its best case.
import http from 'node:http';
import './worker.mjs';

http
  .createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      const out = handle(body ? `api ${req.method} ${req.url} ${body}` : `api ${req.method} ${req.url}`);
      const sp = out.indexOf(' ');
      res.writeHead(Number(out.slice(0, sp)), { 'content-type': 'application/json' });
      res.end(out.slice(sp + 1));
    });
  })
  .listen(8080, '0.0.0.0');

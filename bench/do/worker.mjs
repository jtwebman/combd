// The Durable Object race app for workerd and celld: the same counter and chat
// room as the combd Rust test cell (guest/src/lib.rs, `room`), with the same
// answers, written against Cloudflare's API.
import { DurableObject } from 'cloudflare:workers';

const KEEP = 20;

export class Room extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec('CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v INTEGER)');
    this.sql.exec('CREATE TABLE IF NOT EXISTS msgs (id INTEGER PRIMARY KEY, body TEXT)');
    this.count = this.sql.exec("SELECT v FROM kv WHERE k = 'count'").toArray()[0]?.v ?? 0;
    this.msgs = this.sql.exec('SELECT body FROM msgs ORDER BY id DESC LIMIT ?', KEEP).toArray().map((r) => r.body).reverse();
  }

  async fetch(req) {
    const path = new URL(req.url).pathname.replace(/^\/do\/[^/]+/, '') || '/';
    const json = (status, body) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
    switch (`${req.method} ${path}`) {
      case 'GET /count':
        return json(200, { count: this.count });
      case 'POST /incr':
        this.count += 1;
        this.sql.exec("INSERT INTO kv (k, v) VALUES ('count', ?) ON CONFLICT (k) DO UPDATE SET v = excluded.v", this.count);
        return json(200, { count: this.count });
      case 'GET /msgs':
        return json(200, { msgs: this.msgs });
      case 'POST /msg': {
        const body = await req.text();
        const id = this.sql.exec('INSERT INTO msgs (body) VALUES (?) RETURNING id', body).one().id;
        this.msgs.push(body);
        if (this.msgs.length > KEEP) this.msgs.shift();
        return json(201, { id });
      }
      default:
        return json(404, { error: 'not found' });
    }
  }
}

export default {
  fetch(req, env) {
    const name = new URL(req.url).pathname.split('/')[2];
    if (!name) return new Response('{"error":"not found"}', { status: 404 });
    return env.ROOM.get(env.ROOM.idFromName(name)).fetch(req);
  },
};

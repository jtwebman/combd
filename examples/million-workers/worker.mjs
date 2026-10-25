// One Million Checkboxes as a Cloudflare Workers app with Durable Objects:
// examples/million (combd) ported, with the same page, protocol, and limits,
// so the same load client drives both. See README.md for how it maps, and
// what each way of holding the sockets measured.
//
// A page's socket is held one of three ways (`SESSIONS`):
// - "hub" (the default): Hub objects (HUBS) hold every page's socket,
//   hibernatable; a chunk sends a beat's changes to each Hub with a watcher
//   (one RPC per Hub, not per page), and the Hub sends them to its pages.
// - "object": a Viewer object per socket, which accepts it hibernatable, as
//   million's viewer session; chunks call each watcher's Viewer every beat.
// - "worker": the Worker invocation holds the page's socket (it can't
//   hibernate) and opens a socket to each chunk on screen, which the chunk
//   holds hibernatable; a chunk's changes go up those sockets and straight on.
import { DurableObject } from 'cloudflare:workers';
import INDEX from './index.html';

/** Boxes in a chunk: 64 chunks make a million. */
const CHUNK = 15_625;
const CHUNKS = 64;
/** The most chunks one page may watch at once. */
const MAX_WATCH = 4;
/** Sets a page may make a second. */
const MAX_SETS_PER_S = 20;
/** Watchers hear a chunk's changes on a beat, one message per beat. */
const BEAT_MS = 150;
/** The total hears from a chunk at most this often. */
const TOLD_MS = 1000;
/** A subscription lasts this long unless the viewer renews it (every 20 s). */
const SUB_MS = 60_000;
const RENEW_MS = 20_000;
/** How often a chunk (or a Hub) drops watchers whose subscriptions ran out. */
const PRUNE_MS = 10_000;
/** Hubs: pages are spread over this many. One did best on one core (the
 * race's box; 4 and 16 measured worse); on Cloudflare, where each Hub is a
 * thread on some machine, you'd raise it with the load. */
const HUBS = 1;

const HEX = Array.from({ length: 256 }, (_, b) => b.toString(16).padStart(2, '0'));
const allMsg = (n, bits) => {
  let hex = '';
  for (const b of bits) hex += HEX[b];
  return JSON.stringify({ t: 'all', n, bits: hex });
};

/** The chunks a page showing boxes [from, to) watches, as million's viewer. */
function chunksFor(v) {
  const from = Math.max(0, Math.floor(Number(v.from) || 0));
  const to = Math.max(0, Math.floor(Number(v.to) || 0));
  const lo = Math.min(Math.floor(from / CHUNK), CHUNKS - 1);
  const hi = Math.min(Math.floor((Math.max(to, from + 1) - 1) / CHUNK), CHUNKS - 1, lo + MAX_WATCH - 1);
  return [lo, hi];
}

/** A page's set, checked and rate limited (`r`: {sec, sets}); the chunk and
 * the box in it, or null. Past the limit the page gets `slow`. */
function setFor(v, r, send) {
  const i = v.i;
  if (!Number.isInteger(i) || i < 0 || i >= CHUNK * CHUNKS || typeof v.v !== 'boolean') return null;
  const now = Math.floor(Date.now() / 1000);
  if (r.sec !== now) (r.sec = now), (r.sets = 0);
  if (++r.sets > MAX_SETS_PER_S) {
    send(JSON.stringify({ t: 'slow', i }));
    return null;
  }
  return [Math.floor(i / CHUNK), i % CHUNK, v.v];
}

const chunkOf = (env, n) => env.CHUNK.get(env.CHUNK.idFromName(String(n)));
const parse = (msg) => {
  try {
    return JSON.parse(msg);
  } catch {
    return {};
  }
};
const closeBack = (ws, code) => {
  try {
    ws.close(code === 1005 || code === 1006 ? 1000 : code);
  } catch {}
};

/** One per 15,625 boxes (`0` to `63`): the bits, in memory and in its
 * SQLite, and who's watching (Hubs, Viewers, or Worker-held sockets). */
export class Chunk extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec('CREATE TABLE IF NOT EXISTS on_ (i INTEGER PRIMARY KEY)');
    this.sql.exec('CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v INTEGER)');
    this.n = this.sql.exec("SELECT v FROM meta WHERE k = 'n'").toArray()[0]?.v ?? 0;
    this.bits = new Uint8Array(Math.ceil(CHUNK / 8));
    this.checked = 0;
    for (const { i } of this.sql.exec('SELECT i FROM on_')) {
      this.bits[i >> 3] |= 1 << (i & 7);
      this.checked++;
    }
    /** Hubs and Viewers watching, by id: until when, a stub, and if a Hub.
     * Who (not until when) is on disk too: an idle chunk is evicted after
     * 10 s on workerd, and its watchers' next renewal may be 20 s away. */
    this.sql.exec('CREATE TABLE IF NOT EXISTS subs (id TEXT PRIMARY KEY, hub INTEGER)');
    this.subs = new Map();
    const until = Date.now() + SUB_MS;
    for (const { id, hub } of this.sql.exec('SELECT id, hub FROM subs')) this.watch(id, !!hub, until);
    this.pruned = 0;
    /** Every box, as a new watcher gets it: built again only after a change. */
    this.all = null;
    /** Changes since the last beat, for the watchers (and the disk) at the next. */
    this.pending = [];
    this.beat = null;
    this.told = 0;
    // The total hears from a chunk with boxes checked as soon as it wakes.
    this.dirty = this.checked > 0;
    if (this.dirty) this.arm();
  }

  /** Which chunk it is: the viewers say, as in million. */
  learn(n) {
    if (Number.isInteger(n) && n !== this.n) {
      this.n = n;
      this.all = null;
      this.sql.exec("INSERT INTO meta (k, v) VALUES ('n', ?) ON CONFLICT (k) DO UPDATE SET v = excluded.v", n);
    }
  }

  snapshot() {
    return (this.all ??= allMsg(this.n, this.bits));
  }

  /** Watch (or keep watching), over RPC. A Viewer gets every box the first
   * time (as `all`, which it sends on); a Hub gets the bits, the first time
   * or when it asks (it builds `all` for its pages from its own copy). */
  sub(id, n, hub = false, want = false) {
    this.learn(n);
    const now = Date.now();
    if (now - this.pruned >= PRUNE_MS) {
      for (const [k, s] of this.subs) if (s.until <= now) this.unsub(k);
      this.pruned = now;
    }
    const s = this.subs.get(id);
    if (s) s.until = now + SUB_MS;
    else {
      this.watch(id, hub, now + SUB_MS);
      this.sql.exec('INSERT OR REPLACE INTO subs (id, hub) VALUES (?, ?)', id, hub ? 1 : 0);
    }
    if (s && !want) return null;
    return hub ? this.bits.slice() : this.snapshot();
  }

  watch(id, hub, until) {
    const ns = hub ? this.env.HUB : this.env.VIEWER;
    this.subs.set(id, { until, stub: ns.get(ns.idFromString(id)), hub });
  }

  unsub(id) {
    if (this.subs.delete(id)) this.sql.exec('DELETE FROM subs WHERE id = ?', id);
  }

  /** Watch over a socket (SESSIONS = "worker"): every box, then each change. */
  async fetch(req) {
    if (req.headers.get('upgrade') !== 'websocket') return Response.json({ error: 'not found' }, { status: 404 });
    this.learn(Number(new URL(req.url).searchParams.get('n')));
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server);
    server.send(this.snapshot());
    return new Response(null, { status: 101, webSocket: client });
  }

  webSocketMessage(ws, msg) {
    const v = parse(msg);
    if (v.t === 'set') this.set(v.i, v.v);
  }

  webSocketClose(ws, code) {
    closeBack(ws, code);
  }

  /** `on` is what the box should be, not a toggle: two people clicking at
   * once both get what they clicked. */
  set(i, on, n) {
    this.learn(n);
    if (!Number.isInteger(i) || i < 0 || i >= CHUNK || typeof on !== 'boolean') return;
    const bit = 1 << (i & 7);
    if (((this.bits[i >> 3] & bit) !== 0) === on) return;
    this.bits[i >> 3] ^= bit;
    this.all = null;
    this.checked += on ? 1 : -1;
    // To the disk and the watchers at the next beat.
    this.pending.push([i, on]);
    this.dirty = true;
    this.arm();
  }

  arm() {
    this.beat ??= setTimeout(() => this.tick(), BEAT_MS);
  }

  tick() {
    this.beat = null;
    const sets = this.pending;
    this.pending = [];
    if (sets.length) {
      // A beat's changes in one transaction (million's host commits a batch
      // of waiting requests in one too). Nobody has seen them yet: what the
      // beat sends waits for the write (the output gate).
      this.ctx.storage.transactionSync(() => {
        for (const [i, on] of sets) this.sql.exec(on ? 'INSERT OR IGNORE INTO on_ (i) VALUES (?)' : 'DELETE FROM on_ WHERE i = ?', i);
      });
      const msg = JSON.stringify(sets.length === 1 ? { t: 'set', n: this.n, i: sets[0][0], v: sets[0][1] } : { t: 'sets', n: this.n, c: sets });
      // One RPC per watching object: there's no sending to sockets another
      // object holds.
      for (const s of this.subs.values()) (s.hub ? s.stub.push(msg, this.n, sets) : s.stub.push(msg)).catch(() => {});
      for (const ws of this.ctx.getWebSockets()) {
        try {
          ws.send(msg);
        } catch {}
      }
    }
    const now = Date.now();
    if (this.dirty && now - this.told >= TOLD_MS) {
      this.env.BOXES.get(this.env.BOXES.idFromName('all')).count(this.n, this.checked).catch(() => {});
      this.dirty = false;
      this.told = now;
    }
    if (this.dirty) this.arm();
  }
}

/** One (`all`): each chunk's count, for the total. */
export class Boxes extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec('CREATE TABLE IF NOT EXISTS counts (n INTEGER PRIMARY KEY, checked INTEGER)');
    this.counts = new Map(this.sql.exec('SELECT n, checked FROM counts').toArray().map((r) => [r.n, r.checked]));
  }

  count(n, checked) {
    if (!Number.isInteger(n) || n < 0 || n >= CHUNKS || !Number.isInteger(checked)) return;
    this.sql.exec('INSERT INTO counts (n, checked) VALUES (?, ?) ON CONFLICT (n) DO UPDATE SET checked = excluded.checked', n, checked);
    this.counts.set(n, checked);
  }

  total() {
    let sum = 0;
    for (const c of this.counts.values()) sum += c;
    return sum;
  }
}

/** SESSIONS = "hub": one of HUBS, each holding many pages' sockets
 * (hibernatable), as one combd node holds its viewers' sockets. It watches
 * a chunk once for all its pages that show it, keeps a copy of its bits for
 * pages that arrive, and passes each beat's changes to the pages watching.
 * A page's attachment is what it watches and until when; the rest is
 * rebuilt from those if the Hub hibernated. */
export class Hub extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    /** Chunk number to: its bits (null until they come), `all` built from
     * them, the pages watching, pages waiting for the bits, changes that
     * came before them, and when the Hub last renewed. */
    this.chunks = new Map();
    /** Each page's rate (in memory: a Hub that slept has seen no sets for 10 s). */
    this.rates = new WeakMap();
    this.pruned = Date.now();
    for (const ws of ctx.getWebSockets()) {
      const a = ws.deserializeAttachment();
      if (a) for (let n = a.lo; n <= a.hi; n++) this.chunk(n).socks.add(ws);
    }
  }

  chunk(n) {
    let c = this.chunks.get(n);
    if (!c) this.chunks.set(n, (c = { bits: null, all: null, socks: new Set(), waiting: [], early: [], renewed: 0, asking: false }));
    return c;
  }

  fetch() {
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws, msg) {
    const v = parse(msg);
    if (v.t === 'range') this.range(ws, v);
    else if (v.t === 'set') {
      let r = this.rates.get(ws);
      if (!r) this.rates.set(ws, (r = { sec: 0, sets: 0 }));
      const set = setFor(v, r, (m) => ws.send(m));
      if (set) await chunkOf(this.env, set[0]).set(set[1], set[2], set[0]);
    }
  }

  /** What a page shows (and, every 20 s, that it's still there). */
  range(ws, v) {
    const now = Date.now();
    const [lo, hi] = chunksFor(v);
    const was = ws.deserializeAttachment() ?? { lo: 1, hi: 0 };
    ws.serializeAttachment({ lo, hi, until: now + SUB_MS });
    for (let n = was.lo; n <= was.hi; n++) if (n < lo || n > hi) this.leave(ws, n);
    for (let n = lo; n <= hi; n++) {
      const c = this.chunk(n);
      if (!c.socks.has(ws) && !c.waiting.includes(ws)) {
        if (c.bits) {
          ws.send((c.all ??= allMsg(n, c.bits)));
          c.socks.add(ws);
        } else c.waiting.push(ws);
      }
      // The Hub's own subscription, renewed as its pages renew theirs.
      if (!c.asking && (!c.bits || now - c.renewed >= RENEW_MS)) this.ask(n, c, !c.bits);
    }
    if (now - this.pruned >= PRUNE_MS) this.prune(now);
  }

  async ask(n, c, want) {
    c.asking = true;
    c.renewed = Date.now();
    try {
      const bits = await chunkOf(this.env, n).sub(this.ctx.id.toString(), n, true, want);
      if (bits) {
        c.bits = bits;
        for (const sets of c.early) this.apply(n, c, sets);
        c.all = null;
      }
    } catch {
      c.renewed = 0; // asked again at the next renewal
    }
    c.early = [];
    c.asking = false;
    if (!c.bits) return;
    if (c.waiting.length) {
      c.all ??= allMsg(n, c.bits);
      for (const ws of c.waiting) {
        try {
          ws.send(c.all);
          c.socks.add(ws);
        } catch {}
      }
      c.waiting = [];
    }
  }

  apply(n, c, sets) {
    for (const [i, on] of sets) {
      if (on) c.bits[i >> 3] |= 1 << (i & 7);
      else c.bits[i >> 3] &= ~(1 << (i & 7));
    }
    c.all = null;
  }

  /** A chunk's beat: to every page here that watches it. */
  push(msg, n, sets) {
    const c = this.chunks.get(n);
    if (!c) return;
    if (c.bits) this.apply(n, c, sets);
    else c.early.push(sets);
    for (const ws of c.socks) {
      try {
        ws.send(msg);
      } catch {}
    }
  }

  leave(ws, n) {
    const c = this.chunks.get(n);
    if (!c) return;
    c.socks.delete(ws);
    c.waiting = c.waiting.filter((w) => w !== ws);
    if (!c.socks.size && !c.waiting.length) {
      this.chunks.delete(n);
      chunkOf(this.env, n).unsub(this.ctx.id.toString()).catch(() => {});
    }
  }

  /** Pages that stopped renewing are forgotten, as a chunk forgets viewers. */
  prune(now) {
    this.pruned = now;
    for (const ws of this.ctx.getWebSockets()) {
      const a = ws.deserializeAttachment();
      if (a && a.until <= now) {
        for (let n = a.lo; n <= a.hi; n++) this.leave(ws, n);
        ws.serializeAttachment(null);
      }
    }
  }

  webSocketClose(ws, code) {
    const a = ws.deserializeAttachment();
    if (a) for (let n = a.lo; n <= a.hi; n++) this.leave(ws, n);
    closeBack(ws, code);
  }

  webSocketError(ws) {
    this.webSocketClose(ws, 1011);
  }
}

/** SESSIONS = "object": one per page socket, as million's viewer: what its
 * page looks at, and its sets. Idle, it hibernates; all it keeps is the
 * socket's attachment (the chunks it watches, and its rate). */
export class Viewer extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.s = { lo: 1, hi: 0, sec: 0, sets: 0, ...ctx.getWebSockets()[0]?.deserializeAttachment() };
  }

  fetch() {
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws, msg) {
    const v = parse(msg);
    const s = this.s;
    const id = this.ctx.id.toString();
    if (v.t === 'range') {
      // Sent again every 20 s: that renews the subscriptions.
      const [lo, hi] = chunksFor(v);
      const calls = [];
      for (let n = s.lo; n <= s.hi; n++) if (n < lo || n > hi) calls.push(chunkOf(this.env, n).unsub(id));
      for (let n = lo; n <= hi; n++) calls.push(chunkOf(this.env, n).sub(id, n).then((all) => all && ws.send(all)));
      [s.lo, s.hi] = [lo, hi];
      ws.serializeAttachment(s);
      await Promise.allSettled(calls);
    } else if (v.t === 'set') {
      const set = setFor(v, s, (m) => ws.send(m));
      ws.serializeAttachment(s);
      if (set) await chunkOf(this.env, set[0]).set(set[1], set[2], set[0]);
    }
  }

  /** A chunk's change, for this page. */
  push(msg) {
    for (const ws of this.ctx.getWebSockets()) {
      try {
        ws.send(msg);
      } catch {}
    }
  }

  // A closed socket's viewer just ends: its chunks forget it when its
  // subscriptions expire (60 s), as million's `on_close = "drop"`.
  webSocketClose(ws, code) {
    closeBack(ws, code);
  }
}

/** SESSIONS = "worker": the page's socket, held by this Worker invocation
 * (kept going with waitUntil until the page leaves: celld closes a Worker's
 * outgoing sockets when its event ends). */
function workerSession(env, ctx) {
  const [client, server] = Object.values(new WebSocketPair());
  server.accept();
  /** Chunk number to its socket (a promise, while it opens). */
  const watching = new Map();
  const rate = { sec: 0, sets: 0 };
  const watch = (n) => {
    const p = chunkOf(env, n)
      .fetch(`http://chunk/?n=${n}`, { headers: { upgrade: 'websocket' } })
      .then((r) => {
        const ws = r.webSocket;
        if (!ws) throw new Error(`chunk ${n}: no socket`);
        ws.accept();
        ws.addEventListener('message', (e) => {
          try {
            server.send(e.data);
          } catch {}
        });
        // A chunk that went away: the page's next renewal watches it again.
        ws.addEventListener('close', gone);
        ws.addEventListener('error', gone);
        return ws;
      });
    const gone = () => watching.get(n) === p && watching.delete(n);
    p.catch(gone);
    watching.set(n, p);
  };
  const unwatch = (n) => {
    const p = watching.get(n);
    watching.delete(n);
    p?.then((ws) => ws.close(1000)).catch(() => {});
  };
  server.addEventListener('message', (e) => {
    const v = parse(e.data);
    if (v.t === 'range') {
      // The chunks' sockets stay open, so a renewal with nothing new is free.
      const [lo, hi] = chunksFor(v);
      for (const n of [...watching.keys()]) if (n < lo || n > hi) unwatch(n);
      for (let n = lo; n <= hi; n++) if (!watching.has(n)) watch(n);
    } else if (v.t === 'set') {
      const set = setFor(v, rate, (m) => server.send(m));
      if (!set) return;
      const [n, i, on] = set;
      const p = watching.get(n);
      if (p) p.then((ws) => ws.send(JSON.stringify({ t: 'set', i, v: on }))).catch(() => {});
      else chunkOf(env, n).set(i, on, n).catch(() => {});
    }
  });
  ctx.waitUntil(
    new Promise((done) =>
      server.addEventListener('close', () => {
        for (const n of [...watching.keys()]) unwatch(n);
        try {
          server.close(1000);
        } catch {}
        done();
      }),
    ),
  );
  return new Response(null, { status: 101, webSocket: client });
}

export default {
  async fetch(req, env, ctx) {
    const path = new URL(req.url).pathname;
    if (path === '/ws' && req.headers.get('upgrade') === 'websocket') {
      if (env.SESSIONS === 'worker') return workerSession(env, ctx);
      if (env.SESSIONS === 'object') return env.VIEWER.get(env.VIEWER.newUniqueId()).fetch(req);
      return env.HUB.get(env.HUB.idFromName(String(Math.floor(Math.random() * HUBS)))).fetch(req);
    }
    if (req.method === 'GET' && path === '/') return new Response(INDEX, { headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-cache' } });
    if (req.method === 'GET' && path === '/api/count') {
      const checked = await env.BOXES.get(env.BOXES.idFromName('all')).total();
      return Response.json({ checked });
    }
    return Response.json({ error: 'not found' }, { status: 404 });
  },
};

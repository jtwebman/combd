import worker from './worker.mjs';
Bun.serve({ port: 8080, hostname: '0.0.0.0', fetch: worker.fetch });

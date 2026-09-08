import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import i18next from 'i18next';
import { loadResources } from './resources.mjs';
import { routes, translator, render } from './src/app.mjs';

export async function startServer({ root, port = 0 } = {}) {
  const candidate = await loadResources(root);
  const assets = new Map(await Promise.all([
    ['/app.mjs', './src/app.mjs'], ['/client.mjs', './src/client.mjs'],
    ['/i18next.js', './node_modules/i18next/dist/umd/i18next.min.js'],
  ].map(async ([route, file]) => [route, await readFile(new URL(file, import.meta.url))])));
  const server = http.createServer(async (request, response) => {
    response.setHeader('Cache-Control', 'no-store');
    response.setHeader('X-Content-Type-Options', 'nosniff');
    try {
      const path = new URL(request.url, 'http://localhost').pathname;
      if (request.method !== 'GET') { response.writeHead(405); response.end('Method not allowed'); return; }
      if (assets.has(path)) { response.setHeader('Content-Type', 'text/javascript'); response.end(assets.get(path)); return; }
      if (path === '/resources.json') { response.setHeader('Content-Type', 'application/json'); response.end(JSON.stringify(candidate)); return; }
      if (!routes.includes(path)) { response.writeHead(404); response.end('Not found'); return; }
      const locale = path.split('/')[1];
      const instance = await translator(i18next, candidate.resources, locale);
      response.setHeader('Content-Type', 'text/html; charset=utf-8');
      response.end(`<!doctype html><html lang="${locale}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="icon" href="data:,"><title>JSON consumer fixture</title></head><body><main>${render(instance, path)}</main><script src="/i18next.js"></script><script type="module" src="/client.mjs"></script></body></html>`);
    } catch (error) {
      console.error(error);
      response.writeHead(500); response.end('Consumer rendering failed');
    }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(port, '127.0.0.1', resolve); });
  return { url: `http://127.0.0.1:${server.address().port}`, hashes: candidate.hashes,
    close: () => new Promise((resolve, reject) => { server.close(error => error ? reject(error) : resolve()); server.closeAllConnections(); }) };
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const running = await startServer({ port: Number(process.env.PORT || 4173) });
  console.log(JSON.stringify({ url: running.url, hashes: running.hashes }));
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, async () => { await running.close(); });
}

#!/usr/bin/env node
// Static server for wasm/dist with the headers the WebAssembly build needs.
//
//   node wasm/serve.mjs [--port 8080] [--dir wasm/dist]
//
// - Cross-Origin-Opener-Policy / Cross-Origin-Embedder-Policy: the libgphoto2
//   module uses pthreads, i.e. SharedArrayBuffer, which browsers only enable on
//   cross-origin isolated pages.
// - Bound to 127.0.0.1: http://localhost is a secure context, which getUserMedia
//   and WebUSB require. Anything else needs https.
// No dependencies, Node 18+.

import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, join, normalize, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};
const here = fileURLToPath(new URL('.', import.meta.url));
const root = resolve(option('--dir', join(here, 'dist')));
const port = Number(option('--port', process.env.PORT ?? 8080));

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.jpg': 'image/jpeg',
  '.png': 'image/png',
  '.ts': 'text/plain; charset=utf-8',
};

const server = createServer(async (req, res) => {
  res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
  res.setHeader('Cache-Control', 'no-store');

  const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  let file = normalize(join(root, pathname));
  if (file !== root && !file.startsWith(root + sep)) {
    res.writeHead(403).end('forbidden');
    return;
  }
  try {
    if ((await stat(file)).isDirectory()) file = join(file, 'index.html');
    const body = await readFile(file);
    res.writeHead(200, { 'Content-Type': TYPES[extname(file)] ?? 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404, { 'Content-Type': 'text/plain' }).end('not found');
  }
});

server.listen(port, '127.0.0.1', () => {
  console.log(`toucan-camera-server (wasm) test page: http://localhost:${port}/`);
  console.log(`serving ${root}`);
});

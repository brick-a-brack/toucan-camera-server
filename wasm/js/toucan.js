// Entry point of the WebAssembly build: loads the module, wires the bridges,
// and wraps the in-page server into a fetch()-compatible function.
//
//   import { createServer } from './toucan.js';
//   const toucan = await createServer();
//   const cameras = await (await toucan.fetch('/cameras')).json();
//
// `toucan.fetch` speaks the native server's HTTP API (same paths, bodies, status
// codes), including the MJPEG live-view stream, so code written for the server
// runs against the browser build by swapping `fetch` for `toucan.fetch`.
// `toucan.server` is the typed API (listCameras(), connect(id), …).

import init, { ToucanServer, version } from './pkg/toucan_camera_wasm.js';
import { WebcamBridge } from './webcam-bridge.js';
import { GPhoto2Bridge, GPhoto2Error } from './gphoto2-bridge.js';

export { WebcamBridge, GPhoto2Bridge, GPhoto2Error, ToucanServer };

const BOUNDARY = 'frame';

/**
 * @param {object} [options]
 * @param {boolean} [options.webcam] enable the getUserMedia backend (default: when supported)
 * @param {boolean} [options.gphoto2] enable the WebUSB/libgphoto2 backend (default: when supported)
 * @param {string|URL} [options.gphoto2ModuleUrl] location of toucan_gphoto2.mjs
 * @param {number} [options.liveViewMinIntervalMs] floor between two streamed frames (default 33)
 */
export async function createServer(options = {}) {
  await init();
  const support = {
    webcam: WebcamBridge.support(),
    gphoto2: GPhoto2Bridge.support(),
  };
  const bridges = {};
  if (options.webcam !== false && support.webcam.ok) bridges.webcam = new WebcamBridge();
  if (options.gphoto2 !== false && support.gphoto2.ok) {
    bridges.gphoto2 = new GPhoto2Bridge({ moduleUrl: options.gphoto2ModuleUrl });
  }
  const server = new ToucanServer(bridges);
  return new Toucan(server, bridges, support, options.liveViewMinIntervalMs ?? 33);
}

class Toucan {
  constructor(server, bridges, support, minInterval) {
    this.server = server;
    this.bridges = bridges;
    this.support = support;
    this.version = version();
    this.#minInterval = minInterval;
  }

  #minInterval;

  /**
   * fetch() against the in-page server. Accepts a path ('/cameras'), a URL, or
   * a Request; only the path and query are used.
   * @returns {Promise<Response>}
   */
  async fetch(input, init = {}) {
    const request = input instanceof Request ? input : null;
    const url = new URL(request ? request.url : String(input), 'http://toucan.invalid');
    const method = (init.method ?? request?.method ?? 'GET').toUpperCase();
    let body = init.body ?? (request && method !== 'GET' ? await request.text() : undefined);
    if (body !== undefined && typeof body !== 'string') body = await new Response(body).text();

    const res = await this.server.request(method, url.pathname + url.search, body || undefined);
    if (res.liveView) {
      return new Response(this.#liveViewStream(res.liveView, init.signal ?? request?.signal), {
        status: 200,
        headers: { 'Content-Type': `multipart/x-mixed-replace; boundary=${BOUNDARY}` },
      });
    }
    return new Response(res.status === 204 || res.body.length === 0 ? null : res.body, {
      status: res.status,
      headers: { 'Content-Type': res.contentType },
    });
  }

  /**
   * Live view as an async iterator of JPEG frames (Uint8Array) — handier than
   * the MJPEG stream inside a page. Stops when `signal` aborts or the camera is
   * disconnected.
   */
  async *frames(id, signal) {
    while (!signal?.aborted) {
      const started = performance.now();
      try {
        yield await this.server.liveViewFrame(id);
      } catch (err) {
        if (err?.status === 409 || err?.status === 404 || err?.status === 400) return;
        // "Not ready yet" and transient errors: keep polling, like the server.
        await sleep(100);
        continue;
      }
      const elapsed = performance.now() - started;
      if (elapsed < this.#minInterval) await sleep(this.#minInterval - elapsed);
    }
  }

  // The server's MJPEG framing: --frame, part headers, JPEG, CRLF; a closing
  // boundary when the stream ends.
  #liveViewStream(id, signal) {
    const encoder = new TextEncoder();
    const frames = this.frames(id, signal);
    return new ReadableStream({
      async pull(controller) {
        const { value, done } = await frames.next();
        if (done) {
          controller.enqueue(encoder.encode(`--${BOUNDARY}--\r\n`));
          controller.close();
          return;
        }
        controller.enqueue(
          encoder.encode(`--${BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: ${value.length}\r\n\r\n`),
        );
        controller.enqueue(value);
        controller.enqueue(encoder.encode('\r\n'));
      },
      cancel() {
        frames.return();
      },
    });
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

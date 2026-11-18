// libgphoto2 over WebUSB: the JS side of the `gphoto2` backend (src/gphoto2.rs).
//
// Loads the Emscripten build of libgphoto2 (gphoto2/toucan_gphoto2.mjs, built
// by wasm/gphoto2/build.sh) and exposes the promise API the Rust backend calls.
// Every value crossing over is plain data: device lists, config trees, bytes.
//
// Requirements: a Chromium-based browser (WebUSB), a secure context (https or
// localhost), and cross-origin isolation (COOP/COEP headers) because the module
// is built with pthreads and needs SharedArrayBuffer.

const PTP_CLASS = 6; // USB still-image class
const PTP_SUBCLASS = 1;

/** A libgphoto2 failure: `code` is libgphoto2's result code (GP_ERROR = -1…). */
export class GPhoto2Error extends Error {
  constructor(message, code) {
    super(message);
    this.name = 'GPhoto2Error';
    this.code = code;
  }
}

export class GPhoto2Bridge {
  #moduleUrl;
  #module = null;
  #sessions = new Map(); // port -> Session
  #queue = Promise.resolve();
  #keepAlive = null;

  /**
   * @param {object} [options]
   * @param {string|URL} [options.moduleUrl] where toucan_gphoto2.mjs is served
   * @param {number} [options.keepAliveMs] idle ping period; some bodies (Canon)
   *   sleep after their auto-power-off delay and vanish mid-session otherwise —
   *   the native backend's keep-alive thread, in a timer.
   */
  constructor({ moduleUrl, keepAliveMs = 30_000 } = {}) {
    this.#moduleUrl = moduleUrl ?? new URL('./gphoto2/toucan_gphoto2.mjs', import.meta.url);
    if (keepAliveMs > 0) {
      this.#keepAlive = setInterval(() => this.#pingAll(), keepAliveMs);
    }
  }

  /** Whether this page can use the backend at all, and if not, why. */
  static support() {
    if (!('usb' in navigator)) return { ok: false, reason: 'WebUSB is not available (use Chrome or Edge)' };
    if (!globalThis.crossOriginIsolated) {
      return { ok: false, reason: 'the page is not cross-origin isolated (serve it with COOP/COEP headers)' };
    }
    return { ok: true };
  }

  /**
   * Shows the browser's USB picker for still cameras. Must be called from a user
   * gesture (a click). The granted camera then appears in `listDevices()`.
   */
  static async requestDevice() {
    return navigator.usb.requestDevice({
      filters: [{ classCode: PTP_CLASS, subclassCode: PTP_SUBCLASS }],
    });
  }

  /** The USB devices this page holds a permission for. */
  static async grantedDevices() {
    return 'usb' in navigator ? navigator.usb.getDevices() : [];
  }

  /** libgphoto2's version string (loads the module). */
  version() {
    return this.#run((m) => m.libgphoto2Version());
  }

  /**
   * What this libgphoto2 build can drive: `{ version, preloaded, models, ports }`.
   * `models` is 0 when plugin discovery failed.
   */
  diagnostics() {
    return this.#run((m) => m.diagnostics());
  }

  /**
   * Prints libgphoto2's own log to the console (what `gphoto2 --debug` shows).
   * `verbose` adds the raw USB/PTP data.
   */
  setDebugLog(verbose = false) {
    return this.#run((m) => m.enableDebugLog(verbose));
  }

  // --- API used by the Rust backend -----------------------------------------

  listDevices() {
    return this.#run(async (m) => check(await m.listCameras()));
  }

  open(port, model) {
    return this.#run(async (m) => {
      if (this.#sessions.has(port)) return;
      const session = new m.Session(model, port);
      try {
        check(await session.open());
      } catch (err) {
        session.delete();
        throw err;
      }
      this.#sessions.set(port, session);
    });
  }

  close(port) {
    return this.#run(async () => {
      const session = this.#sessions.get(port);
      if (!session) return;
      this.#sessions.delete(port);
      // Close before delete(): gp_camera_exit talks to the body, which can only
      // happen in an async call — the C++ destructor must not do I/O.
      await session.close();
      session.delete();
    });
  }

  config(port) {
    return this.#withSession(port, async (s) => check(await s.config()));
  }

  widget(port, name) {
    return this.#withSession(port, async (s) => check(await s.widget(name)));
  }

  setConfig(port, name, value) {
    return this.#withSession(port, async (s) => {
      check(await s.setConfig(name, String(value)));
      await s.consumeEvents();
    });
  }

  capturePreview(port) {
    return this.#withSession(port, async (s) => check(await s.capturePreview()));
  }

  captureImage(port) {
    return this.#withSession(port, async (s) => {
      const file = check(await s.captureImage());
      await s.consumeEvents();
      return file;
    });
  }

  /** Stops the keep-alive timer and closes every session. */
  async dispose() {
    clearInterval(this.#keepAlive);
    for (const port of [...this.#sessions.keys()]) await this.close(port);
  }

  // --- internals --------------------------------------------------------------

  #load() {
    if (!this.#module) {
      this.#module = import(String(this.#moduleUrl)).then((ns) => ns.default());
    }
    return this.#module;
  }

  // libgphoto2 runs under Asyncify, which can only have one suspended call at
  // a time across the whole module — and entering the module at all while one
  // is suspended (even a synchronous getter) corrupts it and the pending call
  // never resolves. So EVERY call into the module goes through this queue.
  #run(op) {
    const result = this.#queue.then(async () => op(await this.#load()));
    this.#queue = result.catch(() => {});
    return result;
  }

  #withSession(port, op) {
    return this.#run(async () => {
      const session = this.#sessions.get(port);
      if (!session) throw new GPhoto2Error(`camera ${port} is not open`, -1);
      return op(session);
    });
  }

  #pingAll() {
    for (const port of this.#sessions.keys()) {
      this.#withSession(port, (s) => s.consumeEvents()).catch(() => {});
    }
  }
}

// The C++ side returns `{ error, code }` instead of throwing.
function check(result) {
  if (result && typeof result === 'object' && typeof result.error === 'string' && 'code' in result) {
    throw new GPhoto2Error(result.error, result.code);
  }
  return result;
}

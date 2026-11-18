// Webcams through getUserMedia: the JS side of the `webcam-browser` backend
// (src/webcam.rs).
//
// Owns the browser objects (MediaStream, a hidden <video> per camera, a canvas
// to grab frames from) and hands the Rust side plain data: the device list, the
// track's capabilities/settings, and JPEG bytes.

const PTZ = { pan: true, tilt: true, zoom: true };

export class WebcamBridge {
  #open = new Map(); // deviceId -> { stream, track, video, canvas }
  #idealWidth;
  #idealHeight;

  /**
   * @param {object} [options]
   * @param {number} [options.width] preferred stream width (default 1920)
   * @param {number} [options.height] preferred stream height (default 1080)
   */
  constructor({ width = 1920, height = 1080 } = {}) {
    this.#idealWidth = width;
    this.#idealHeight = height;
  }

  static support() {
    if (!navigator.mediaDevices?.getUserMedia) {
      return { ok: false, reason: 'getUserMedia is not available (it needs https or localhost)' };
    }
    return { ok: true };
  }

  /**
   * Asks for camera permission once. Until it is granted the browser lists no
   * webcam (or lists them without label or id), so call this from a click.
   */
  static async requestPermission() {
    const stream = await navigator.mediaDevices.getUserMedia({ video: true });
    stream.getTracks().forEach((t) => t.stop());
  }

  // --- API used by the Rust backend -----------------------------------------

  async listDevices() {
    const devices = await navigator.mediaDevices.enumerateDevices();
    return devices
      .filter((d) => d.kind === 'videoinput' && d.deviceId)
      .map((d) => ({ deviceId: d.deviceId, label: d.label }));
  }

  async open(deviceId) {
    if (this.#open.has(deviceId)) return;
    const base = {
      deviceId: { exact: deviceId },
      width: { ideal: this.#idealWidth },
      height: { ideal: this.#idealHeight },
    };
    let stream;
    try {
      // Asking for pan/tilt/zoom up front is what makes Chrome grant PTZ control.
      stream = await navigator.mediaDevices.getUserMedia({ video: { ...base, ...PTZ } });
    } catch (err) {
      if (err?.name !== 'OverconstrainedError' && err?.name !== 'TypeError') throw err;
      stream = await navigator.mediaDevices.getUserMedia({ video: base });
    }
    const [track] = stream.getVideoTracks();
    const video = document.createElement('video');
    video.muted = true;
    video.playsInline = true;
    video.srcObject = stream;
    await video.play();
    await firstFrame(video);
    // The constraints the stream was opened with: every later applyConstraints()
    // call has to restate them, since it replaces the whole set.
    const { width, height } = track.getSettings();
    this.#open.set(deviceId, {
      stream,
      track,
      video,
      base: { deviceId: { exact: deviceId }, width: { ideal: width }, height: { ideal: height } },
      canvas: document.createElement('canvas'),
    });
  }

  async close(deviceId) {
    const cam = this.#open.get(deviceId);
    if (!cam) return;
    this.#open.delete(deviceId);
    cam.stream.getTracks().forEach((t) => t.stop());
    cam.video.srcObject = null;
  }

  async capabilities(deviceId) {
    const { track } = this.#get(deviceId);
    // Plain JSON copies: the Rust side deserialises them.
    return {
      capabilities: plain(track.getCapabilities?.() ?? {}),
      settings: plain(track.getSettings()),
    };
  }

  /**
   * Applies one parameter write. Image-capture constraints (exposure, focus,
   * zoom…) only take effect as `advanced` constraints, which the browser skips
   * silently when the camera cannot honour them — so the result is read back
   * from getSettings(), and a value that did not stick is an error rather than
   * a write that looked successful.
   */
  async applyConstraints(deviceId, constraints) {
    const cam = this.#get(deviceId);
    if ('width' in constraints || 'height' in constraints) {
      cam.base = { ...cam.base, width: { ideal: constraints.width }, height: { ideal: constraints.height } };
      await cam.track.applyConstraints(cam.base);
      const { width, height } = cam.track.getSettings();
      await resized(cam.video, width, height);
      return;
    }
    await cam.track.applyConstraints({ ...cam.base, advanced: [constraints] });
    const settings = cam.track.getSettings();
    const steps = cam.track.getCapabilities?.() ?? {};
    for (const [name, wanted] of Object.entries(constraints)) {
      const got = settings[name];
      const tolerance = typeof wanted === 'number' ? Math.max(steps[name]?.step ?? 0, 1e-6) / 2 + 1e-9 : 0;
      const ok = typeof wanted === 'number' ? Math.abs(got - wanted) <= tolerance : got === wanted;
      if (!ok) throw new Error(`the camera did not accept ${name} = ${JSON.stringify(wanted)} (it reports ${JSON.stringify(got)})`);
    }
  }

  async grabFrame(deviceId, quality = 0.8) {
    const cam = this.#get(deviceId);
    const { video, canvas } = cam;
    if (!video.videoWidth) await firstFrame(video);
    canvas.width = video.videoWidth;
    canvas.height = video.videoHeight;
    canvas.getContext('2d').drawImage(video, 0, 0);
    const blob = await new Promise((resolve, reject) =>
      canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('JPEG encoding failed'))), 'image/jpeg', quality),
    );
    return new Uint8Array(await blob.arrayBuffer());
  }

  /**
   * A still at the stream's resolution, encoded at high quality. (ImageCapture's
   * takePhoto() would reconfigure the stream to the sensor's photo mode, which
   * restarts it on most webcams — too disruptive between two live-view frames.)
   */
  async takePhoto(deviceId) {
    return this.grabFrame(deviceId, 0.95);
  }

  #get(deviceId) {
    const cam = this.#open.get(deviceId);
    if (!cam) throw new Error(`webcam ${deviceId} is not open`);
    if (cam.track.readyState === 'ended') throw new Error('the webcam was disconnected');
    return cam;
  }
}

function plain(value) {
  return JSON.parse(JSON.stringify(value));
}

// Waits until the <video> shows frames of the new size (a resize restarts the
// stream; grabbing before that would hand out a stale-size frame).
function resized(video, width, height) {
  if (video.videoWidth === width && video.videoHeight === height) return Promise.resolve();
  return new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      video.removeEventListener('resize', onResize);
      resolve();
    };
    const onResize = () => {
      if (video.videoWidth === width && video.videoHeight === height) finish();
    };
    const timer = setTimeout(finish, 3000); // give up waiting, never fail the write
    video.addEventListener('resize', onResize);
  });
}

function firstFrame(video) {
  if (video.videoWidth && video.readyState >= 2) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('the webcam produced no frame')), 5000);
    const done = () => {
      if (!video.videoWidth) return;
      clearTimeout(timer);
      video.removeEventListener('loadeddata', done);
      video.removeEventListener('resize', done);
      resolve();
    };
    video.addEventListener('loadeddata', done);
    video.addEventListener('resize', done);
  });
}

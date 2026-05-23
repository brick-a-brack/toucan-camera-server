# Remote Camera Protocol (Stop Motion Studio) — API reference

This document describes the HTTP API a camera server must expose to be controlled by
**Stop Motion Studio**'s "Remote Camera" feature. It was reverse-engineered from a Charles
proxy capture of the iOS app (`User-Agent: Stop Motion Studio/8858 …`) talking to a remote
camera server.

It is provided as a reference target: implementing these endpoints lets
`toucan-camera-server` masquerade as a Stop Motion Studio remote camera.

## Conventions

| | |
|---|---|
| **Base URL** | `http://<host>:2222` (port `2222` in the capture) |
| **Transport** | Plain HTTP/1.1, `keep-alive`. No TLS. |
| **Authentication** | **None.** No `Authorization` header, no token. |
| **HTTP method** | **`POST` for every endpoint.** |
| **Parameters** | Passed in the **query string**. The request body is always empty (`Content-Length: 0`). |
| **Protocol version** | `4` — reported as `REMOTE_CAMERA_PROTOCOL_VERSION` in `/status`. |

> **Note on numeric formatting.** The client sends floating-point query values with six
> decimals even for integers (e.g. `iso=2500.000000`, `Width=1920.000000`). The server should
> parse them as floats and round/coerce as needed.

### Response shape

- **`/preview`** returns the raw image bytes (`Content-Type: image/jpeg`).
- **`/status`** returns the camera state as JSON (served with `Content-Type: text/html`, but the
  body is JSON).
- **Every control endpoint** (`setISO`, `setZoomFactor`, …) responds `200 OK` with the **full
  `/status` JSON body** — i.e. each setter returns the updated camera state. The content type is
  again `text/html` with a JSON payload.

---

## Endpoints

### `POST /status`

Returns the current camera state and capabilities. No query parameters.

**Response** — `200 OK`, JSON body (see [Status object](#status-object) below).

---

### `POST /preview`

Returns a single preview frame (live view). The client polls this endpoint repeatedly to build
an MJPEG-like stream; it was by far the most frequent call in the capture.

| Query param | Type | Example | Description |
|---|---|---|---|
| `Width` | float (px) | `1920.000000` | Requested frame width. |
| `Height` | float (px) | `1080.000000` | Requested frame height. |
| `Format` | string | `JPG` | Image format. Only `JPG` observed. |

Observed resolutions (16:9, requested while the user resized the preview window):
`640×360`, `800×450`, `960×540`, `1120×630`, `1280×720`, `1440×810`, `1600×900`,
`1760×990`, `1920×1080`.

**Response** — `200 OK`, `Content-Type: image/jpeg`, raw JPEG bytes.

---

### `POST /setISO`

| Query param | Type | Example | Range (from `/status`) |
|---|---|---|---|
| `iso` | float | `2500.000000` | `minISO` … `maxISO` (e.g. `100`–`6400`) |

---

### `POST /setExposureDuration`

Sets the shutter speed / exposure time.

| Query param | Type | Example | Range |
|---|---|---|---|
| `duration` | float (**microseconds**) | `40000.000000` | `minExposureDuration` … `maxExposureDuration` (reported in **seconds** in `/status`) |

The query value is in **microseconds**; `/status` reports the same value in **seconds**.
e.g. `duration=65000` ⇒ `currentExposureDuration: 0.065`. Observed: `40000`, `65000`,
`166000`, `250000`. Server-reported bounds: `minExposureDuration ≈ 7.76e-5 s` (~77.6 µs),
`maxExposureDuration = 32 s`.

---

### `POST /changeExposureTargetBiasTo`

Sets exposure compensation (EV bias).

| Query param | Type | Example | Range |
|---|---|---|---|
| `bias` | float (EV) | `0.000000` | `minExposureTargetBias` … `maxExposureTargetBias` (e.g. `-12` … `+12`) |

---

### `POST /exposuremode`

| Query param | Type | Example | Values |
|---|---|---|---|
| `AVCaptureExposureMode` | int | `1` | AVFoundation `AVCaptureExposureMode` enum (observed: `0`, `1`) |

`AVCaptureExposureMode` enum: `0 = Locked`, `1 = AutoExpose`, `2 = ContinuousAutoExposure`,
`3 = Custom`. The modes the device supports are advertised in `/status` via the
`AVCaptureExposureMode*` boolean flags.

---

### `POST /whitebalancemode`

| Query param | Type | Example | Values |
|---|---|---|---|
| `AVCaptureWhiteBalanceMode` | int | `2` | One of `supportedWhiteBalanceModes` (observed: `0`, `2`) |

Standard AVFoundation values: `0 = Locked`, `1 = AutoWhiteBalance`,
`2 = ContinuousAutoWhiteBalance`. Values beyond `2` in `supportedWhiteBalanceModes`
(e.g. `5,6,7,9,10,11`) are **server-specific white-balance presets** (Kelvin presets);
the capture showed `AVCaptureWhiteBalanceMode: 9` as a current value.

---

### `POST /setWhiteBalanceGains`

| Query param | Type | Example | Range |
|---|---|---|---|
| `gains` | float | `4000.000000` | `minWhitebalanceGains` … `maxWhitebalanceGains` (e.g. `3000`–`8000`) |

Observed: `3000`, `4000`, `5000`, `6000`, `7000`, `8000` (looks like a color-temperature scale).

---

### `POST /focusmode`

| Query param | Type | Example | Values |
|---|---|---|---|
| `AVCaptureFocusMode` | int | `1` | AVFoundation `AVCaptureFocusMode` enum (observed: `0`, `1`) |

`AVCaptureFocusMode` enum: `0 = Locked`, `1 = AutoFocus`, `2 = ContinuousAutoFocus`.
Supported modes advertised via the `AVCaptureFocusMode*` flags in `/status`.

---

### `POST /setLensPosition`

Sets manual focus position (only meaningful when focus mode is `Locked`).

| Query param | Type | Example | Range |
|---|---|---|---|
| `position` | float | `10.926363` | `minFocusLensPosition` … `maxFocusLensPosition` (e.g. `0.1195` … `12.5`) |

> Note: the range is **not** the usual AVFoundation `0.0`–`1.0`; the server exposes its own
> lens-position scale. Always read the bounds from `/status`.

---

### `POST /setZoomFactor`

| Query param | Type | Example | Range |
|---|---|---|---|
| `zoom` | float | `1.548295` | `1.0` … `maxZoomFactor` (e.g. `1`–`10`) |

---

## Status object

Body returned by `/status` and by every control endpoint. Example capture:

```json
{
  "REMOTE_CAMERA_PROTOCOL_VERSION": 4,

  "CAPTURE_RESOLUTION_WIDTH": 1280,
  "CAPTURE_RESOLUTION_HEIGHT": 720,

  "currentCaptureDeviceNumber": 0,
  "numberOfAvailableCaptureDevices": 2,
  "MULTIPLE_CAPTURE_DEVICE_AVAILABLE": true,
  "AVCaptureDevicePosition": 0,

  "currentISO": 2498,
  "minISO": 100,
  "maxISO": 6400,

  "currentExposureDuration": 0.065,
  "minExposureDuration": 7.763999938964844e-05,
  "maxExposureDuration": 32,

  "exposureTargetBias": 0,
  "minExposureTargetBias": -12,
  "maxExposureTargetBias": 12,

  "AVCaptureExposureMode": 0,
  "AVCaptureExposureModeLocked": true,
  "AVCaptureExposureModeAutoExpose": true,
  "AVCaptureExposureModeContinuousAutoExposure": true,
  "AVCaptureExposureModeCustom": true,

  "AVCaptureFocusMode": 0,
  "AVCaptureFocusModeLocked": true,
  "AVCaptureFocusModeAutoFocus": true,
  "AVCaptureFocusModeContinuousAutoFocus": true,
  "currentFocusLensPosition": 11.363636,
  "minFocusLensPosition": 0.11954521,
  "maxFocusLensPosition": 12.5,

  "AVCaptureWhiteBalanceMode": 9,
  "AVCaptureWhiteBalanceModeLocked": true,
  "AVCaptureWhiteBalanceModeAutoWhiteBalance": true,
  "AVCaptureWhiteBalanceModeContinuousAutoWhiteBalance": true,
  "supportedWhiteBalanceModes": [1, 2, 0, 3, 5, 6, 7, 9, 10, 11],
  "currentWhitebalanceGains": 8000,
  "minWhitebalanceGains": 3000,
  "maxWhitebalanceGains": 8000,

  "currentZoomFactor": 1.5,
  "maxZoomFactor": 10
}
```

### Field reference

| Field | Type | Meaning |
|---|---|---|
| `REMOTE_CAMERA_PROTOCOL_VERSION` | int | Protocol version (`4`). |
| `CAPTURE_RESOLUTION_WIDTH` / `CAPTURE_RESOLUTION_HEIGHT` | int | Sensor/capture resolution in px. |
| `currentCaptureDeviceNumber` | int | Index of the active capture device. |
| `numberOfAvailableCaptureDevices` | int | Number of selectable cameras. |
| `MULTIPLE_CAPTURE_DEVICE_AVAILABLE` | bool | Whether more than one camera is available. |
| `AVCaptureDevicePosition` | int | AVFoundation device position: `0` Unspecified, `1` Back, `2` Front. |
| `currentISO` / `minISO` / `maxISO` | number | Current ISO and its bounds. |
| `currentExposureDuration` / `minExposureDuration` / `maxExposureDuration` | number (**seconds**) | Current shutter speed and bounds. (Setter takes **microseconds**.) |
| `exposureTargetBias` / `minExposureTargetBias` / `maxExposureTargetBias` | number (EV) | Current exposure compensation and bounds. |
| `AVCaptureExposureMode` | int | Current exposure mode (see `/exposuremode`). |
| `AVCaptureExposureMode{Locked,AutoExpose,ContinuousAutoExposure,Custom}` | bool | Which exposure modes the device supports. |
| `AVCaptureFocusMode` | int | Current focus mode (see `/focusmode`). |
| `AVCaptureFocusMode{Locked,AutoFocus,ContinuousAutoFocus}` | bool | Which focus modes are supported. |
| `currentFocusLensPosition` / `minFocusLensPosition` / `maxFocusLensPosition` | number | Manual focus position and bounds (server-specific scale). |
| `AVCaptureWhiteBalanceMode` | int | Current white-balance mode. |
| `AVCaptureWhiteBalanceMode{Locked,AutoWhiteBalance,ContinuousAutoWhiteBalance}` | bool | Which standard WB modes are supported. |
| `supportedWhiteBalanceModes` | int[] | All selectable WB modes (incl. server-specific presets > 2). |
| `currentWhitebalanceGains` / `minWhitebalanceGains` / `maxWhitebalanceGains` | number | Current WB gains (≈ Kelvin) and bounds. |
| `currentZoomFactor` / `maxZoomFactor` | number | Current zoom and max zoom (min is `1.0`). |

---

## Endpoint summary

| Method & path | Query parameter(s) | Returns |
|---|---|---|
| `POST /status` | — | Status JSON |
| `POST /preview` | `Width`, `Height`, `Format=JPG` | JPEG frame |
| `POST /setISO` | `iso` | Status JSON |
| `POST /setExposureDuration` | `duration` (µs) | Status JSON |
| `POST /changeExposureTargetBiasTo` | `bias` (EV) | Status JSON |
| `POST /exposuremode` | `AVCaptureExposureMode` | Status JSON |
| `POST /whitebalancemode` | `AVCaptureWhiteBalanceMode` | Status JSON |
| `POST /setWhiteBalanceGains` | `gains` | Status JSON |
| `POST /focusmode` | `AVCaptureFocusMode` | Status JSON |
| `POST /setLensPosition` | `position` | Status JSON |
| `POST /setZoomFactor` | `zoom` | Status JSON |

> Endpoints that exist in the protocol but were **not exercised** in this capture (e.g. switching
> the active capture device, or triggering a full-resolution capture) are not documented here.
> `currentCaptureDeviceNumber` / `numberOfAvailableCaptureDevices` in `/status` strongly suggest a
> device-selection endpoint exists.

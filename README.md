# Toucan Camera Server

[![Official Website](docs/tags/website.svg)](https://brickfilms.com/) [![Discord](docs/tags/discord.svg)](https://discord.com/invite/mmU2sVAJUq)

**ToucanCameraServer** is an awesome, free, and open-source camera control REST API. The goal is to let users control cameras through a web API.

👉 _This project is supported by Brick à Brack, the non-profit organization that owns [Brickfilms.com](https://brickfilms.com/) - the biggest brickfilming community. You can join us; it's free and without ads!_ 🎥

- 📡 **Live view** - View the camera feed in real time (MJPEG Stream).
- 📸 **Take photos** - Take photos with any camera.
- ⚙️ **Change settings** - Update camera settings easily.
- 🔗 **Relay remote cameras** - Connect to other ToucanCameraServer instances and control their cameras as if they were local.

## Get started

Start the server — a token is generated automatically and the URL to open is printed:

```sh
./toucan-camera-server
```

```
[config] PORT=8040
[config] EXPOSE=false
[config] TOKEN=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
[info] Listening on http://127.0.0.1:8040/?token=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

Open the printed URL in a browser to access the web UI. The token is already included in the URL — no extra setup needed.

**Options**

| Flag              | Description                                                                       |
| ----------------- | --------------------------------------------------------------------------------- |
| `--port <port>`   | Port to listen on (default: `8040`, falls back to a free port if in use)          |
| `--token <token>` | Authentication token (default: auto-generated UUID v4)                            |
| `--expose`        | Bind to `0.0.0.0` (reachable from the LAN) instead of `127.0.0.1` (loopback only) |

By default the server is reachable only from the local machine. Use `--expose` to make it reachable from other devices on the network (it always stays protected by the token). On Android the server is exposed on the LAN automatically.

## Authentication

Every request must include the token, either as a header or a query parameter:

| Method                  | Example                         |
| ----------------------- | ------------------------------- |
| `Authorization` header  | `Authorization: Bearer <token>` |
| `token` query parameter | `GET /cameras?token=<token>`    |

Requests with an invalid or missing token receive a `403 Forbidden` response.

## Remote cameras

A server can relay cameras from other ToucanCameraServer instances ("peers") on the network. Remote cameras then show up in `GET /cameras` (and the web UI) tagged with the peer's `host:port`, and you can connect, stream, capture, and change settings on them exactly like local cameras.

Manage peers from the **Remote peers** panel in the web UI, or via the API:

| Method   | Endpoint      | Description                                                          |
| -------- | ------------- | ------------------------------------------------------------------- |
| `GET`    | `/peers`      | List registered peers                                               |
| `POST`   | `/peers`      | Register a peer — body `{ "url": "192.168.1.5:8040", "token": "…" }` |
| `DELETE` | `/peers/{id}` | Remove a peer                                                       |

The `url` may be given as `host:port` or `http://host:port`. The `token` is the **peer's** own authentication token, and is optional. When adding a peer, the server checks that it is reachable and that the token is valid — an unreachable or invalid peer is rejected and never stored.

> Peers are kept in memory only and are not persisted across restarts. For two machines to reach each other, start each server with `--expose`.

## Contribute

Feel free to make pull-requests or report issues 😉

## Compatibility

| Backend                      | Windows | macOS | Linux | Android |
| ---------------------------- | ------- | ----- | ----- | ------- |
| Webcams / Cameras            | 🟢¹     | 🟢²   | 🟠³   | 🟢⁴     |
| Canon EOS (EDSDK)            | 🟢      | 🟢    | 🟢    | 🔴      |
| Various cameras (libgphoto2) | 🔴      | 🟢⁶   | 🟢⁶   | 🔴      |
| Remote (other instances)     | 🟢⁵     | 🟢⁵   | 🟢⁵   | 🟢⁵     |

🟢 - Supported  
🟠 - Planned  
🔴 - Not compatible / possible

1. Using MediaFoundation
2. Using AVFoundation and IOKit
3. Using V4L2
4. Using camera2
5. Relayed over HTTP — see [Remote cameras](#remote-cameras)
6. Using libgphoto2 — Nikon, Sony, Fuji and many other PTP/USB cameras.

> All native dependencies (Canon EDSDK, libgphoto2 and its camera drivers, …) are packaged inside the release archives — just download, unzip and run, nothing else to install.

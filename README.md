# Toucan Camera Server

[![Official Website](docs/tags/website.svg)](https://brickfilms.com/) [![Discord](docs/tags/discord.svg)](https://discord.com/invite/mmU2sVAJUq)

**ToucanCameraServer** is an awesome, free, and open-source camera control REST API. The goal is to let users control cameras through a web API.

👉 _This project is supported by Brick à Brack, the non-profit organization that owns [Brickfilms.com](https://brickfilms.com/) - the biggest brickfilming community. You can join us; it's free and without ads!_ 🎥

- 📡 **Live view** - View the camera feed in real time (MJPEG Stream).
- 📸 **Take photos** - Take photos with any camera.
- ⚙️ **Change settings** - Update camera settings easily.

## Get started

Start the server — a token is generated automatically and the URL to open is printed:

```sh
./toucan-camera-server
```

```
[config] PORT=8040
[config] TOKEN=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
[info] Listening on http://127.0.0.1:8040/?token=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

Open the printed URL in a browser to access the web UI. The token is already included in the URL — no extra setup needed.

**Options**

| Flag              | Description                                                              |
| ----------------- | ------------------------------------------------------------------------ |
| `--port <port>`   | Port to listen on (default: `8040`, falls back to a free port if in use) |
| `--token <token>` | Authentication token (default: auto-generated UUID v4)                   |

## Authentication

Every request must include the token, either as a header or a query parameter:

| Method                  | Example                         |
| ----------------------- | ------------------------------- |
| `Authorization` header  | `Authorization: Bearer <token>` |
| `token` query parameter | `GET /cameras?token=<token>`    |

Requests with an invalid or missing token receive a `403 Forbidden` response.

## Contribute

Feel free to make pull-requests or report issues 😉

## Compatibility

| Backend                      | Windows | macOS | Linux | Android |
| ---------------------------- | ------- | ----- | ----- | ------- |
| Webcams / Cameras            | 🟢¹     | 🟢²   | 🟠³   | 🟢⁴     |
| Canon EOS (EDSDK)            | 🟢      | 🟢    | 🟢    | 🔴      |
| Nikon (Nikon SDKs)           | 🟠      | 🟠    | 🔴    | 🔴      |
| Various cameras (libgphoto2) | 🔴      | 🟠    | 🟠    | 🟠      |

🟢 - Supported  
🟠 - Planned  
🔴 - Not compatible / possible

1. Using MediaFoundation
2. Using AVFoundation and IOKit
3. Using V4L2
4. Using camera2

$env:PATH = "C:\Users\mbaco\AppData\Local\Android\Sdk\ndk\28.0.12674087\toolchains\llvm\prebuilt\windows-x86_64\bin;" + $env:PATH
$env:ANDROID_NDK_HOME = "C:\Users\mbaco\AppData\Local\Android\Sdk\ndk\28.0.12674087"
// cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --lib --release --features backend-camera2-android

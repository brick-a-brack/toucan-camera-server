# Embedding the camera server in another Android app

The release pipeline publishes two Android artifacts:

| Artifact | Contents |
|---|---|
| `toucan-camera-server-android.zip` | the Toucan app itself (signed `.aab` + `.apk`) |
| `toucan-camera-lib-android.zip` | `libtoucan_camera.so` (arm64-v8a) — the server engine alone |

This page is about the second one: running the server inside **your own** app.

The `.so` contains the HTTP server and the camera backends. It contains none of
the Android plumbing — the foreground service, the notification, the status
parsing — so you supply that. `android/app/src/main/java/com/brickfilms/toucancameraserver/CameraServerService.kt`
in this repository is the reference implementation; copy from it rather than
starting blank.

## The one constraint you cannot work around

The native entry points use JNI **short names**, which encode the package and
class of the declaring class:

```
Java_com_brickfilms_toucancameraserver_CameraServerService_startServer
Java_com_brickfilms_toucancameraserver_CameraServerService_stopServer
Java_com_brickfilms_toucancameraserver_CameraServerService_isServerRunning
Java_com_brickfilms_toucancameraserver_CameraServerService_serverStatusJson
Java_com_brickfilms_toucancameraserver_CameraServerService_setToken
```

There is no `JNI_OnLoad` / `RegisterNatives` and no neutral C API, so your
`external` / `native` declarations **must** live in a class named exactly
`com.brickfilms.toucancameraserver.CameraServerService`, whatever your app's own
package is. Declaring them anywhere else compiles fine and throws
`UnsatisfiedLinkError` on the first call.

You can verify the symbols in the artifact you downloaded:

```sh
llvm-nm --defined-only -D libtoucan_camera.so | grep Java_com_brickfilms
```

## Setup

1. Drop the library at `src/main/jniLibs/arm64-v8a/libtoucan_camera.so`.
2. Declare the service and the permissions it needs in your manifest:

```xml
<uses-permission android:name="android.permission.CAMERA" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_CAMERA" />
<uses-permission android:name="android.permission.INTERNET" />
<uses-permission android:name="android.permission.ACCESS_WIFI_STATE" />
<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />

<service
    android:name="com.brickfilms.toucancameraserver.CameraServerService"
    android:foregroundServiceType="camera"
    android:exported="false" />
```

3. Create the class at that exact FQN. The minimum that binds the library:

```kotlin
package com.brickfilms.toucancameraserver   // not your own package — see above

class CameraServerService : android.app.Service() {
    companion object {
        init { System.loadLibrary("toucan_camera") }

        @JvmStatic external fun startServer(port: Int, token: String, expose: Boolean): Int
        @JvmStatic external fun stopServer()
        @JvmStatic external fun isServerRunning(): Boolean
        @JvmStatic external fun serverStatusJson(): String
        @JvmStatic external fun setToken(token: String)
    }
    // onStartCommand / onDestroy / the notification are yours to write.
}
```

`@JvmStatic` inside the `companion object` is what puts the native methods as
`static` on the outer class, which is what the symbols above expect. Check with
`javap -p` if a call throws `UnsatisfiedLinkError`.

## The native contract

### `startServer(port, token, expose): Int`

Returns the port the server **actually** bound, or a negative error code:
`-1` runtime, `-2` bind failed, `-3` panic. The requested port may be taken, in
which case the OS assigns another — never assume 8040.

- `port <= 0` → the default (8040).
- `token` empty → the one left by a previous `setToken`, else a random UUID. The
  server is never left open.
- `expose` → `true` binds `0.0.0.0` (other devices on the network can reach the
  API), `false` binds `127.0.0.1` (only apps on this device). The `BIND_ADDR`
  environment variable overrides both.

It **blocks until the socket is bound**, so never call it on the main thread.

On a server that is already running: an identical `port`/`expose` changes
nothing and returns the live port (so a service redelivery is harmless); a
different `token` is applied in place; a different `port`/`expose` stops and
rebinds the server, dropping every connection and releasing the camera sessions.

### `stopServer()`

Cuts the accept loop, releases every camera session and waits (up to 5 s) for the
server thread to exit, so the next `startServer` finds the camera free. Call it
from `onDestroy`. Blocking but bounded.

### `isServerRunning(): Boolean`

The real state of the native server, not a flag you maintain.

### `serverStatusJson(): String`

Everything is already in memory, so this is cheap enough to poll.

```json
{
  "running": true,
  "port": 8040,
  "bind_address": "0.0.0.0",
  "expose": true,
  "token": "ABC123",
  "version": "0.0.18",
  "instance_id": "7f3c…",
  "uptime_seconds": 42,
  "backends": ["camera2-android", "remote"],
  "last_error": null
}
```

`token` is the one the auth middleware currently accepts — or, when stopped, the
one the next `startServer` would use. `last_error` says why the last start
failed, and is cleared on a successful one. Device information is **not** here:
it belongs to the HTTP API (`GET /cameras`), which actually enumerates hardware.

### `setToken(token: String)`

Replaces the pairing token; effective immediately on a running server, which
means a client holding the old one gets `403` on its next request. An **empty
token is refused** (the current one is kept): the auth middleware compares the
presented value to this one, so an empty token would let `?token=` through.

## Using the server

Everything else goes over HTTP on the bound port, protected by the token
(`Authorization: Bearer <token>` or `?token=`). See the main README for the
routes — `GET /cameras`, `PUT /cameras/{id}/connect`, `GET /cameras/{id}/liveview`,
`POST /cameras/{id}/capture`, …

## Keeping in sync

The JSON above and the five signatures are the contract. If you upgrade the
`.so`, re-read this page: the Rust side of it is `src/lib.rs`, module
`android_jni`.

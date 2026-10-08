package com.brickfilms.toucancameraserver

/**
 * Result of a [CameraServerService.start] or [CameraServerService.stop] call.
 *
 * A `fun interface` rather than a Kotlin function type, so a Java lambda
 * satisfies it directly:
 *
 * ```java
 * CameraServerService.start(context, "ABC123", 8040, true, null, state -> {
 *     if (state.isRunning()) {
 *         Log.i(TAG, "API on port " + state.getPort());
 *     } else {
 *         Log.e(TAG, "server failed: " + state.getError());
 *     }
 * });
 * ```
 *
 * Always invoked exactly once, on the main thread.
 */
fun interface ServerCallback {
    fun onResult(state: ServerState)
}

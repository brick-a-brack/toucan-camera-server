//! Process-wide graceful shutdown.
//!
//! Backends that own hardware/SDK sessions (Canon EDSDK, Nikon MAID, Windows
//! MediaFoundation) run on dedicated OS threads and only release the device when
//! that thread tears down. On a normal drop this happens via each backend's
//! `Drop`, but an abrupt exit — most importantly a Windows Ctrl-C, which the
//! Nikon SDK otherwise turns into a hard kill — skips every destructor and leaves
//! the body in an open session, so it no longer re-enumerates until it is
//! physically reconnected.
//!
//! This module centralises the fix: it holds the backend registry and runs every
//! backend's [`CameraBackend::shutdown`](crate::camera::CameraBackend::shutdown)
//! once, from the Windows console control handler (before `process::exit`) and
//! from the Unix graceful-shutdown path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::routes::cameras::BackendState;

/// The live backend registry. A Windows console control handler is a bare C
/// callback with no user-data pointer, so it can only reach the backends through
/// a global. Set once by `run_server`, right after the backends are built.
static BACKENDS: Mutex<Option<BackendState>> = Mutex::new(None);

/// Ensures the teardown runs at most once, however many times `run` is called
/// (e.g. both the console handler and the Unix graceful path firing).
static RAN: AtomicBool = AtomicBool::new(false);

/// Records the backend registry so the shutdown path can reach every backend.
pub fn set_backends(backends: BackendState) {
    if let Ok(mut slot) = BACKENDS.lock() {
        *slot = Some(backends);
    }
}

/// Gracefully releases every backend (bounded, blocking), exactly once. Safe to
/// call from an OS signal handler.
pub fn run() {
    if RAN.swap(true, Ordering::SeqCst) {
        return;
    }
    let backends = BACKENDS.lock().ok().and_then(|slot| slot.clone());
    if let Some(backends) = backends {
        for backend in backends.values() {
            backend.shutdown();
        }
    }
}

/// Installs (or re-installs) our console control handler so Ctrl-C / Ctrl-Break
/// gracefully release the SDKs before the process exits.
///
/// Windows invokes control handlers in reverse registration order (LIFO). The
/// Nikon CS-Layer registers its own handler during `InitializeSDK` that swallows
/// Ctrl-C, so we remove any prior copy of ours and add it again — landing it on
/// top of the SDK's — every time this is called. It is therefore safe (and
/// intended) to call once at startup and again after the Nikon SDK initialises.
#[cfg(windows)]
pub fn install_console_handler() {
    use std::os::raw::c_void;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;

    extern "system" {
        fn SetConsoleCtrlHandler(handler: *mut c_void, add: i32) -> i32;
    }

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        match ctrl_type {
            // Release the SDKs, then exit. 130 is the conventional Ctrl-C code.
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                run();
                std::process::exit(130);
            }
            // Leave close/logoff/shutdown to the default (and other) handlers.
            _ => 0, // FALSE
        }
    }

    unsafe {
        // Remove a prior registration (harmless if absent), then add fresh so ours
        // sits on top of any handler registered in between (e.g. the Nikon SDK's).
        SetConsoleCtrlHandler(handler as *mut c_void, 0);
        if SetConsoleCtrlHandler(handler as *mut c_void, 1) == 0 {
            eprintln!("[shutdown] failed to install console Ctrl-C handler");
        }
    }
}

//! Minimal cross-platform dynamic-library loader: `dlopen`/`dlsym` on Unix
//! (macOS, Linux, Android), `LoadLibrary`/`GetProcAddress` on Windows.
//!
//! Camera SDKs are loaded through this at runtime rather than linked at build
//! time, so a given process loads only the SDKs it actually uses. That is what
//! lets each backend run in its own worker process (`backends::subprocess`) —
//! essential on macOS, where the Canon EDSDK and the Nikon SDK cannot share a
//! process (identical Objective-C class names + a shared main run loop).

use std::ffi::CString;
use std::os::raw::{c_char, c_void};

#[cfg(unix)]
extern "C" {
    fn dlopen(filename: *const c_char, flag: std::os::raw::c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[cfg(windows)]
extern "system" {
    fn LoadLibraryA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// Opens a dynamic library by path, returning an opaque handle (`None` on
/// failure). The handle is kept for the process lifetime — SDKs stay resident
/// until exit, so we never `dlclose`.
///
/// # Safety
/// Loads and runs arbitrary native initializer code from `path`.
pub unsafe fn open(path: &str) -> Option<*mut c_void> {
    let c = CString::new(path).ok()?;
    #[cfg(unix)]
    let handle = dlopen(c.as_ptr(), 2 /* RTLD_NOW */);
    #[cfg(windows)]
    let handle = LoadLibraryA(c.as_ptr());
    (!handle.is_null()).then_some(handle)
}

/// Resolves a symbol from an open handle (`None` if absent).
///
/// # Safety
/// `handle` must come from [`open`]; the returned pointer must be transmuted to
/// the symbol's actual signature.
pub unsafe fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    #[cfg(unix)]
    let p = dlsym(handle, c.as_ptr());
    #[cfg(windows)]
    let p = GetProcAddress(handle, c.as_ptr());
    (!p.is_null()).then_some(p)
}

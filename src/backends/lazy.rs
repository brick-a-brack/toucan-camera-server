//! Lazy, hardware-gated backend wrapper.
//!
//! Wraps a heavyweight single-vendor SDK backend (Canon EDSDK, Nikon MAID, …) so
//! that the real backend — and its OS thread, its loaded SDK/DLL — is **never
//! instantiated until a USB device of the declared vendor is actually present on
//! the bus**. At startup, or when that brand is never plugged in, the wrapper stays
//! completely inert: no thread, no SDK, no cost.
//!
//! Once a matching device appears, the real backend is created **once** and kept
//! alive for the rest of the process (even across power-off / unplug). This avoids
//! paying the SDK init cost again on every reconnect — Nikon's `InitializeSDK` in
//! particular can take ~10 s, and in a stop-motion workflow the body is toggled
//! OFF/asleep frequently. The wrapped backend's own logic handles those transitions
//! (session teardown, warm-up), so the wrapper never needs to tear the thread down.
//!
//! Only single-vendor SDK backends are wrapped. Webcam backends (many vendors,
//! different enumeration) and the remote backend (no local hardware) stay eager.

use std::sync::{Arc, Mutex};

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceInfo, ParameterType,
};

/// Returns whether any USB device of one of `vendor_ids` is currently on the bus.
///
/// Fails open: if the USB scan itself errors, returns `true` so the caller falls
/// back to asking the SDK rather than hiding a device that is actually there.
pub fn usb_vendor_present(vendor_ids: &[u16]) -> bool {
    use nusb::MaybeFuture;
    match nusb::list_devices().wait() {
        Ok(devices) => devices
            .into_iter()
            .any(|d| vendor_ids.contains(&d.vendor_id())),
        Err(_) => true,
    }
}

/// Factory that builds the real backend on first hardware detection.
type Factory = fn() -> Result<Arc<dyn CameraBackend>, CameraError>;

/// A [`CameraBackend`] decorator that defers instantiation of the wrapped backend
/// until a device of its USB vendor(s) is detected. See the module docs.
pub struct LazyBackend {
    backend_id: &'static str,
    vendor_ids: &'static [u16],
    priority: i32,
    factory: Factory,
    /// The real backend, once instantiated. `None` until the vendor is first seen;
    /// stays `Some` for the rest of the process afterwards.
    inner: Mutex<Option<Arc<dyn CameraBackend>>>,
}

impl LazyBackend {
    /// Wraps `factory` so the real backend is only built when a USB device matching
    /// `vendor_ids` appears. `backend_id` and `priority` must match what the real
    /// backend reports — they are exposed without instantiating anything (routing of
    /// opaque device IDs and cross-backend dedup depend on them).
    pub fn new(
        backend_id: &'static str,
        vendor_ids: &'static [u16],
        priority: i32,
        factory: Factory,
    ) -> Self {
        Self {
            backend_id,
            vendor_ids,
            priority,
            factory,
            inner: Mutex::new(None),
        }
    }

    /// Returns the real backend, instantiating it on first demand when the hardware
    /// is present. Returns `None` when the vendor is absent (nothing instantiated).
    ///
    /// The USB bus is only scanned while the backend has **not** yet been built;
    /// once it exists it is returned directly, so steady-state calls add no scan.
    fn resolve(&self) -> Option<Arc<dyn CameraBackend>> {
        // Fast path: already built (kept alive for the process lifetime).
        if let Some(b) = self.inner.lock().unwrap().as_ref() {
            return Some(b.clone());
        }
        // Not built yet — only in this state do we probe the bus.
        if !usb_vendor_present(self.vendor_ids) {
            return None;
        }
        let mut guard = self.inner.lock().unwrap();
        // Re-check under the lock: another thread may have built it meanwhile.
        if guard.is_none() {
            match (self.factory)() {
                Ok(b) => {
                    eprintln!("[lazy] {} device detected — backend initialized", self.backend_id);
                    *guard = Some(b);
                }
                Err(e) => {
                    eprintln!("[lazy] {} backend failed to initialize: {e}", self.backend_id);
                    return None;
                }
            }
        }
        guard.clone()
    }
}

impl CameraBackend for LazyBackend {
    fn backend_id(&self) -> &str {
        self.backend_id
    }

    fn dedup_priority(&self) -> i32 {
        self.priority
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        match self.resolve() {
            Some(b) => b.list_devices(),
            None => Ok(Vec::new()),
        }
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        match self.resolve() {
            Some(b) => b.connect(native_id),
            None => Err(CameraError::DeviceNotFound(native_id.to_string())),
        }
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        match self.resolve() {
            Some(b) => b.disconnect(native_id),
            None => Err(CameraError::DeviceNotFound(native_id.to_string())),
        }
    }

    fn is_connected(&self, native_id: &str) -> bool {
        match self.resolve() {
            Some(b) => b.is_connected(native_id),
            None => false,
        }
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        match self.resolve() {
            Some(b) => b.get_parameters(native_id),
            None => Err(CameraError::DeviceNotFound(native_id.to_string())),
        }
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        match self.resolve() {
            Some(b) => b.get_live_view_frame(native_id),
            None => Err(CameraError::DeviceNotFound(native_id.to_string())),
        }
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        match self.resolve() {
            Some(b) => b.set_parameter(native_id, param_type, value),
            None => Err(CameraError::DeviceNotFound(native_id.to_string())),
        }
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        match self.resolve() {
            Some(b) => b.capture_photo(native_id),
            None => Err(CameraError::DeviceNotFound(native_id.to_string())),
        }
    }

    fn shutdown(&self) {
        // Never instantiate on shutdown — only tear down a backend that was actually
        // built (peek the slot directly, no USB scan / no factory).
        if let Some(b) = self.inner.lock().unwrap().as_ref() {
            b.shutdown();
        }
    }
}

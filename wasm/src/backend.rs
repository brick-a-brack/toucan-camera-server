//! The browser counterpart of the server's `CameraBackend` trait.
//!
//! Same methods, same native-ID contract, same errors — but asynchronous: in a
//! browser every camera call (getUserMedia, WebUSB) is a promise, and blocking
//! the page's only thread on one is not an option. The page is single-threaded,
//! so futures are `!Send` and state lives in `RefCell`s.

use std::future::Future;
use std::pin::Pin;

use crate::camera::{CameraError, CameraParameter, DeviceInfo, ParameterType};

/// A boxed, single-threaded future — what every backend method returns, so the
/// trait stays object-safe (`Rc<dyn WebBackend>`).
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub trait WebBackend {
    /// Unique name of this backend, used to build opaque device IDs.
    fn backend_id(&self) -> &str;

    /// Priority used to break cross-backend duplicates; see the server trait.
    fn dedup_priority(&self) -> i32 {
        0
    }

    /// Every device currently visible to this backend. `DeviceInfo.id` is the
    /// already-encoded opaque ID.
    fn list_devices(&self) -> LocalFuture<'_, Result<Vec<DeviceInfo>, CameraError>>;

    /// Opens a session. Connecting an already-connected device is a no-op.
    fn connect<'a>(&'a self, native_id: &'a str) -> LocalFuture<'a, Result<(), CameraError>>;

    fn disconnect<'a>(&'a self, native_id: &'a str) -> LocalFuture<'a, Result<(), CameraError>>;

    fn is_connected(&self, native_id: &str) -> bool;

    /// The currently settable parameters (device must be connected).
    fn get_parameters<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<CameraParameter>, CameraError>>;

    /// One live-view frame as JPEG bytes (device must be connected).
    fn get_live_view_frame<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>>;

    /// Sets a parameter; `value` follows the server's string conventions
    /// (`"true"`/`"false"`, a stringified integer, or an option value).
    fn set_parameter<'a>(
        &'a self,
        native_id: &'a str,
        param_type: ParameterType,
        value: &'a str,
    ) -> LocalFuture<'a, Result<(), CameraError>>;

    /// A full-quality still as JPEG bytes (device must be connected).
    fn capture_photo<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>>;
}

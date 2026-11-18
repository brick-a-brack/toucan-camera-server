pub mod averaging;
pub mod rotation;
mod types;

pub use types::*;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Common interface every camera backend must implement.
///
/// Backends work exclusively with native IDs (e.g. Canon port names).
/// Opaque ID encoding/decoding is handled by the route layer.
pub trait CameraBackend: Send + Sync {
    /// Unique name of this backend (e.g. `"canon"`). Used to build opaque device IDs.
    fn backend_id(&self) -> &str;

    /// Priority used to break cross-backend duplicates (devices sharing a
    /// [`DeviceInfo::dedup_key`]). The highest-priority backend wins. Dedicated
    /// vendor SDK backends (richer control: native live view, full parameter set)
    /// override this above the generic backends. Default: generic priority.
    fn dedup_priority(&self) -> i32 {
        0
    }

    /// Returns all devices currently visible to this backend.
    /// The `DeviceInfo.id` field contains the already-encoded opaque ID.
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError>;

    /// Opens a session with the device identified by `native_id`.
    /// Connecting an already-connected device is a no-op.
    fn connect(&self, native_id: &str) -> Result<(), CameraError>;

    /// Closes the session with the device identified by `native_id`.
    fn disconnect(&self, native_id: &str) -> Result<(), CameraError>;

    /// Returns true if a session is currently open for `native_id`.
    fn is_connected(&self, native_id: &str) -> bool;

    /// Returns all currently settable parameters with their allowed values.
    /// The device must be connected before calling this.
    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError>;

    /// Captures a single live view frame and returns it as raw JPEG bytes.
    /// The device must be connected before calling this.
    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError>;

    /// Sets a parameter by its type and value.
    ///
    /// `value` is always a string:
    /// - Boolean params:            `"true"` or `"false"`
    /// - Range params:              stringified integer (e.g. `"42"`)
    /// - Select / RangeSelect:      opaque key from `ParameterOption.value` (e.g. `"77"`)
    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError>;

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let _ = native_id;
        Err(CameraError::NotSupported)
    }

    /// Best-effort, bounded, blocking teardown before the process exits.
    ///
    /// Called by the process-wide shutdown path (`crate::shutdown`) on Ctrl-C
    /// (Windows) and on graceful shutdown (Unix). Backends that own a hardware/SDK
    /// session must release it here — close sessions, stop live view, terminate the
    /// SDK — so the device is not left claimed and re-enumerates on the next run.
    /// Must not block indefinitely: the SDK thread may be mid-call, so
    /// implementations wait for teardown with a short timeout. Default: no-op (for
    /// backends with nothing to release, e.g. the remote proxy).
    fn shutdown(&self) {}
}

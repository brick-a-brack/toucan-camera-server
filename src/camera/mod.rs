use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Opaque device ID
// ---------------------------------------------------------------------------

/// Opaque, URL-safe device identifier exposed by the API.
///
/// Encodes the backend name and the backend-native device ID as
/// `base64url(backend:native_id)`, e.g. `base64url("canon:USB:0,1,0")`.
/// This avoids URL-encoding issues and hides internal identifiers from clients.
pub struct DeviceId {
    pub backend: String,
    pub native_id: String,
}

impl DeviceId {
    pub fn new(backend: impl Into<String>, native_id: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            native_id: native_id.into(),
        }
    }

    /// Encodes to the opaque string sent to clients.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}:{}", self.backend, self.native_id))
    }

    /// Decodes an opaque string received from a client.
    pub fn decode(encoded: &str) -> Result<Self, CameraError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| CameraError::InvalidDeviceId)?;
        let s = String::from_utf8(bytes).map_err(|_| CameraError::InvalidDeviceId)?;
        let (backend, native_id) = s.split_once(':').ok_or(CameraError::InvalidDeviceId)?;
        Ok(Self {
            backend: backend.to_string(),
            native_id: native_id.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Parameter type — exhaustive list of all known parameter identifiers.
// Adding a new parameter requires updating this enum and any backend that
// exposes it. This prevents spelling mismatches between backends.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterType {

    // Stream format and quality
    ImageQuality,
    VideoStreamFormat,
    PowerLineFrequency,

    // Color temperature
    ColorTemperature,

    // Aperture
    Aperture,
    ApertureAuto,

    // Shutter speed
    ShutterSpeed,
    ShutterSpeedAuto,

    // Camera white balance
    WhiteBalance,
    WhiteBalanceAuto,

    // Camera sharpness
    Sharpness,
    SharpnessAuto,

    // Camera gamma
    Gamma,
    GammaAuto,

    // Camera exposure
    Exposure,
    ExposureAuto,
    ExposureCompensation,
    BacklightCompensation,

    // Camera focus
    Focus,
    FocusAuto,
    FocusMode,

    // Camera saturation
    Saturation,
    SaturationAuto,

    // Camera brightness
    Brightness,
    BrightnessAuto,

    // Camera contrast
    Contrast,
    ContrastAuto,

    // Camera hue
    Hue,
    HueAuto,

    // Camera Gain
    Gain,
    GainAuto,

    // Camera Pan
    Pan,
    PanAuto,

    // Camera Tilt
    Tilt,
    TiltAuto,

    // Camera Roll
    Roll,
    RollAuto,

    // Camera Zoom
    Zoom,
    ZoomAuto,

    // ISO
    Iso,
    IsoAuto,

    // Photo resolution (width × height encoded as w*10000+h)
    PhotoResolution,

    // Live view controls
    LiveViewZoom,
    LiveViewPan,
    LiveViewTilt,
    LiveViewRoll,
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Device information returned by `list_devices`.
/// The `id` field is the opaque encoded ID suitable for use in subsequent API calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Opaque, URL-safe device identifier (base64url encoded).
    pub id: String,
    /// Human-readable device name (e.g. "Canon EOS R5").
    pub name: String,
    /// Whether a session is currently open with this device.
    pub connected: bool,
    /// Stable cross-backend identity of the physical device (see [`dedup_key`]),
    /// or `None` when the backend can't determine it. The server uses it to drop
    /// duplicates when several backends see the same camera (e.g. a Nikon shown by
    /// both the Nikon SDK and gphoto2), keeping the higher-priority backend.
    /// Never serialized — it is an internal dedup hint, not part of the API.
    #[serde(skip)]
    pub dedup_key: Option<String>,
}

/// Builds the cross-backend dedup key for a physical camera from its USB vendor
/// id and model name. Backends that can identify a device's vendor + model emit
/// this so the server can recognise the same body reported by two backends.
///
/// The model is normalised so the various spellings agree: the dedicated SDK's
/// name (e.g. "Z5_2"), libgphoto2's name ("Nikon Z 5_2") and the USB product
/// string ("Z 5_2") all reduce to the same token.
pub fn dedup_key(vendor_id: u16, model: &str) -> String {
    format!("{vendor_id:04x}:{}", normalize_model(model))
}

/// Whether to log cross-backend dedup decisions (`TOUCAN_DEDUP_DEBUG=1`).
pub fn dedup_debug_enabled() -> bool {
    std::env::var_os("TOUCAN_DEDUP_DEBUG").is_some()
}

/// Normalises a camera model for cross-backend matching: lowercase, map Nikon's
/// `_2`/`_3` mark suffixes to `ii`/`iii`, drop vendor/category noise words that
/// appear in some names but not others (`nikon`, and the `dsc` = "Digital Still
/// Camera" prefix USB product strings carry), and strip everything but
/// alphanumerics. So the dedicated SDK's name ("Z5_2"), libgphoto2's name and the
/// USB product string ("DSC Z5_2") all reduce to the same token.
/// e.g. `"Nikon Z 6_2"` → `"z6ii"`, `"DSC Z5_2"` → `"z5ii"`, `"Canon EOS R5"` →
/// `"canoneosr5"`.
pub fn normalize_model(model: &str) -> String {
    let s = model
        .to_ascii_lowercase()
        .replace("_3", "iii")
        .replace("_2", "ii")
        .replace("nikon", "")
        .replace("dsc", "");
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// One option in a Select or RangeSelect parameter.
/// `value` is the opaque string key passed back to `set_parameter`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterOption {
    /// Human-readable label (e.g. "f/5.6", "1/500", "ISO 400").
    pub label: String,
    /// Opaque string key for identifying the option.
    pub value: String,
}

/// A camera parameter, discriminated by its representation kind.
///
/// Every variant carries `disabled: bool`.  When `true` the parameter is
/// read-only in the current camera state (e.g. focus value while auto-focus
/// is active) and the client should render it as greyed-out.  It is still
/// always included in the response so the client has a complete picture.
///
/// - `boolean`      — on/off toggle; `current` is a bool. Value sent to `set_parameter` is `"true"` or `"false"`.
/// - `range`        — continuous numeric value (slider); `current`, `min`, `max`, `step` are integers.
/// - `select`       — arbitrary discrete choices; `current` matches one `option.value`.
/// - `range_select` — ordered discrete values with numeric progression (ISO, aperture);
///   rendered as a select but values are semantically ordered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CameraParameter {
    Boolean {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: bool,
        #[serde(default)]
        disabled: bool,
    },
    Range {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: i32,
        min: i32,
        max: i32,
        step: i32,
        #[serde(default)]
        disabled: bool,
    },
    Select {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: String,
        options: Vec<ParameterOption>,
        #[serde(default)]
        disabled: bool,
    },
    RangeSelect {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: String,
        options: Vec<ParameterOption>,
        #[serde(default)]
        disabled: bool,
    },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Serialize, Deserialize)]
pub enum CameraError {
    #[error("SDK error: {0:#010x}")]
    SdkError(u32),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("invalid device id")]
    InvalidDeviceId,
    #[error("no session open for this device")]
    NotConnected,
    #[error("operation not supported by this backend")]
    NotSupported,
    /// A driver error that carries its own human-readable reason (libgphoto2
    /// reports "Camera is busy", "Could not claim the USB device"… rather than a
    /// numeric SDK code). Preferred over `SdkError` when a message is available.
    #[error("{0}")]
    Backend(String),
    #[error("remote backend error: {0}")]
    Remote(String),
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_normalization() {
        assert_eq!(normalize_model("Nikon Z 6"), "z6");
        assert_eq!(normalize_model("Nikon Z 6_2"), "z6ii"); // mark II
        assert_eq!(normalize_model("Nikon Z 6_3"), "z6iii"); // mark III
        assert_eq!(normalize_model("Z5_2"), "z5ii"); // SDK-style name
        assert_eq!(normalize_model("Nikon Z 5_2"), "z5ii"); // gphoto / USB style
        assert_eq!(normalize_model("DSC Z5_2"), "z5ii"); // USB product string (DSC prefix)
        assert_eq!(normalize_model("Canon EOS R5"), "canoneosr5");
        assert_eq!(normalize_model("Nikon D850"), "d850");
    }

    #[test]
    fn dedup_key_agrees_across_naming() {
        // The same physical Z5II named the SDK way and the gphoto2/USB way yields
        // one key, so the server recognises it as a single camera.
        let from_sdk = dedup_key(0x04b0, "Z5_2");
        let from_gphoto = dedup_key(0x04b0, "Nikon Z 5_2");
        let from_usb_product = dedup_key(0x04b0, "DSC Z5_2"); // real Z5II USB product string
        assert_eq!(from_sdk, from_gphoto);
        assert_eq!(from_sdk, from_usb_product);
        assert_eq!(from_sdk, "04b0:z5ii");
        // Distinct bodies → distinct keys (z5 vs z5ii vs z50).
        assert_ne!(dedup_key(0x04b0, "Z 5"), dedup_key(0x04b0, "Z 5_2"));
        assert_ne!(dedup_key(0x04b0, "Z 5"), dedup_key(0x04b0, "Z 50"));
    }
}

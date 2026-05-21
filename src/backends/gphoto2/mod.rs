use std::collections::HashMap;
use std::sync::Mutex;

use gphoto2::widget::{RadioWidget, RangeWidget, ToggleWidget, Widget};

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

/// libgphoto2-backed camera backend.
///
/// `gphoto2::Context` and `gphoto2::Camera` are documented `Send + Sync`; the
/// crate serializes per-camera calls internally. A simple `Mutex<HashMap>` of
/// open `Camera` handles is therefore sufficient — no actor thread is needed.
pub struct GPhoto2Backend {
    context:   gphoto2::Context,
    connected: Mutex<HashMap<String, gphoto2::Camera>>,
}

impl GPhoto2Backend {
    pub fn new() -> Result<Self, CameraError> {
        let context = gphoto2::Context::new().map_err(map_err)?;
        Ok(Self {
            context,
            connected: Mutex::new(HashMap::new()),
        })
    }

    /// Returns a clone of the live `Camera` handle for `native_id`.
    /// The clone is a refcount bump on the underlying gphoto2 camera, so we
    /// release the lock before doing any (potentially slow) SDK call.
    fn camera_for(&self, native_id: &str) -> Result<gphoto2::Camera, CameraError> {
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        connected
            .get(native_id)
            .cloned()
            .ok_or(CameraError::NotConnected)
    }
}

fn map_err(err: gphoto2::Error) -> CameraError {
    eprintln!("[gphoto2] error: {err}");
    CameraError::SdkError(0)
}

impl CameraBackend for GPhoto2Backend {
    fn backend_id(&self) -> &str {
        "gphoto2"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let cameras = self.context.list_cameras().wait().map_err(map_err)?;
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");

        let devices = cameras
            .map(|d| DeviceInfo {
                connected: connected.contains_key(&d.port),
                id: DeviceId::new("gphoto2", &d.port).encode(),
                name: d.model,
            })
            .collect();

        Ok(devices)
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        // Idempotent: already connected → no-op.
        if self.is_connected(native_id) {
            return Ok(());
        }

        // gphoto2 does not let us open a camera by raw port string — we need
        // the matching `CameraDescriptor` from `list_cameras`.
        let descriptor = self
            .context
            .list_cameras()
            .wait()
            .map_err(map_err)?
            .find(|d| d.port == native_id)
            .ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))?;

        let camera = self
            .context
            .get_camera(&descriptor)
            .wait()
            .map_err(map_err)?;

        let mut connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        connected.insert(native_id.to_string(), camera);
        Ok(())
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let mut connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        // Drop runs cleanup automatically; no explicit close call needed.
        connected.remove(native_id);
        Ok(())
    }

    fn is_connected(&self, native_id: &str) -> bool {
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        connected.contains_key(native_id)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let camera = self.camera_for(native_id)?;
        let root = camera.config().wait().map_err(map_err)?;

        let mut params = Vec::new();
        for child in root.children_iter() {
            walk_widget(&child, &mut params);
        }
        Ok(params)
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let camera = self.camera_for(native_id)?;
        // capture_preview() — fast, low-res preview frame for live view streaming.
        // Different from capture_image(): no card write, no shutter actuation,
        // no DirItemRequestTransfer event. Designed to be called continuously.
        let preview = camera.capture_preview().wait().map_err(map_err)?;
        let data = preview.get_data(&self.context).wait().map_err(map_err)?;
        Ok(data.into_vec())
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let camera = self.camera_for(native_id)?;

        // capture_image() triggers the shutter and returns a path on the camera
        // (either internal RAM or SD card, depending on the `capturetarget`
        // config). The file's format follows the camera's image-quality
        // setting — make sure the camera is in JPEG mode for now (the HTTP
        // response is hardcoded to image/jpeg upstream).
        let path = camera.capture_image().wait().map_err(map_err)?;

        // Download into memory (no on-disk intermediate) via the in-memory
        // variant of `CameraFS::download`. The underlying gphoto2 call is
        // gp_camera_file_get(GP_FILE_TYPE_NORMAL).
        let file = camera
            .fs()
            .download(&path.folder(), &path.name())
            .wait()
            .map_err(map_err)?;

        let data = file.get_data(&self.context).wait().map_err(map_err)?;
        Ok(data.into_vec())
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let camera = self.camera_for(native_id)?;
        let key = config_key_for(param_type).ok_or(CameraError::NotSupported)?;

        // Try the most likely widget type first; fall back through alternatives.
        // gphoto2 exposes the same logical parameter as Radio on most cameras
        // but as Range or Toggle on some, so we probe each.
        if let Ok(widget) = camera.config_key::<RadioWidget>(key).wait() {
            widget.set_choice(value).map_err(map_err)?;
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }
        if let Ok(widget) = camera.config_key::<RangeWidget>(key).wait() {
            let v: f32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            widget.set_value(v);
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }
        if let Ok(widget) = camera.config_key::<ToggleWidget>(key).wait() {
            let on = matches!(value, "1" | "true" | "True");
            widget.set_toggled(on);
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }

        Err(CameraError::NotSupported)
    }
}

// ---------------------------------------------------------------------------
// Widget tree walking → CameraParameter list
// ---------------------------------------------------------------------------

fn walk_widget(widget: &Widget, out: &mut Vec<CameraParameter>) {
    match widget {
        Widget::Group(g) => {
            for child in g.children_iter() {
                walk_widget(&child, out);
            }
        }
        Widget::Radio(r) => {
            if r.readonly() {
                return;
            }
            if let Some(pt) = param_type_for(&r.name()) {
                let options: Vec<ParameterOption> = r
                    .choices_iter()
                    .map(|c| {
                        let s = c.to_string();
                        ParameterOption {
                            label: s.clone(),
                            value: s,
                        }
                    })
                    .collect();
                let current = r.choice();
                out.push(if is_ordered(pt) {
                    CameraParameter::RangeSelect {
                        param_type: pt,
                        current,
                        options,
                        disabled: false,
                    }
                } else {
                    CameraParameter::Select {
                        param_type: pt,
                        current,
                        options,
                        disabled: false,
                    }
                });
            }
        }
        Widget::Range(r) => {
            if r.readonly() {
                return;
            }
            if let Some(pt) = param_type_for(&r.name()) {
                let (range, step) = r.range_and_step();
                out.push(CameraParameter::Range {
                    param_type: pt,
                    current: r.value() as i32,
                    min: *range.start() as i32,
                    max: *range.end() as i32,
                    step: step as i32,
                    disabled: false,
                });
            }
        }
        Widget::Toggle(t) => {
            if t.readonly() {
                return;
            }
            if let Some(pt) = param_type_for(&t.name()) {
                let current = if t.toggled().unwrap_or(false) { "1" } else { "0" }.to_string();
                out.push(CameraParameter::Select {
                    param_type: pt,
                    current,
                    options: vec![
                        ParameterOption { label: "Off".into(), value: "0".into() },
                        ParameterOption { label: "On".into(),  value: "1".into() },
                    ],
                    disabled: false,
                });
            }
        }
        _ => {} // Button, Date, Text → not exposed as parameters
    }
}

// ---------------------------------------------------------------------------
// Mapping between gphoto2 config-key names and our ParameterType enum.
//
// Cameras report slightly different config-key names depending on the camlib
// (Nikon vs Sony vs Fuji vs ptp2…). The pairs below are the ones I have seen
// in the wild — extend as you observe new ones.
// ---------------------------------------------------------------------------

fn param_type_for(name: &str) -> Option<ParameterType> {
    match name {
        "iso" | "isospeed" | "iso speed" | "iso_speed" => Some(ParameterType::Iso),
        "shutterspeed" | "shutter_speed" | "shutter speed" => Some(ParameterType::ShutterSpeed),
        "aperture" | "f-number" | "f_number" | "fnumber" => Some(ParameterType::Aperture),
        "whitebalance" | "white_balance" | "white balance" => Some(ParameterType::WhiteBalance),
        "colortemperature" | "color_temperature" => Some(ParameterType::ColorTemperature),
        "exposurecompensation" | "exposure_compensation" => Some(ParameterType::ExposureCompensation),
        "imageformat" | "image_format" | "imagequality" | "image_quality" => Some(ParameterType::ImageQuality),
        _ => None,
    }
}

fn config_key_for(param_type: ParameterType) -> Option<&'static str> {
    match param_type {
        ParameterType::Iso                  => Some("iso"),
        ParameterType::ShutterSpeed         => Some("shutterspeed"),
        ParameterType::Aperture             => Some("aperture"),
        ParameterType::WhiteBalance         => Some("whitebalance"),
        ParameterType::ColorTemperature     => Some("colortemperature"),
        ParameterType::ExposureCompensation => Some("exposurecompensation"),
        ParameterType::ImageQuality         => Some("imageformat"),
        _ => None,
    }
}

/// Parameters whose values form an ordered numeric progression (ISO, aperture,
/// shutter speed, exposure compensation) should render as `RangeSelect` so the
/// UI knows the order is meaningful.
fn is_ordered(pt: ParameterType) -> bool {
    matches!(
        pt,
        ParameterType::Iso
            | ParameterType::Aperture
            | ParameterType::ShutterSpeed
            | ParameterType::ExposureCompensation
            | ParameterType::ColorTemperature
    )
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gphoto2::widget::{RadioWidget, RangeWidget, ToggleWidget, Widget};

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

/// Interval between keep-alive pings. Canon bodies sleep after their
/// `autopoweroff` timer (as low as ~30 s on some menus), so we ping well under it.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// libgphoto2-backed camera backend.
///
/// `gphoto2::Context` and `gphoto2::Camera` are documented `Send + Sync`; the
/// crate serializes per-camera calls internally. Open `Camera` handles live in a
/// shared `Arc<Mutex<HashMap>>` so the background keep-alive thread can ping them.
pub struct GPhoto2Backend {
    context:   gphoto2::Context,
    connected: Arc<Mutex<HashMap<String, gphoto2::Camera>>>,
}

impl GPhoto2Backend {
    pub fn new() -> Result<Self, CameraError> {
        // libgphoto2 localizes parameter labels via gettext, following the system
        // locale (French here: "Automatique", "pose longue", even "0,5" for "0.5").
        // Force the C locale before the first gphoto2 call — which is what activates
        // gettext — so labels and numeric formatting are stable English/ASCII. This
        // also keeps the option `value`s we round-trip back to `set_choice`
        // consistent between reads and writes.
        std::env::set_var("LC_ALL", "C");

        // If libgphoto2's plugins were bundled next to the binary (build.rs
        // `copy_gphoto2_bundle`), point libgphoto2 at them so the server runs
        // without a system libgphoto2 install. No-op otherwise → dev builds use
        // the system install.
        Self::use_bundled_plugins();

        let context = gphoto2::Context::new().map_err(map_err)?;
        let connected: Arc<Mutex<HashMap<String, gphoto2::Camera>>> =
            Arc::new(Mutex::new(HashMap::new()));
        spawn_keepalive(connected.clone());
        Ok(Self { context, connected })
    }

    /// Point `CAMLIBS`/`IOLIBS` at plugin directories shipped next to the
    /// executable, if present. Must run before the first gphoto2 call.
    fn use_bundled_plugins() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(dir) = exe.parent() else {
            return;
        };
        for (var, sub) in [("CAMLIBS", "camlibs"), ("IOLIBS", "iolibs")] {
            let path = dir.join(sub);
            if path.is_dir() {
                std::env::set_var(var, &path);
            }
        }
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

/// Background thread that keeps every connected camera awake. Canon bodies refuse
/// to have `autopoweroff` disabled (PTP `0x2019` Device Busy), so instead we
/// generate periodic activity — any PTP transaction resets the body's idle timer,
/// the same trick EOS Utility uses. Runs for the whole process lifetime; cameras
/// are pinged only while present in the map.
fn spawn_keepalive(connected: Arc<Mutex<HashMap<String, gphoto2::Camera>>>) {
    thread::Builder::new()
        .name("gphoto2-keepalive".into())
        .spawn(move || loop {
            thread::sleep(KEEPALIVE_INTERVAL);
            // Snapshot the handles, then release the lock before the (slow) PTP
            // reads so connect/disconnect/operations are never blocked on us.
            let cameras: Vec<gphoto2::Camera> = {
                let map = connected.lock().expect("gphoto2 mutex poisoned");
                map.values().cloned().collect()
            };
            for camera in cameras {
                // Reading the config tree is enough activity to reset the timer
                // and works on any gphoto2 body. Best-effort: ignore errors (a
                // camera may have just been unplugged).
                let _ = camera.config().wait();
            }
        })
        .expect("failed to spawn gphoto2 keep-alive thread");
}

fn map_err(err: gphoto2::Error) -> CameraError {
    eprintln!("[gphoto2] error: {err}");
    CameraError::SdkError(0)
}

/// When the dedicated Canon EDSDK backend is compiled in, it owns Canon bodies
/// (native EVF live view, full Canon property set), so the gphoto2 backend hides
/// them — otherwise the same camera shows up under two backends and the two
/// drivers fight over the USB device. Without `backend-canon`, gphoto2 handles
/// Canon too. libgphoto2 reports Canon models as e.g. "Canon EOS 600D".
fn owned_by_other_backend(model: &str) -> bool {
    cfg!(feature = "backend-canon") && model.to_lowercase().starts_with("canon")
}

impl CameraBackend for GPhoto2Backend {
    fn backend_id(&self) -> &str {
        "gphoto2"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let cameras = self.context.list_cameras().wait().map_err(map_err)?;
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");

        let devices = cameras
            .filter(|d| !owned_by_other_backend(&d.model))
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

        // Defensive: when the EDSDK backend is compiled in it owns Canon bodies,
        // so refuse to grab one here even if a client crafts the id (it would
        // contend with EDSDK for the USB device). list_devices already hides them.
        if owned_by_other_backend(&descriptor.model) {
            return Err(CameraError::NotSupported);
        }

        let camera = self
            .context
            .get_camera(&descriptor)
            .wait()
            .map_err(map_err)?;

        // A background keep-alive thread (see `spawn_keepalive`) pings this
        // camera periodically to stop the body from sleeping mid-session.
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

        // A select/range_select with fewer than two options offers no real choice
        // (e.g. exposure compensation in Manual mode, which the body reports as a
        // single "0"). Hide those so the UI only shows controls the user can act on.
        params.retain(|p| match p {
            CameraParameter::Select { options, .. }
            | CameraParameter::RangeSelect { options, .. } => options.len() >= 2,
            _ => true,
        });

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

        // IsoAuto is a synthetic toggle backed by the camera's "iso" radio widget,
        // mirroring the Canon backend. true → select the auto choice (the only
        // non-numeric one); false → select the first concrete ISO value queried
        // live from the camera, rather than hardcoding a value it may not offer.
        if param_type == ParameterType::IsoAuto {
            let key = config_key_for(ParameterType::Iso).ok_or(CameraError::NotSupported)?;
            let widget = camera.config_key::<RadioWidget>(key).wait().map_err(map_err)?;
            let on = matches!(value, "1" | "true" | "True");
            let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
            let target = if on {
                choices.iter().find(|c| c.parse::<u32>().is_err())
            } else {
                choices.iter().find(|c| c.parse::<u32>().is_ok())
            }
            .ok_or(CameraError::NotSupported)?;
            widget.set_choice(target.as_str()).map_err(map_err)?;
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }

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
            let Some(pt) = param_type_for(&r.name()) else {
                return;
            };
            let current = r.choice();
            let choices: Vec<String> = r.choices_iter().map(|c| c.to_string()).collect();

            match pt {
                // ISO is split into an IsoAuto toggle + an Iso selector, mirroring
                // the Canon backend. libgphoto2 localizes the auto label (e.g.
                // "Automatique" in French), so we detect it locale-independently:
                // the auto choice is the only non-numeric one — every real ISO
                // value parses as an integer.
                ParameterType::Iso => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|&c| is_concrete_iso(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    if options.is_empty() {
                        return;
                    }
                    let iso_auto = !is_concrete_iso(&current);
                    out.push(CameraParameter::Boolean {
                        param_type: ParameterType::IsoAuto,
                        current: iso_auto,
                        disabled: false,
                    });
                    // When auto is on the concrete ISO is read-only; show the first
                    // value as a placeholder and disable the control.
                    let iso_current = if iso_auto {
                        options[0].value.clone()
                    } else {
                        current
                    };
                    out.push(CameraParameter::RangeSelect {
                        param_type: ParameterType::Iso,
                        current: iso_current,
                        options,
                        disabled: iso_auto,
                    });
                }
                // Shutter speed: drop the bulb entry — it is a long-exposure mode,
                // not a discrete speed. Its label is localized too ("pose longue"
                // in French), so we exclude any choice with no digit; every real
                // speed has one ("30", "1/60", "0.5"…), bulb does not.
                ParameterType::ShutterSpeed => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|&c| is_real_shutter_speed(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(CameraParameter::RangeSelect {
                        param_type: ParameterType::ShutterSpeed,
                        current,
                        options,
                        disabled: false,
                    });
                }
                // Image quality: the server only ever returns JPEG (capture is
                // hardcoded to image/jpeg), so hide RAW / RAW+JPEG formats —
                // selecting one would break capture.
                ParameterType::ImageQuality => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|&c| is_jpeg_format(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(CameraParameter::Select {
                        param_type: ParameterType::ImageQuality,
                        current,
                        options,
                        disabled: false,
                    });
                }
                _ => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
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

// ---------------------------------------------------------------------------
// Value classification — locale-independent, since libgphoto2 localizes labels.
// ---------------------------------------------------------------------------

/// A concrete (non-auto) ISO value. libgphoto2 reports the auto choice with a
/// localized label ("Auto" / "Automatique"…), but every real ISO is a plain
/// integer — so anything that does not parse as one is the auto entry.
fn is_concrete_iso(choice: &str) -> bool {
    choice.parse::<u32>().is_ok()
}

/// A real shutter speed. The bulb entry is localized too ("Bulb" / "pose
/// longue"), but unlike every real speed ("30", "1/60", "0.5") it has no digit.
fn is_real_shutter_speed(choice: &str) -> bool {
    choice.chars().any(|c| c.is_ascii_digit())
}

/// A JPEG image-quality choice. RAW / RAW+JPEG / cRAW formats contain "RAW"; the
/// server only ever returns JPEG (capture is hardcoded to image/jpeg), so we hide
/// them — selecting one would break capture.
fn is_jpeg_format(choice: &str) -> bool {
    !choice.to_uppercase().contains("RAW")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_config_keys_and_ignores_unknown() {
        assert_eq!(param_type_for("iso"), Some(ParameterType::Iso));
        assert_eq!(param_type_for("shutterspeed"), Some(ParameterType::ShutterSpeed));
        assert_eq!(param_type_for("aperture"), Some(ParameterType::Aperture));
        assert_eq!(param_type_for("imageformat"), Some(ParameterType::ImageQuality));
        assert_eq!(param_type_for("focusmode"), None); // intentionally not exposed
        assert_eq!(param_type_for("somethingelse"), None);
    }

    #[test]
    fn config_key_round_trips_through_param_type_for() {
        for pt in [
            ParameterType::Iso,
            ParameterType::ShutterSpeed,
            ParameterType::Aperture,
            ParameterType::WhiteBalance,
            ParameterType::ColorTemperature,
            ParameterType::ExposureCompensation,
            ParameterType::ImageQuality,
        ] {
            let key = config_key_for(pt).expect("each mapped type has a config key");
            assert_eq!(param_type_for(key), Some(pt), "key {key:?} should round-trip");
        }
    }

    #[test]
    fn iso_auto_is_the_only_non_numeric_choice() {
        assert!(is_concrete_iso("100"));
        assert!(is_concrete_iso("6400"));
        assert!(!is_concrete_iso("Auto"));
        assert!(!is_concrete_iso("Automatique")); // localized label still detected
    }

    #[test]
    fn bulb_is_the_digitless_shutter_choice() {
        assert!(is_real_shutter_speed("30"));
        assert!(is_real_shutter_speed("1/4000"));
        assert!(is_real_shutter_speed("0.5"));
        assert!(!is_real_shutter_speed("Bulb"));
        assert!(!is_real_shutter_speed("pose longue")); // localized label still detected
    }

    #[test]
    fn raw_formats_are_filtered_out() {
        assert!(is_jpeg_format("L"));
        assert!(is_jpeg_format("cL"));
        assert!(is_jpeg_format("S2"));
        assert!(!is_jpeg_format("RAW"));
        assert!(!is_jpeg_format("RAW + L"));
        assert!(!is_jpeg_format("cRAW")); // compact RAW is still RAW
    }

    #[test]
    fn ordered_covers_numeric_progressions_only() {
        assert!(is_ordered(ParameterType::Iso));
        assert!(is_ordered(ParameterType::Aperture));
        assert!(is_ordered(ParameterType::ShutterSpeed));
        assert!(!is_ordered(ParameterType::WhiteBalance));
        assert!(!is_ordered(ParameterType::ImageQuality));
    }

    #[test]
    fn non_canon_models_are_never_deferred() {
        assert!(!owned_by_other_backend("Nikon D750"));
        assert!(!owned_by_other_backend("Sony Alpha 7"));
    }

    #[test]
    fn canon_is_deferred_only_when_edsdk_is_compiled_in() {
        // Canon bodies are handed to the EDSDK backend only when it is built in.
        assert_eq!(
            owned_by_other_backend("Canon EOS 600D"),
            cfg!(feature = "backend-canon")
        );
    }
}

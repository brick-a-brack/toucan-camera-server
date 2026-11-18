use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gphoto2::widget::{RadioWidget, RangeWidget, ToggleWidget, Widget};

use crate::camera::{CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterType};

pub mod curation;

use curation::{
    auto_iso_choice, config_keys_for, curate, is_concrete_iso, is_manual_focus_choice, is_truthy,
    leaf_params, on_off_choice, type_of, Leaf,
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
    /// Whether a body can actually drive its focus motor, keyed by native ID
    /// (port). Filled once per device by `focus_drive_works`, because the widget
    /// alone does not answer the question (see there). Never cleared: a body that
    /// refused once will refuse again, and re-probing on every parameter read
    /// would nudge the lens each time.
    focus_drive: Arc<Mutex<HashMap<String, bool>>>,
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
        Ok(Self { context, connected, focus_drive: Arc::new(Mutex::new(HashMap::new())) })
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

    /// Whether the body can really drive its focus motor — asked once, then cached.
    ///
    /// The widget alone does not answer this: libgphoto2 builds it from the PTP
    /// operations the camera *advertises*, and some bodies advertise more than they
    /// honour. The Nikon Z5 II exposes `manualfocusdrive` and then answers "not in
    /// live view" to every drive command, even with live view demonstrably running
    /// (`viewfinder` reads back on, previews stream fine). Nothing in the config
    /// tree tells the two apart, so the only honest test is to try it: drive the
    /// motor by one step and see. A step of 1 is the smallest move the body can
    /// make and is imperceptible in the preview.
    ///
    /// Called from `get_parameters`, so a body that cannot focus never advertises
    /// the control in the first place. Only bodies exposing the focus drive as a
    /// range (Nikon) are probed — a Canon-style radio has no neutral step to send,
    /// and Canon focus drive works, so those are taken at their word and fall back
    /// to learning from a refusal in `drive_focus`.
    fn focus_drive_works(&self, native_id: &str, camera: &gphoto2::Camera) -> bool {
        if let Some(known) = self.focus_drive.lock().expect("gphoto2 mutex poisoned").get(native_id)
        {
            return *known;
        }

        let Some(widget) = widget_for::<RangeWidget>(camera, ParameterType::Focus) else {
            return true;
        };

        // The motor only obeys in live view, and a preview grab is what gets the
        // body there — a no-op on one already streaming.
        let _ = camera.capture_preview().wait();
        widget.set_value(1.0);

        match camera.set_config(&widget).wait() {
            Ok(()) => {
                self.remember_focus_drive(native_id, true);
                true
            }
            Err(e) if is_focus_refusal(&e) => {
                eprintln!(
                    "[gphoto2] {native_id}: the body refuses to drive the focus motor ({e}) \
                     — hiding the Focus parameter for this device"
                );
                self.remember_focus_drive(native_id, false);
                false
            }
            // A busy or I/O error says nothing about the body's capabilities: leave
            // it unclassified and let the next parameter read ask again.
            Err(e) => {
                eprintln!("[gphoto2] {native_id}: focus-drive probe inconclusive ({e})");
                true
            }
        }
    }

    fn remember_focus_drive(&self, native_id: &str, works: bool) {
        self.focus_drive
            .lock()
            .expect("gphoto2 mutex poisoned")
            .insert(native_id.to_string(), works);
    }

    /// Drives the focus motor by a relative amount.
    ///
    /// A refusal here is the same signal `focus_drive_works` probes for, so record
    /// it: it is how the bodies that skip the probe (Canon-style radio widget) stop
    /// advertising a control they cannot honour. "Focus at limit" / "stepping too
    /// small" (`CameraError`) mean the motor *did* move, and a busy error is
    /// transient — neither counts.
    fn drive_focus(
        &self,
        native_id: &str,
        camera: &gphoto2::Camera,
        value: &str,
    ) -> Result<(), CameraError> {
        let _ = camera.capture_preview().wait();

        // Range on Nikon, Radio ("Near 1"/"Far 1"…) on Canon.
        let focus = ParameterType::Focus;
        let outcome = if let Some(widget) = widget_for::<RangeWidget>(camera, focus) {
            let v: f32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            widget.set_value(v);
            camera.set_config(&widget).wait()
        } else if let Some(widget) = widget_for::<RadioWidget>(camera, focus) {
            widget.set_choice(value).map_err(map_err)?;
            camera.set_config(&widget).wait()
        } else {
            return Err(CameraError::NotSupported);
        };

        match outcome {
            Ok(()) => Ok(()),
            Err(e) if is_focus_refusal(&e) => {
                eprintln!(
                    "[gphoto2] {native_id}: the body refused to drive the focus motor ({e}) \
                     — hiding the Focus parameter for this device"
                );
                self.remember_focus_drive(native_id, false);
                Err(CameraError::Backend(
                    "gphoto2: this camera does not support driving the focus motor".to_string(),
                ))
            }
            Err(e) => Err(map_err(e)),
        }
    }
}

/// Whether a failed focus drive means "this body cannot do it" rather than "not
/// right now". libgphoto2 reports the Nikon refusal as a bare `GP_ERROR`
/// (`Other`), and a body that never advertised the operation as `NotSupported`.
fn is_focus_refusal(err: &gphoto2::Error) -> bool {
    matches!(err.kind(), gphoto2::error::ErrorKind::Other | gphoto2::error::ErrorKind::NotSupported)
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

/// libgphoto2 errors carry a human-readable reason ("Camera is busy", "Could not
/// claim the USB device", plus any driver detail) — keep it, so a failed
/// `set_parameter` tells the user what the body actually refused instead of an
/// opaque `SDK error: 0x00000000`.
fn map_err(err: gphoto2::Error) -> CameraError {
    eprintln!("[gphoto2] error: {err}");
    CameraError::Backend(format!("gphoto2: {err}"))
}

/// libgphoto2 reports cameras it has no specific driver for under a generic
/// PTP/MTP class name (e.g. a new body shows as "USB PTP Class Camera", not its
/// model). For those we take the model from the USB product string instead, so
/// the cross-backend dedup key still matches the dedicated SDK backend's.
fn is_generic_ptp_name(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("ptp class camera") || m == "ptp camera" || m == "mtp device"
}

/// Parses a libgphoto2 USB port (`"usb:BUS,DEV"`, zero-padded decimals) into the
/// `(bus number, device address)` pair libusb/nusb use. `None` for non-USB ports.
fn parse_usb_port(port: &str) -> Option<(u8, u8)> {
    let (bus, dev) = port.strip_prefix("usb:")?.split_once(',')?;
    Some((bus.trim().parse().ok()?, dev.trim().parse().ok()?))
}

/// The libusb-style bus number for a device, matching how libgphoto2 formats the
/// port. nusb exposes this per-platform: `location_id >> 24` on macOS (IOKit),
/// `busnum` on Linux (sysfs) — the same values libusb derives.
fn device_bus(d: &nusb::DeviceInfo) -> u8 {
    #[cfg(target_os = "macos")]
    {
        (d.location_id() >> 24) as u8
    }
    #[cfg(target_os = "linux")]
    {
        d.busnum()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = d;
        0
    }
}

/// Maps `(bus number, device address)` → `(USB vendor id, USB product string)`
/// for every connected USB device via nusb (IOKit on macOS, sysfs on Linux — the
/// same numbering libgphoto2's libusb uses). The product string is the device's
/// own model name, reliable even when libgphoto2 only knows the body generically.
/// Empty on enumeration failure (then no dedup key is emitted).
fn usb_camera_map() -> HashMap<(u8, u8), (u16, Option<String>)> {
    use nusb::MaybeFuture;
    match nusb::list_devices().wait() {
        Ok(devs) => devs
            .map(|d| {
                let key = (device_bus(&d), d.device_address());
                (key, (d.vendor_id(), d.product_string().map(str::to_owned)))
            })
            .collect(),
        Err(_) => HashMap::new(),
    }
}

/// Sony's USB vendor id. The dedicated Sony CrSDK backend enumerates bodies by
/// their ILCE/ILME/ZV model number (e.g. "ILCE-7M4"), which is exactly the USB
/// product string. libgphoto2 instead reports a marketing name ("Sony Alpha-A7 IV
/// (PC Control)") that normalizes to a different token, so for Sony bodies the
/// dedup key must be built from the USB product string too — otherwise the two
/// backends never agree and the camera shows up twice.
const USB_VENDOR_SONY: u16 = 0x054c;

/// The cross-backend dedup key for a gphoto2 device: its real USB vendor (from
/// nusb) plus its model. The model is libgphoto2's own name, except when that is
/// a generic PTP/MTP class name, or the body is a Sony (see [`USB_VENDOR_SONY`]) —
/// then the USB product string is used so the key matches the one a dedicated SDK
/// backend emits for the same body. `None` when the USB device can't be resolved
/// (non-USB port, enumeration blocked), in which case it is simply never deduped.
///
/// Note: this backend knows nothing about Canon/Nikon/Sony backends — it only
/// publishes its device's identity. The server decides which backend wins.
fn gphoto_dedup_key(
    gphoto_model: &str,
    port: &str,
    usb: &HashMap<(u8, u8), (u16, Option<String>)>,
) -> Option<String> {
    let (vendor, product) = parse_usb_port(port).and_then(|key| usb.get(&key))?;
    // Prefer the USB product string when libgphoto2 only knows the body under a
    // generic class name, or for Sony bodies (whose CrSDK keys off the model
    // number == the product string, not libgphoto2's marketing name). Fall back
    // to libgphoto2's own name if no product string is available.
    let prefer_product = is_generic_ptp_name(gphoto_model) || *vendor == USB_VENDOR_SONY;
    let model = if prefer_product {
        product.as_deref().filter(|s| !s.is_empty()).unwrap_or(gphoto_model)
    } else {
        gphoto_model
    };
    Some(crate::camera::dedup_key(*vendor, model))
}

impl CameraBackend for GPhoto2Backend {
    fn backend_id(&self) -> &str {
        "gphoto2"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let cameras = self.context.list_cameras().wait().map_err(map_err)?;
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        let usb = usb_camera_map();

        if crate::camera::dedup_debug_enabled() {
            eprintln!("[dedup] nusb sees {} USB device(s): {usb:?}", usb.len());
        }

        // List every camera and tag it with its cross-backend identity. The
        // server drops duplicates a higher-priority (SDK) backend also serves.
        let devices = cameras
            .map(|d| {
                let dedup_key = gphoto_dedup_key(&d.model, &d.port, &usb);
                if crate::camera::dedup_debug_enabled() {
                    eprintln!(
                        "[dedup] gphoto2 model={:?} port={:?} -> key={dedup_key:?}",
                        d.model, d.port
                    );
                }
                DeviceInfo {
                    connected: connected.contains_key(&d.port),
                    dedup_key,
                    id: DeviceId::new("gphoto2", &d.port).encode(),
                    name: d.model,
                }
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

        // Each parameter is tagged with the config key it came from, so the dedup
        // below can keep the key `set_parameter` will write to.
        let mut tagged = Vec::new();
        for child in root.children_iter() {
            walk_widget(&child, &mut tagged);
        }

        // One entry per parameter type, the ISO selector reconciled with a separate
        // auto-ISO toggle, choice-less selectors hidden — see `curation::curate`.
        let mut params = curate(tagged);

        // A body can advertise a focus-drive widget it cannot honour, so only offer
        // the control once the body has proven it works (probed once, then cached).
        if params.iter().any(|p| type_of(p) == ParameterType::Focus)
            && !self.focus_drive_works(native_id, &camera)
        {
            params.retain(|p| type_of(p) != ParameterType::Focus);
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

        // IsoAuto is synthetic: some bodies back it with a dedicated toggle, others
        // with an "Auto" entry in the ISO list. See `set_iso_auto`.
        if param_type == ParameterType::IsoAuto {
            return set_iso_auto(&camera, is_truthy(value));
        }

        // FocusAuto is a synthetic toggle backed by the camera's "focusmode" radio
        // widget (there is no separate AF/MF boolean — MF is one focusmode value),
        // mirroring the IsoAuto handling above. true → first AF choice; false →
        // the manual choice, both queried live from the camera.
        if param_type == ParameterType::FocusAuto {
            let widget: RadioWidget = widget_for(&camera, ParameterType::FocusMode)
                .ok_or(CameraError::NotSupported)?;
            let on = is_truthy(value);
            let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
            let target = if on {
                choices.iter().find(|c| !is_manual_focus_choice(c))
            } else {
                choices.iter().find(|c| is_manual_focus_choice(c))
            }
            .ok_or(CameraError::NotSupported)?;
            widget.set_choice(target.as_str()).map_err(map_err)?;
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }

        // Focus is the one parameter a body can advertise and still be unable to
        // execute, so it gets its own path (which learns from a refusal).
        if param_type == ParameterType::Focus {
            return self.drive_focus(native_id, &camera, value);
        }

        // gphoto2 exposes the same logical parameter as Radio on most cameras but
        // as Range or Toggle on some (focus drive is a Range on Nikon, a Radio on
        // Canon), so probe each widget type over every key the parameter is known by.
        if let Some(widget) = widget_for::<RadioWidget>(&camera, param_type) {
            // An on/off radio is exposed as a Boolean, so clients send "true" /
            // "false" — translate those back to the camera's own On/Off choice.
            // Any other parameter round-trips the option value verbatim.
            let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
            let choice = on_off_choice(&choices, is_truthy(value))
                .cloned()
                .unwrap_or_else(|| value.to_string());
            widget.set_choice(choice.as_str()).map_err(map_err)?;
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }
        if let Some(widget) = widget_for::<RangeWidget>(&camera, param_type) {
            let v: f32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            widget.set_value(v);
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }
        if let Some(widget) = widget_for::<ToggleWidget>(&camera, param_type) {
            widget.set_toggled(is_truthy(value));
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }

        Err(CameraError::NotSupported)
    }
}

/// Turns auto-ISO on or off, whichever way the body models it.
///
/// Two families: Nikon keeps its ISO list concrete and exposes auto-ISO as its
/// own `autoiso` toggle widget; Canon folds an "Auto" choice into the ISO list.
/// Prefer the dedicated toggle when the body has one, otherwise fall back to
/// selecting the auto choice (turning auto off then means selecting the first
/// concrete ISO the camera offers, rather than hardcoding a value it may not have).
fn set_iso_auto(camera: &gphoto2::Camera, on: bool) -> Result<(), CameraError> {
    // A dedicated auto-ISO widget: a toggle on some drivers, an "On"/"Off" radio
    // on others — libgphoto2 models the same PTP on/off property either way.
    if let Some(toggle) = widget_for::<ToggleWidget>(camera, ParameterType::IsoAuto) {
        toggle.set_toggled(on);
        camera.set_config(&toggle).wait().map_err(map_err)?;
        return Ok(());
    }
    if let Some(radio) = widget_for::<RadioWidget>(camera, ParameterType::IsoAuto) {
        let choices: Vec<String> = radio.choices_iter().map(|c| c.to_string()).collect();
        let target = on_off_choice(&choices, on).ok_or(CameraError::NotSupported)?;
        radio.set_choice(target.as_str()).map_err(map_err)?;
        camera.set_config(&radio).wait().map_err(map_err)?;
        return Ok(());
    }

    let widget: RadioWidget =
        widget_for(camera, ParameterType::Iso).ok_or(CameraError::NotSupported)?;
    let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
    let target = if on {
        auto_iso_choice(&choices).cloned()
    } else {
        choices.iter().find(|c| is_concrete_iso(c)).cloned()
    }
    .ok_or(CameraError::NotSupported)?;

    widget.set_choice(target.as_str()).map_err(map_err)?;
    camera.set_config(&widget).wait().map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Widget tree walking → CameraParameter list
// ---------------------------------------------------------------------------

/// Walks the config tree, tagging every parameter with the config key it was read
/// from — `dedup_by_type` needs it to keep the key writes will go to.
fn walk_widget(widget: &Widget, out: &mut Vec<(String, CameraParameter)>) {
    if let Widget::Group(g) = widget {
        for child in g.children_iter() {
            walk_widget(&child, out);
        }
        return;
    }

    let key = widget.name();
    let mut leaf = Vec::new();
    walk_leaf(widget, &mut leaf);
    out.extend(leaf.into_iter().map(|param| (key.clone(), param)));
}

/// Hands one leaf widget to the shared curation (`curation::leaf_params`), as the
/// backend-neutral `Leaf` the WebAssembly build also produces from its own
/// libgphoto2 module.
fn walk_leaf(widget: &Widget, out: &mut Vec<CameraParameter>) {
    let leaf = match widget {
        Widget::Radio(r) => Leaf::Choice {
            name: r.name(),
            readonly: r.readonly(),
            current: r.choice(),
            choices: r.choices_iter().map(|c| c.to_string()).collect(),
        },
        Widget::Range(r) => {
            let (range, step) = r.range_and_step();
            Leaf::Range {
                name: r.name(),
                readonly: r.readonly(),
                value: r.value(),
                min: *range.start(),
                max: *range.end(),
                step,
            }
        }
        Widget::Toggle(t) => Leaf::Toggle {
            name: t.name(),
            readonly: t.readonly(),
            value: t.toggled(),
        },
        _ => return, // Group (walked above), Button, Date, Text → not parameters
    };
    out.extend(leaf_params(&leaf));
}

/// Fetches the camera's widget for `param_type` as `W`, trying the type's config
/// keys in `CONFIG_KEYS` order. `None` when the body exposes none of them, or
/// exposes one but not as a `W`.
///
/// A body may expose several keys of the same type, and they need not agree — see
/// `dedup_by_type`, which resolves the read path against this same order so that a
/// write always lands on the widget the client is looking at.
fn widget_for<W>(camera: &gphoto2::Camera, param_type: ParameterType) -> Option<W>
where
    W: TryFrom<Widget, Error = gphoto2::Error> + 'static + Send,
{
    config_keys_for(param_type).find_map(|key| camera.config_key::<W>(key).wait().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_enumeration_runs() {
        // Smoke test: the nusb path (IOKit on macOS, sysfs on Linux) must run
        // without panicking. Contents depend on what's plugged in / sandboxing.
        let _ = usb_camera_map();
    }

    #[test]
    fn usb_port_parsing() {
        assert_eq!(parse_usb_port("usb:020,007"), Some((20, 7))); // zero-padded
        assert_eq!(parse_usb_port("usb:1,8"), Some((1, 8)));
        assert_eq!(parse_usb_port("ptpip:192.168.1.1"), None);
        assert_eq!(parse_usb_port("usb:"), None);
        assert_eq!(parse_usb_port("disk:/x"), None);
    }

    #[test]
    fn generic_ptp_name_detection() {
        assert!(is_generic_ptp_name("USB PTP Class Camera"));
        assert!(is_generic_ptp_name("PTP Camera"));
        assert!(is_generic_ptp_name("MTP Device"));
        assert!(!is_generic_ptp_name("Nikon Z 6"));
        assert!(!is_generic_ptp_name("Canon EOS 600D"));
    }

    #[test]
    fn dedup_key_uses_product_string_for_generic_names() {
        // A new Nikon body libgphoto2 only knows generically: the key comes from
        // the USB product string and must equal the one the Nikon SDK emits.
        let mut usb = HashMap::new();
        usb.insert((20u8, 7u8), (0x04B0u16, Some("Nikon Z 5_2".to_string())));
        let key = gphoto_dedup_key("USB PTP Class Camera", "usb:020,007", &usb);
        // Nikon SDK lists the same body as "Z5_2" → same key.
        assert_eq!(key, Some(crate::camera::dedup_key(0x04B0, "Z5_2")));
    }

    #[test]
    fn dedup_key_uses_gphoto_name_when_specific() {
        // A Canon EOS libgphoto2 names specifically: keep that name (its USB
        // product string is often the generic "Canon Digital Camera"). Matches
        // the EDSDK device description.
        let mut usb = HashMap::new();
        usb.insert((20u8, 7u8), (0x04A9u16, Some("Canon Digital Camera".to_string())));
        let key = gphoto_dedup_key("Canon EOS R5", "usb:020,007", &usb);
        assert_eq!(key, Some(crate::camera::dedup_key(0x04A9, "Canon EOS R5")));
    }

    #[test]
    fn dedup_key_uses_product_string_for_sony() {
        // libgphoto2 names a Sony body by its marketing name, but the Sony CrSDK
        // keys off the ILCE model number (== the USB product string). gphoto2 must
        // use the product string so both backends agree and the α7 IV isn't shown
        // twice ("Sony Alpha-A7 IV (PC Control)" + "Sony ILCE-7M4").
        let mut usb = HashMap::new();
        usb.insert((0u8, 3u8), (USB_VENDOR_SONY, Some("ILCE-7M4".to_string())));
        let key = gphoto_dedup_key("Sony Alpha-A7 IV (PC Control)", "usb:000,003", &usb);
        assert_eq!(key, Some(crate::camera::dedup_key(USB_VENDOR_SONY, "ILCE-7M4")));
    }

    #[test]
    fn dedup_key_sony_falls_back_to_gphoto_name_without_product() {
        // No USB product string available → keep libgphoto2's name (best effort).
        let mut usb = HashMap::new();
        usb.insert((0u8, 3u8), (USB_VENDOR_SONY, None));
        let key = gphoto_dedup_key("Sony Alpha-A7 IV (PC Control)", "usb:000,003", &usb);
        assert_eq!(
            key,
            Some(crate::camera::dedup_key(USB_VENDOR_SONY, "Sony Alpha-A7 IV (PC Control)"))
        );
    }

    #[test]
    fn dedup_key_none_when_usb_unresolved() {
        // No USB entry for the port → no key → the device is never deduped.
        let usb = HashMap::new();
        assert_eq!(gphoto_dedup_key("Nikon Z 5", "usb:020,007", &usb), None);
        assert_eq!(gphoto_dedup_key("Some Camera", "ptpip:1.2.3.4", &usb), None);
    }
}

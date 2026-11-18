//! libgphoto2 parameter curation, independent of how the config tree is read.
//!
//! The native backend (`mod.rs`) reads widgets through the `gphoto2` crate; the
//! WebAssembly build (`wasm/`) reads them from its own Emscripten libgphoto2
//! module over WebUSB. Both reduce a widget to a [`Leaf`] and hand it here, so a
//! body exposes exactly the same parameters, labels and quirk workarounds on
//! both — the wasm crate `#[path]`-includes this very file.
//!
//! Only depends on `crate::camera`'s data model (`src/camera/types.rs`).

use std::collections::{HashMap, HashSet};

use crate::camera::{CameraParameter, ParameterOption, ParameterType};

/// One leaf widget of libgphoto2's config tree, reduced to what curation needs.
/// Radio and menu widgets are both a `Choice`.
#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Choice {
        name: String,
        readonly: bool,
        current: String,
        choices: Vec<String>,
    },
    Range {
        name: String,
        readonly: bool,
        value: f32,
        min: f32,
        max: f32,
        step: f32,
    },
    /// `value` is `None` when the driver reports neither on nor off (camera
    /// actions read back as 2).
    Toggle {
        name: String,
        readonly: bool,
        value: Option<bool>,
    },
}

/// The parameters one leaf widget contributes — none for a read-only or unknown
/// widget, two for the widgets split into an auto toggle + a selector (ISO,
/// focus mode).
pub fn leaf_params(leaf: &Leaf) -> Vec<CameraParameter> {
    let mut out = Vec::new();
    match leaf {
        Leaf::Choice { name, readonly, current, choices } => {
            if *readonly {
                return out;
            }
            let Some(pt) = param_type_for(name) else {
                return out;
            };
            let current = current.clone();

            // An on/off property the driver models as a two-choice radio rather
            // than a toggle (Nikon's `autoiso`) is still a Boolean to our clients.
            if on_off_choice(choices, true).is_some() {
                out.push(CameraParameter::Boolean {
                    param_type: pt,
                    current: is_on_choice(&current),
                    disabled: false,
                });
                return out;
            }

            match pt {
                // ISO is split into an IsoAuto toggle + an Iso selector, mirroring
                // the Canon backend. libgphoto2 localizes the auto label (e.g.
                // "Automatique" in French), so we detect it locale-independently:
                // the auto choice is the only non-numeric one — every real ISO
                // value parses as an integer.
                ParameterType::Iso => {
                    let auto = auto_iso_choice(choices).cloned();
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|c| Some(*c) != auto.as_ref())
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    if options.is_empty() {
                        return out;
                    }
                    // Only this body's own ISO list can say whether auto is on. When
                    // it holds no auto entry (Nikon), auto-ISO lives in a separate
                    // `autoiso` toggle widget walked on its own — leave IsoAuto to it
                    // and let `reconcile_iso_auto` disable the selector afterwards.
                    let iso_auto = match &auto {
                        Some(auto) => {
                            let on = *auto == current;
                            out.push(CameraParameter::Boolean {
                                param_type: ParameterType::IsoAuto,
                                current: on,
                                disabled: false,
                            });
                            on
                        }
                        None => false,
                    };
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
                // Focus mode: split into a FocusAuto toggle + an AF-only FocusMode
                // select, mirroring the ISO split (and the Nikon backend). The
                // manual choice ("Manual"/"MF"…) maps to FocusAuto=false; the AF
                // sub-modes (AF-S/AF-C/…) stay in the select, disabled in MF. The
                // manual jog itself is a separate "manualfocusdrive" widget (→
                // Focus, handled by the Range arm below).
                ParameterType::FocusMode => {
                    let focus_auto = !is_manual_focus_choice(&current);
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|c| !is_manual_focus_choice(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(CameraParameter::Boolean {
                        param_type: ParameterType::FocusAuto,
                        current: focus_auto,
                        disabled: false,
                    });
                    if !options.is_empty() {
                        // In MF the current isn't an AF mode; show the first as a
                        // disabled-time placeholder.
                        let mode_current = if focus_auto {
                            current
                        } else {
                            options[0].value.clone()
                        };
                        out.push(CameraParameter::Select {
                            param_type: ParameterType::FocusMode,
                            current: mode_current,
                            options,
                            disabled: !focus_auto,
                        });
                    }
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
        Leaf::Range { name, readonly, value, min, max, step } => {
            if *readonly {
                return out;
            }
            if let Some(pt) = param_type_for(name) {
                out.push(CameraParameter::Range {
                    param_type: pt,
                    current: *value as i32,
                    min: *min as i32,
                    max: *max as i32,
                    step: *step as i32,
                    disabled: false,
                });
            }
        }
        // A toggle is a plain on/off widget (e.g. Nikon's `autoiso`) → Boolean.
        Leaf::Toggle { name, readonly, value } => {
            if *readonly {
                return out;
            }
            if let Some(pt) = param_type_for(name) {
                out.push(CameraParameter::Boolean {
                    param_type: pt,
                    current: value.unwrap_or(false),
                    disabled: false,
                });
            }
        }
    }
    out
}

/// Post-processes every `(config key, parameter)` pair walked from the tree into
/// the list `get_parameters` returns: one parameter per type (`dedup_by_type`),
/// the ISO selector disabled while a separate auto-ISO toggle is on, and
/// select-type parameters with fewer than two options dropped — they offer no
/// real choice (e.g. exposure compensation in Manual mode, which the body
/// reports as a single "0").
pub fn curate(tagged: Vec<(String, CameraParameter)>) -> Vec<CameraParameter> {
    let mut params = dedup_by_type(tagged);
    reconcile_iso_auto(&mut params);
    params.retain(|p| match p {
        CameraParameter::Select { options, .. } | CameraParameter::RangeSelect { options, .. } => {
            options.len() >= 2
        }
        _ => true,
    });
    params
}

// ---------------------------------------------------------------------------
// Mapping between gphoto2 config-key names and our ParameterType enum.
//
// Cameras name the same logical parameter differently depending on the camlib
// (Nikon exposes aperture as "f-number", Canon as "aperture"), so several keys
// can map to one type. This table is the single source of truth for both
// directions: reads look a key up with `param_type_for`, writes probe every key
// a type is known by with `config_keys_for`. Extend it as you observe new names.
// ---------------------------------------------------------------------------

pub const CONFIG_KEYS: &[(&str, ParameterType)] = &[
    ("iso", ParameterType::Iso),
    ("isospeed", ParameterType::Iso),
    ("iso speed", ParameterType::Iso),
    ("iso_speed", ParameterType::Iso),
    // Nikon bodies keep the ISO list concrete and expose auto-ISO as its own
    // toggle widget; Canon folds an "Auto" choice into the ISO list instead
    // (handled in `walk_widget` / `set_iso_auto`).
    ("autoiso", ParameterType::IsoAuto),
    ("isoauto", ParameterType::IsoAuto),
    ("iso_auto", ParameterType::IsoAuto),
    ("shutterspeed", ParameterType::ShutterSpeed),
    ("shutterspeed2", ParameterType::ShutterSpeed),
    ("shutter_speed", ParameterType::ShutterSpeed),
    ("shutter speed", ParameterType::ShutterSpeed),
    ("aperture", ParameterType::Aperture),
    ("f-number", ParameterType::Aperture),
    ("f_number", ParameterType::Aperture),
    ("fnumber", ParameterType::Aperture),
    ("whitebalance", ParameterType::WhiteBalance),
    ("white_balance", ParameterType::WhiteBalance),
    ("white balance", ParameterType::WhiteBalance),
    ("colortemperature", ParameterType::ColorTemperature),
    ("color_temperature", ParameterType::ColorTemperature),
    ("exposurecompensation", ParameterType::ExposureCompensation),
    ("exposure_compensation", ParameterType::ExposureCompensation),
    ("imageformat", ParameterType::ImageQuality),
    ("image_format", ParameterType::ImageQuality),
    ("imagequality", ParameterType::ImageQuality),
    ("image_quality", ParameterType::ImageQuality),
    // Focus mode (AF-S/AF-C/Manual…) → split into FocusAuto + FocusMode (see
    // walk_widget). "manualfocusdrive" is the relative manual-focus jog → Focus.
    ("focusmode", ParameterType::FocusMode),
    ("focusmode2", ParameterType::FocusMode),
    ("focus_mode", ParameterType::FocusMode),
    ("manualfocusdrive", ParameterType::Focus),
    ("manual_focus_drive", ParameterType::Focus),
];

pub fn param_type_for(name: &str) -> Option<ParameterType> {
    let name = name.to_ascii_lowercase();
    CONFIG_KEYS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, pt)| *pt)
}

/// Every config key `param_type` is known by, in preference order.
pub fn config_keys_for(param_type: ParameterType) -> impl Iterator<Item = &'static str> {
    CONFIG_KEYS
        .iter()
        .filter(move |(_, pt)| *pt == param_type)
        .map(|(key, _)| *key)
}

/// Boolean values arrive as strings from the API (`"true"` / `"false"`), but be
/// lenient about what clients send.
pub fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Whether a radio choice denotes the "on" / "off" state of an on/off property.
/// libgphoto2 spells them "On"/"Off" (English under the `LC_ALL=C` we force in
/// `new()`), some drivers "1"/"0".
pub fn is_on_choice(choice: &str) -> bool {
    matches!(choice.trim().to_ascii_lowercase().as_str(), "on" | "1")
}

pub fn is_off_choice(choice: &str) -> bool {
    matches!(choice.trim().to_ascii_lowercase().as_str(), "off" | "0")
}

/// The choice standing for `on` in an on/off radio, or `None` when `choices` is
/// not such a pair. libgphoto2 models the same PTP on/off property as a toggle
/// widget on some drivers and as a two-choice radio on others, so both shapes
/// have to be recognised and mapped onto our `Boolean` parameter kind.
pub fn on_off_choice(choices: &[String], on: bool) -> Option<&String> {
    let is_on_off_pair = choices.len() == 2
        && choices.iter().any(|c| is_on_choice(c))
        && choices.iter().any(|c| is_off_choice(c));
    if !is_on_off_pair {
        return None;
    }
    choices
        .iter()
        .find(|c| if on { is_on_choice(c) } else { is_off_choice(c) })
}

/// True when a gphoto2 `focusmode` choice denotes manual focus — the one mode
/// that is NOT autofocus. The label is localized by libgphoto2 (LC_ALL=C is set
/// in `new()`, but be defensive), so match "manual"/"mf" case-insensitively.
/// Mirrors the Nikon backend's `is_manual_focus`.
pub fn is_manual_focus_choice(choice: &str) -> bool {
    let c = choice.trim().to_ascii_lowercase();
    c == "mf" || c.contains("manual")
}

// ---------------------------------------------------------------------------
// Post-processing of the walked parameter list
// ---------------------------------------------------------------------------

pub fn type_of(param: &CameraParameter) -> ParameterType {
    match param {
        CameraParameter::Boolean { param_type, .. }
        | CameraParameter::Range { param_type, .. }
        | CameraParameter::Select { param_type, .. }
        | CameraParameter::RangeSelect { param_type, .. } => *param_type,
    }
}

/// Reduces the walked `(config key, parameter)` pairs to one parameter per type,
/// keeping the one read from the key `CONFIG_KEYS` ranks first — the same key
/// `widget_for` will write to.
///
/// A body can expose the same logical parameter under two keys, and they need not
/// behave the same. The Nikon Z5 II has *both* auto-ISO spellings, as two distinct
/// PTP properties that disagree: `autoiso` (`ISOAuto`) writes and reads back fine,
/// while `isoauto` (`ISO_Auto`) is stuck on "On" and rejects every write with "Bad
/// parameters". Keeping whichever came first in the camera's config tree meant
/// reading `isoauto` while writing `autoiso`: turning auto-ISO off silently
/// succeeded on one property and the next refresh re-read the other, still "On".
/// Nikon's `focusmode` / `focusmode2` pair is resolved the same way.
///
/// A parameter the walk synthesized rather than read from a key of its own (the
/// IsoAuto folded into a Canon ISO list) ranks last, so a body that also has a
/// dedicated widget for it wins — that widget is the one a write can reach.
pub fn dedup_by_type(tagged: Vec<(String, CameraParameter)>) -> Vec<CameraParameter> {
    let rank = |key: &str, param_type: ParameterType| {
        CONFIG_KEYS
            .iter()
            .position(|(k, pt)| *k == key && *pt == param_type)
            .unwrap_or(usize::MAX)
    };

    let mut best: HashMap<ParameterType, usize> = HashMap::new();
    for (key, param) in &tagged {
        let param_type = type_of(param);
        let r = rank(key, param_type);
        best.entry(param_type).and_modify(|b| *b = (*b).min(r)).or_insert(r);
    }

    let mut kept: HashSet<ParameterType> = HashSet::new();
    tagged
        .into_iter()
        .filter(|(key, param)| {
            let param_type = type_of(param);
            rank(key, param_type) == best[&param_type] && kept.insert(param_type)
        })
        .map(|(_, param)| param)
        .collect()
}

/// Marks the ISO selector read-only while auto-ISO is on. Needed for the bodies
/// that back IsoAuto with a standalone toggle widget (Nikon): the ISO list is a
/// different widget and cannot know about it on its own. A no-op for the bodies
/// whose ISO list carries its own "Auto" choice — `walk_widget` already set the
/// flag there.
pub fn reconcile_iso_auto(params: &mut [CameraParameter]) {
    let auto_on = params.iter().any(|p| {
        matches!(
            p,
            CameraParameter::Boolean {
                param_type: ParameterType::IsoAuto,
                current: true,
                ..
            }
        )
    });
    if !auto_on {
        return;
    }
    for param in params.iter_mut() {
        if let CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            disabled,
            ..
        } = param
        {
            *disabled = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Value classification — locale-independent, since libgphoto2 localizes labels.
// ---------------------------------------------------------------------------

/// The "Auto" entry of an ISO choice list, if the body has one (Canon does,
/// Nikon does not — it has a separate `autoiso` toggle instead).
///
/// libgphoto2 localizes the label ("Auto" / "Automatique"…), so we identify it
/// by shape: every real ISO is a plain integer, so the auto entry is the only
/// non-numeric choice. Bodies that list extended values ("Hi 1", "Lo 0.3") have
/// several non-numeric choices and no auto entry — then none of them is auto.
pub fn auto_iso_choice(choices: &[String]) -> Option<&String> {
    let mut non_numeric = choices.iter().filter(|c| !is_concrete_iso(c));
    let candidate = non_numeric.next()?;
    non_numeric.next().is_none().then_some(candidate)
}

/// A concrete (non-auto) ISO value: a plain integer, as every real ISO is.
pub fn is_concrete_iso(choice: &str) -> bool {
    choice.parse::<u32>().is_ok()
}

/// A real shutter speed. The bulb entry is localized too ("Bulb" / "pose
/// longue"), but unlike every real speed ("30", "1/60", "0.5") it has no digit.
pub fn is_real_shutter_speed(choice: &str) -> bool {
    choice.chars().any(|c| c.is_ascii_digit())
}

/// A JPEG image-quality choice. Capture is hardcoded to image/jpeg, so every mode
/// that writes a raw file — alone or alongside a JPEG — is hidden: selecting one
/// would break capture.
///
/// Matching on "RAW" alone is not enough. Vendors name the format after their own
/// extension and only sometimes spell out "raw": the Nikon Z5 II offers "NEF
/// (Raw)" but also "NEF+Basic" and "NEF+Fine", which are RAW+JPEG modes with no
/// "raw" in the label. So match the raw extensions themselves.
///
/// Choices the driver could not decode ("Unknown value 0003") are hidden too: they
/// may well be raw modes, and there is no way to tell — offering one would be a
/// coin flip on whether capture still works.
pub fn is_jpeg_format(choice: &str) -> bool {
    const RAW_FORMATS: &[&str] = &[
        "RAW", // generic, and Canon's cRAW
        "NEF", "NRW", // Nikon
        "CR2", "CR3", "CRW", // Canon
        "ARW", "SR2", // Sony
        "ORF", // Olympus / OM System
        "RW2", // Panasonic
        "RAF", // Fujifilm
        "PEF", "DNG", // Pentax
    ];
    let upper = choice.to_uppercase();
    !RAW_FORMATS.iter().any(|raw| upper.contains(raw)) && !upper.starts_with("UNKNOWN VALUE")
}

/// Parameters whose values form an ordered numeric progression (ISO, aperture,
/// shutter speed, exposure compensation) should render as `RangeSelect` so the
/// UI knows the order is meaningful.
pub fn is_ordered(pt: ParameterType) -> bool {
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
        assert_eq!(param_type_for("autoiso"), Some(ParameterType::IsoAuto));
        assert_eq!(param_type_for("shutterspeed"), Some(ParameterType::ShutterSpeed));
        assert_eq!(param_type_for("aperture"), Some(ParameterType::Aperture));
        assert_eq!(param_type_for("imageformat"), Some(ParameterType::ImageQuality));
        assert_eq!(param_type_for("focusmode"), Some(ParameterType::FocusMode));
        assert_eq!(param_type_for("manualfocusdrive"), Some(ParameterType::Focus));
        assert_eq!(param_type_for("somethingelse"), None);
    }

    /// The Nikon spellings must be writable, not just readable: `set_parameter`
    /// looks the widget up by every key of the type, and Nikon names aperture
    /// "f-number" (issue #28 — "Failed to set Aperture: unknown parameter type").
    #[test]
    fn vendor_key_aliases_are_reachable_from_the_param_type() {
        let keys: Vec<&str> = config_keys_for(ParameterType::Aperture).collect();
        assert!(keys.contains(&"aperture")); // Canon
        assert!(keys.contains(&"f-number")); // Nikon
    }

    #[test]
    fn config_keys_round_trip_through_param_type_for() {
        for (key, pt) in CONFIG_KEYS {
            assert_eq!(param_type_for(key), Some(*pt), "key {key:?} should round-trip");
            assert!(
                config_keys_for(*pt).any(|k| k == *key),
                "key {key:?} should be probed for {pt:?}"
            );
        }
    }

    #[test]
    fn every_settable_param_type_has_at_least_one_key() {
        for pt in [
            ParameterType::Iso,
            ParameterType::IsoAuto,
            ParameterType::ShutterSpeed,
            ParameterType::Aperture,
            ParameterType::WhiteBalance,
            ParameterType::ColorTemperature,
            ParameterType::ExposureCompensation,
            ParameterType::ImageQuality,
            ParameterType::FocusMode,
            ParameterType::Focus,
        ] {
            assert!(config_keys_for(pt).next().is_some(), "{pt:?} has no config key");
        }
    }

    #[test]
    fn truthy_values() {
        assert!(is_truthy("true"));
        assert!(is_truthy("True"));
        assert!(is_truthy("1"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy("0"));
    }

    #[test]
    fn manual_focus_choice_detection() {
        assert!(is_manual_focus_choice("Manual"));
        assert!(is_manual_focus_choice("MF"));
        assert!(is_manual_focus_choice("Manual focus"));
        assert!(!is_manual_focus_choice("AF-S"));
        assert!(!is_manual_focus_choice("AF-C"));
        assert!(!is_manual_focus_choice("AI Servo"));
    }

    #[test]
    fn iso_auto_is_the_only_non_numeric_choice() {
        assert!(is_concrete_iso("100"));
        assert!(is_concrete_iso("6400"));
        assert!(!is_concrete_iso("Auto"));
        assert!(!is_concrete_iso("Automatique")); // localized label still detected
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    /// Drivers model an on/off property either as a toggle or as an On/Off radio
    /// (Nikon's `autoiso`); both must map onto our Boolean parameter kind.
    #[test]
    fn on_off_radio_pairs_map_to_booleans() {
        let on_off = strings(&["On", "Off"]);
        assert_eq!(on_off_choice(&on_off, true), Some(&"On".to_string()));
        assert_eq!(on_off_choice(&on_off, false), Some(&"Off".to_string()));

        let numeric = strings(&["0", "1"]);
        assert_eq!(on_off_choice(&numeric, true), Some(&"1".to_string()));

        // A real multi-choice list is not a boolean, even one holding "off".
        let wb = strings(&["Auto", "Daylight", "Off"]);
        assert_eq!(on_off_choice(&wb, true), None);
        let iso = strings(&["100", "200"]);
        assert_eq!(on_off_choice(&iso, true), None);
    }

    #[test]
    fn auto_iso_choice_detection() {
        // Canon-style list: the auto entry is the only non-numeric choice, whatever
        // libgphoto2 localized it to.
        let canon = strings(&["Auto", "100", "200", "6400"]);
        assert_eq!(auto_iso_choice(&canon), Some(&"Auto".to_string()));
        let localized = strings(&["Automatique", "100", "200"]);
        assert_eq!(auto_iso_choice(&localized), Some(&"Automatique".to_string()));

        // Nikon-style list: concrete values only — auto-ISO is a separate toggle.
        let nikon = strings(&["100", "200", "25600"]);
        assert_eq!(auto_iso_choice(&nikon), None);

        // Extended values are not an auto entry: several non-numeric choices → none.
        let extended = strings(&["100", "25600", "Hi 1", "Hi 2"]);
        assert_eq!(auto_iso_choice(&extended), None);
    }

    /// Bodies that expose auto-ISO as a standalone toggle (Nikon) walk it as a
    /// separate widget, so the ISO selector only learns it is read-only here.
    #[test]
    fn iso_selector_is_disabled_while_the_auto_iso_toggle_is_on() {
        let iso = || CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            current: "100".into(),
            options: vec![ParameterOption { label: "100".into(), value: "100".into() }],
            disabled: false,
        };
        let auto = |current| CameraParameter::Boolean {
            param_type: ParameterType::IsoAuto,
            current,
            disabled: false,
        };

        let mut on = vec![iso(), auto(true)];
        reconcile_iso_auto(&mut on);
        assert!(matches!(on[0], CameraParameter::RangeSelect { disabled: true, .. }));

        let mut off = vec![iso(), auto(false)];
        reconcile_iso_auto(&mut off);
        assert!(matches!(off[0], CameraParameter::RangeSelect { disabled: false, .. }));
    }

    /// Nikon exposes both `focusmode` and `focusmode2`; only one control should
    /// reach the client, and it must be the key `CONFIG_KEYS` ranks first — the one
    /// `set_parameter` writes to.
    #[test]
    fn duplicate_parameter_types_are_dropped() {
        let mode = |key: &str, current: &str| {
            (
                key.to_string(),
                CameraParameter::Select {
                    param_type: ParameterType::FocusMode,
                    current: current.into(),
                    options: vec![],
                    disabled: false,
                },
            )
        };
        // Tree order puts `focusmode2` first; `CONFIG_KEYS` prefers `focusmode`.
        let params = dedup_by_type(vec![
            mode("focusmode2", "AF-C"),
            mode("focusmode", "AF-S"),
            ("iso".to_string(), iso_param()),
        ]);
        assert_eq!(params.len(), 2);
        assert!(params
            .iter()
            .any(|p| matches!(p, CameraParameter::Select { current, .. } if current == "AF-S")));
    }

    /// The Nikon Z5 II exposes two auto-ISO properties that disagree: `autoiso` is
    /// the writable one, `isoauto` is stuck on. Reading the stuck one while writing
    /// the other made "turn auto-ISO off" look like a no-op, so the read must land
    /// on the key `CONFIG_KEYS` prefers regardless of the camera's tree order.
    #[test]
    fn auto_iso_read_follows_the_key_writes_go_to() {
        let toggle = |key: &str, current: bool| {
            (
                key.to_string(),
                CameraParameter::Boolean {
                    param_type: ParameterType::IsoAuto,
                    current,
                    disabled: false,
                },
            )
        };
        let params = dedup_by_type(vec![toggle("isoauto", true), toggle("autoiso", false)]);
        assert_eq!(params.len(), 1);
        assert!(matches!(params[0], CameraParameter::Boolean { current: false, .. }));
    }

    /// A parameter the walk synthesized from another widget's choices (Canon folds
    /// "Auto" into its ISO list) has no key of its own, so it must not outrank a
    /// body's dedicated widget — but it must still survive when it stands alone.
    #[test]
    fn folded_iso_auto_is_kept_when_it_is_the_only_one() {
        let folded = (
            "iso".to_string(),
            CameraParameter::Boolean {
                param_type: ParameterType::IsoAuto,
                current: true,
                disabled: false,
            },
        );
        let params = dedup_by_type(vec![folded]);
        assert_eq!(params.len(), 1);
        assert!(matches!(params[0], CameraParameter::Boolean { current: true, .. }));
    }

    fn iso_param() -> CameraParameter {
        CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            current: "100".into(),
            options: vec![],
            disabled: false,
        }
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

    /// The Nikon Z5 II names its raw modes after the NEF extension, and only one of
    /// them says "Raw" — the RAW+JPEG pairs do not.
    #[test]
    fn vendor_raw_extensions_are_filtered_out() {
        assert!(is_jpeg_format("JPEG Basic"));
        assert!(is_jpeg_format("JPEG Normal"));
        assert!(is_jpeg_format("JPEG Fine"));

        assert!(!is_jpeg_format("NEF (Raw)"));
        assert!(!is_jpeg_format("NEF+Basic"));
        assert!(!is_jpeg_format("NEF+Fine"));
        assert!(!is_jpeg_format("ARW"), "Sony raw");
        assert!(!is_jpeg_format("RAF+F"), "Fujifilm raw + JPEG");
    }

    /// Choices libgphoto2 could not decode may be raw modes — never offer them.
    #[test]
    fn undecoded_choices_are_filtered_out() {
        assert!(!is_jpeg_format("Unknown value 0003"));
        assert!(!is_jpeg_format("unknown value 000a"));
    }

    #[test]
    fn ordered_covers_numeric_progressions_only() {
        assert!(is_ordered(ParameterType::Iso));
        assert!(is_ordered(ParameterType::Aperture));
        assert!(is_ordered(ParameterType::ShutterSpeed));
        assert!(!is_ordered(ParameterType::WhiteBalance));
        assert!(!is_ordered(ParameterType::ImageQuality));
    }
}

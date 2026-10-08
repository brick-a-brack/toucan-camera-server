//! Device-orientation handling: the angle arithmetic shared by every path, JPEG
//! header inspection, and lossless (DCT-domain) JPEG rotation.
//!
//! Only the Android backend uses this today — it is the one platform whose camera
//! is bolted to a device the user turns around — but nothing here is
//! Android-specific, so it lives next to the `CameraBackend` trait rather than
//! inside the backend.
//!
//! Three consumers, one angle:
//!
//! - the live-view path rotates the packed RGB buffer in the C bridge, while
//!   converting YUV to RGB (free: only the destination index changes);
//! - the photo path asks the camera HAL to do it via `ACAMERA_JPEG_ORIENTATION`
//!   (free too — but the HAL is allowed to just write an EXIF tag instead);
//! - when the HAL only tagged EXIF, [`rotate_jpeg`] turns the pixels — without
//!   decoding them where the dimensions allow it, by re-encoding where they do
//!   not. Either way the whole image is rotated; nothing is ever cropped.

use std::sync::atomic::{AtomicI32, Ordering};

/// The device orientation is unknown — the phone is flat on a table, or no host
/// ever reported one. Mirrors `OrientationEventListener.ORIENTATION_UNKNOWN`.
pub const ORIENTATION_UNKNOWN: i32 = -1;

/// Current device orientation in degrees (0-359), or [`ORIENTATION_UNKNOWN`].
///
/// Process-global rather than per-session: it describes the device, not a camera,
/// and the Android NDK cannot read it — only the Java layer can, which pushes it
/// here through the `setDeviceRotation` JNI call.
static DEVICE_ORIENTATION: AtomicI32 = AtomicI32::new(ORIENTATION_UNKNOWN);

/// Records the device orientation reported by the host application.
///
/// Accepts the raw 0-359 value of `OrientationEventListener` (and its
/// `ORIENTATION_UNKNOWN`); anything else is clamped to unknown, which means "do
/// not rotate" everywhere downstream.
pub fn set_device_orientation(degrees: i32) {
    let value = if (0..360).contains(&degrees) {
        degrees
    } else {
        ORIENTATION_UNKNOWN
    };
    DEVICE_ORIENTATION.store(value, Ordering::Relaxed);
}

/// The last device orientation reported, or [`ORIENTATION_UNKNOWN`].
pub fn device_orientation() -> i32 {
    DEVICE_ORIENTATION.load(Ordering::Relaxed)
}

/// Clockwise rotation, in degrees, that an image coming out of the sensor needs
/// so it is upright for a user holding the device at `device_orientation`.
///
/// This is the reference implementation documented for
/// `ACAMERA_JPEG_ORIENTATION` in the NDK headers
/// (`NdkCameraMetadataTags.h`, `getJpegOrientation`), so `device_orientation`
/// must use `OrientationEventListener`'s convention — **not** the
/// `Surface.ROTATION_*` constants of `Display.rotation`, whose sign is opposite.
///
/// `sensor_orientation` is the camera's fixed mounting angle
/// (`ACAMERA_SENSOR_ORIENTATION`), always 0, 90, 180 or 270. External cameras
/// report 0 and must not be rotated — the caller decides that by passing
/// [`ORIENTATION_UNKNOWN`].
pub fn jpeg_orientation(sensor_orientation: i32, device_orientation: i32, front_facing: bool) -> i32 {
    if device_orientation == ORIENTATION_UNKNOWN {
        return 0;
    }

    // Round the device orientation to a multiple of 90.
    let mut device = device_orientation.rem_euclid(360);
    device = (device + 45) / 90 * 90;

    // A front sensor is mirrored, so the correction runs the other way.
    if front_facing {
        device = -device;
    }

    (sensor_orientation + device).rem_euclid(360)
}

/// Normalises an arbitrary angle to one of 0, 90, 180, 270.
pub fn quantize(degrees: i32) -> i32 {
    let d = degrees.rem_euclid(360);
    (d + 45) / 90 * 90 % 360
}

// ---------------------------------------------------------------------------
// JPEG header inspection
// ---------------------------------------------------------------------------

/// The EXIF `Orientation` tag (0x0112) of a JPEG, and where its value sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExifOrientation {
    /// Tag value: 1 = upright, 3 = 180°, 6 = 90° CW, 8 = 270° CW.
    pub value: u16,
    /// Absolute offset of the 2-byte value inside the JPEG.
    pub offset: usize,
    /// Byte order of the enclosing TIFF header.
    pub little_endian: bool,
}

/// The EXIF `Orientation` value matching a clockwise rotation, if it has one.
pub fn exif_tag_for(degrees: i32) -> Option<u16> {
    match quantize(degrees) {
        0 => Some(1),
        90 => Some(6),
        180 => Some(3),
        270 => Some(8),
        _ => None,
    }
}

/// Real pixel dimensions of a JPEG, read from its `SOF` marker.
///
/// A header scan: a few dozen bytes, no entropy decoding. Needed because an
/// `AImageReader` reports the size it was *created* with — the NDK documents that
/// `AImage_getWidth`/`getHeight` are **not** updated when the HAL rotates the
/// image data, so they cannot tell us what actually came back.
pub fn jpeg_pixel_size(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = jpeg_first_marker(data)?;

    while i + 1 < data.len() {
        let (marker, payload, next) = jpeg_marker_at(data, i)?;
        match marker {
            // SOF0..SOF15, except the non-frame markers interleaved in that range
            // (DHT 0xC4, JPGx 0xC8, DAC 0xCC).
            0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                // payload: precision(1) height(2) width(2)
                if payload + 5 > data.len() {
                    return None;
                }
                let height = u16::from_be_bytes([data[payload + 1], data[payload + 2]]) as u32;
                let width = u16::from_be_bytes([data[payload + 3], data[payload + 4]]) as u32;
                return Some((width, height));
            }
            // Start of scan: no frame header found before the pixel data.
            0xDA => return None,
            _ => i = next,
        }
    }
    None
}

/// Reads the EXIF `Orientation` tag out of a JPEG's `APP1` segment.
pub fn exif_orientation(data: &[u8]) -> Option<ExifOrientation> {
    let mut i = jpeg_first_marker(data)?;

    while i + 1 < data.len() {
        let (marker, payload, next) = jpeg_marker_at(data, i)?;
        if marker == 0xDA {
            return None; // pixel data reached
        }
        if marker == 0xE1 {
            // APP1 payload: "Exif\0\0" then a TIFF header.
            if payload + 6 <= data.len() && &data[payload..payload + 6] == b"Exif\0\0" {
                if let Some(found) = exif_orientation_in_tiff(data, payload + 6, next) {
                    return Some(found);
                }
            }
        }
        i = next;
    }
    None
}

/// Rewrites a JPEG's EXIF `Orientation` tag to 1 (upright).
///
/// Called after the pixels have actually been turned: leaving the HAL's tag in
/// place would make every EXIF-aware viewer rotate the image a second time.
/// Returns whether a tag was found and changed.
pub fn reset_exif_orientation(data: &mut [u8]) -> bool {
    let Some(found) = exif_orientation(data) else {
        return false;
    };
    if found.offset + 2 > data.len() {
        return false;
    }
    let bytes = if found.little_endian {
        1u16.to_le_bytes()
    } else {
        1u16.to_be_bytes()
    };
    data[found.offset..found.offset + 2].copy_from_slice(&bytes);
    true
}

/// What still has to be done to a JPEG the camera HAL returned, after we asked it
/// for `requested` degrees through `ACAMERA_JPEG_ORIENTATION`.
///
/// The HAL is free to either turn the pixels or merely write the EXIF tag, and
/// there is no way to ask in advance — so we look at what came back.
/// `reader_width`/`reader_height` are the dimensions the `AImageReader` was
/// created with, i.e. the sensor-native framing before any rotation.
///
/// Returns 0 when the image is already upright.
pub fn residual_rotation(
    jpeg: &[u8],
    requested: i32,
    reader_width: i32,
    reader_height: i32,
) -> i32 {
    let requested = quantize(requested);
    if requested == 0 {
        return 0;
    }

    // The HAL only tagged EXIF: the tag mirrors exactly what we asked for.
    if let Some(found) = exif_orientation(jpeg) {
        if exif_tag_for(requested) == Some(found.value) {
            return requested;
        }
    }

    // For a quarter turn the real pixel dimensions settle it: unchanged means the
    // HAL left the pixels alone, swapped means it turned them.
    if requested == 90 || requested == 270 {
        if let Some((width, height)) = jpeg_pixel_size(jpeg) {
            if width as i32 == reader_width && height as i32 == reader_height {
                return requested;
            }
            return 0;
        }
    }

    // 180° with a clean (or absent) EXIF tag: the HAL rotated the pixels.
    0
}

/// Offset of the first marker's identifier byte, skipping the `SOI`.
fn jpeg_first_marker(data: &[u8]) -> Option<usize> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    Some(2)
}

/// Reads the marker starting at `i` (which must point at its `0xFF` prefix) and
/// returns `(identifier, offset of its payload, offset of the next marker)`.
///
/// The payload offset is returned rather than derived from `i` because any
/// number of `0xFF` fill bytes may sit between `i` and the identifier.
fn jpeg_marker_at(data: &[u8], mut i: usize) -> Option<(u8, usize, usize)> {
    while i < data.len() && data[i] == 0xFF {
        i += 1;
    }
    let marker = *data.get(i)?;
    i += 1;

    // Standalone markers carry no payload.
    if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
        return Some((marker, i, i));
    }
    let length = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
    if length < 2 {
        return None;
    }
    // The length counts its own two bytes, so the payload follows them.
    Some((marker, i + 2, i + length))
}

/// Walks a TIFF header starting at `tiff` (bounded by `end`) looking for IFD0's
/// `Orientation` entry.
fn exif_orientation_in_tiff(data: &[u8], tiff: usize, end: usize) -> Option<ExifOrientation> {
    let end = end.min(data.len());
    if tiff + 8 > end {
        return None;
    }
    let little_endian = match &data[tiff..tiff + 2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b = [*data.get(at)?, *data.get(at + 1)?];
        Some(if little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b = [
            *data.get(at)?,
            *data.get(at + 1)?,
            *data.get(at + 2)?,
            *data.get(at + 3)?,
        ];
        Some(if little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };

    let ifd = tiff + u32_at(tiff + 4)? as usize;
    if ifd + 2 > end {
        return None;
    }
    let count = u16_at(ifd)? as usize;

    for n in 0..count {
        let entry = ifd + 2 + n * 12;
        if entry + 12 > end {
            return None;
        }
        if u16_at(entry)? != 0x0112 {
            continue;
        }
        // SHORT value, stored in the first two bytes of the 4-byte value field.
        let offset = entry + 8;
        return Some(ExifOrientation {
            value: u16_at(offset)?,
            offset,
            little_endian,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// JPEG rotation
// ---------------------------------------------------------------------------

/// Quality used when a rotation has to go through a re-encode. Matches the
/// frame-averaging path in `routes::cameras`, the other place that re-encodes a
/// camera JPEG.
const JPEG_REENCODE_QUALITY: u8 = 95;

/// Why [`rotate_jpeg_lossless`] gave up — the dimensions are not a whole number
/// of MCUs, so no exact coefficient permutation exists.
const NOT_MCU_ALIGNED: &str =
    "dimensions are not a whole number of MCUs, so no exact transformation exists";

/// Which route [`rotate_jpeg`] took, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationPath {
    /// Nothing to do — the angle was zero.
    Unchanged,
    /// Coefficients permuted in place: no decoding, no quality loss.
    Lossless,
    /// Decoded, rotated and re-encoded, because the dimensions are not a whole
    /// number of MCUs.
    Reencoded,
}

/// Rotates a JPEG clockwise by 0, 90, 180 or 270 degrees, **whole image, never
/// cropped**.
///
/// Prefers the lossless route (coefficients permuted, no decoding — see
/// [`rotate_jpeg_lossless`]). That only works when both dimensions are a whole
/// number of MCUs, which a camera still often is not: 4000x3000 at 4:2:0 has a
/// 16-pixel MCU and 3000 is not a multiple of 16. `jpegtran` would trim the
/// partial edge there; we re-encode the full image instead, so the output always
/// has exactly the input's pixels, turned.
pub fn rotate_jpeg(jpeg: &[u8], degrees: i32) -> Result<(Vec<u8>, RotationPath), String> {
    if quantize(degrees) == 0 {
        return Ok((jpeg.to_vec(), RotationPath::Unchanged));
    }
    match rotate_jpeg_lossless(jpeg, degrees) {
        Ok(out) => Ok((out, RotationPath::Lossless)),
        Err(lossless_error) => match rotate_jpeg_reencoded(jpeg, degrees) {
            Ok(out) => Ok((out, RotationPath::Reencoded)),
            // Both failed: report the re-encode's reason, which is the one that
            // actually describes a broken image rather than an awkward size.
            Err(reencode_error) => Err(format!(
                "{reencode_error} (lossless route: {lossless_error})"
            )),
        },
    }
}

/// Rotates a JPEG by decoding it, turning the pixels and re-encoding.
///
/// The fallback for dimensions the lossless route cannot handle exactly. Costs a
/// full decode plus a full encode and one generation of JPEG loss, but keeps
/// every pixel. EXIF does not survive, which is correct here: the orientation it
/// described is now baked into the pixels.
fn rotate_jpeg_reencoded(jpeg: &[u8], degrees: i32) -> Result<Vec<u8>, String> {
    let decoded = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .map_err(|e| format!("could not decode the JPEG: {e}"))?
        .into_rgb8();
    let (width, height) = (decoded.width(), decoded.height());

    encode_rotated_rgb(&decoded, width, height, quantize(degrees))
}

/// Encodes packed RGB24 as a baseline JPEG, rotating it on the way.
///
/// Hands the work to libjpeg-turbo: the encoder is where a re-encode spends its
/// time (263 ms of a 350 ms round trip on a 12 Mpx still, against 55 ms for the
/// decode), and the pure-Rust encoder has no SIMD while libjpeg-turbo brings NEON
/// on ARM. Rotating inside the scanline loop also saves a whole pass and a
/// second full-size buffer.
#[cfg(feature = "jpeg-rotate")]
fn encode_rotated_rgb(rgb: &[u8], width: u32, height: u32, degrees: i32) -> Result<Vec<u8>, String> {
    use std::os::raw::{c_int, c_uchar};

    extern "C" {
        fn tc_encode_rotated_rgb(
            rgb: *const c_uchar,
            width: c_int,
            height: c_int,
            degrees: c_int,
            quality: c_int,
            out: *mut *mut c_uchar,
            out_len: *mut usize,
            error_code: *mut c_int,
        ) -> c_int;
        fn tc_free_jpeg(data: *mut c_uchar);
    }

    let expected = width as usize * height as usize * 3;
    if rgb.len() < expected {
        return Err(format!(
            "RGB buffer is {} bytes, expected {expected} for {width}x{height}",
            rgb.len()
        ));
    }

    let mut out: *mut c_uchar = std::ptr::null_mut();
    let mut out_len: usize = 0;
    let mut error_code: c_int = 0;

    let status = unsafe {
        tc_encode_rotated_rgb(
            rgb.as_ptr(),
            width as c_int,
            height as c_int,
            degrees,
            JPEG_REENCODE_QUALITY as c_int,
            &mut out,
            &mut out_len,
            &mut error_code,
        )
    };

    if status == 0 && !out.is_null() && out_len > 0 {
        let encoded = unsafe { std::slice::from_raw_parts(out, out_len).to_vec() };
        unsafe { tc_free_jpeg(out) };
        Ok(encoded)
    } else {
        Err(format!(
            "could not re-encode the JPEG (status {status}, libjpeg error {error_code})"
        ))
    }
}

/// Pure-Rust equivalent, for builds without the `jpeg-rotate` feature.
#[cfg(not(feature = "jpeg-rotate"))]
fn encode_rotated_rgb(rgb: &[u8], width: u32, height: u32, degrees: i32) -> Result<Vec<u8>, String> {
    use image::codecs::jpeg::JpegEncoder;

    let img = image::RgbImage::from_raw(width, height, rgb.to_vec())
        .ok_or_else(|| "RGB buffer does not match the dimensions".to_string())?;
    let img = image::DynamicImage::ImageRgb8(img);
    let rotated = match degrees {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    };

    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut std::io::Cursor::new(&mut out), JPEG_REENCODE_QUALITY)
        .encode_image(&rotated)
        .map_err(|e| format!("could not re-encode the JPEG: {e}"))?;
    Ok(out)
}

/// Rotates a JPEG clockwise by 90, 180 or 270 degrees **without decoding it**.
///
/// Works in the DCT domain, like `jpegtran`: the entropy-coded coefficients are
/// read, the 8×8 blocks permuted and transposed, then re-encoded. No IDCT/DCT, no
/// colour conversion, no requantisation — the pixels come back identical, turned.
/// It is not free though: Huffman decoding and re-encoding the whole scan still
/// costs roughly a fifth of a full decode + re-encode cycle.
///
/// Fails with [`NOT_MCU_ALIGNED`] when the dimensions are not a whole number of
/// MCUs, rather than trimming the partial edge blocks the way `jpegtran` would —
/// [`rotate_jpeg`] then re-encodes the whole image instead.
///
/// EXIF and the other metadata markers are carried over, with `Orientation`
/// reset to upright since the pixels now are.
///
/// The libjpeg pipeline itself lives in `jpeg_rotate.c`: its fatal-error path
/// has to be left with `longjmp`, which Rust cannot do.
#[cfg(feature = "jpeg-rotate")]
pub fn rotate_jpeg_lossless(jpeg: &[u8], degrees: i32) -> Result<Vec<u8>, String> {
    use std::os::raw::{c_int, c_uchar};

    const TC_ROTATE_OK: c_int = 0;
    const TC_ROTATE_NOT_EXACT: c_int = 1;
    const TC_ROTATE_FAILED: c_int = 2;

    extern "C" {
        fn tc_rotate_jpeg(
            input: *const c_uchar,
            input_len: usize,
            degrees: c_int,
            out: *mut *mut c_uchar,
            out_len: *mut usize,
            error_code: *mut c_int,
        ) -> c_int;
        fn tc_free_jpeg(data: *mut c_uchar);
    }

    let degrees = quantize(degrees);
    if degrees == 0 {
        return Ok(jpeg.to_vec());
    }

    let mut out: *mut c_uchar = std::ptr::null_mut();
    let mut out_len: usize = 0;
    let mut error_code: c_int = 0;

    let status = unsafe {
        tc_rotate_jpeg(
            jpeg.as_ptr(),
            jpeg.len(),
            degrees,
            &mut out,
            &mut out_len,
            &mut error_code,
        )
    };

    match status {
        TC_ROTATE_OK if !out.is_null() && out_len > 0 => {
            let mut rotated = unsafe { std::slice::from_raw_parts(out, out_len).to_vec() };
            unsafe { tc_free_jpeg(out) };
            // The pixels carry the orientation now; a leftover tag would make an
            // EXIF-aware viewer turn the image a second time.
            reset_exif_orientation(&mut rotated);
            Ok(rotated)
        }
        TC_ROTATE_NOT_EXACT => Err(NOT_MCU_ALIGNED.to_string()),
        TC_ROTATE_FAILED => Err(format!("libjpeg error {error_code}")),
        other => Err(format!("lossless rotation failed (status {other})")),
    }
}

#[cfg(not(feature = "jpeg-rotate"))]
pub fn rotate_jpeg_lossless(jpeg: &[u8], degrees: i32) -> Result<Vec<u8>, String> {
    if quantize(degrees) == 0 {
        return Ok(jpeg.to_vec());
    }
    Err("built without the jpeg-rotate feature".to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_orientation_never_rotates() {
        for sensor in [0, 90, 180, 270] {
            assert_eq!(jpeg_orientation(sensor, ORIENTATION_UNKNOWN, false), 0);
            assert_eq!(jpeg_orientation(sensor, ORIENTATION_UNKNOWN, true), 0);
        }
    }

    #[test]
    fn back_camera_matches_the_ndk_reference() {
        // A typical back sensor is mounted at 90°.
        assert_eq!(jpeg_orientation(90, 0, false), 90); // device upright
        assert_eq!(jpeg_orientation(90, 90, false), 180);
        assert_eq!(jpeg_orientation(90, 180, false), 270);
        assert_eq!(jpeg_orientation(90, 270, false), 0);
        // A sensor mounted upright needs no correction when the device is upright.
        assert_eq!(jpeg_orientation(0, 0, false), 0);
    }

    #[test]
    fn front_camera_corrects_the_other_way() {
        assert_eq!(jpeg_orientation(270, 0, true), 270);
        assert_eq!(jpeg_orientation(270, 90, true), 180);
        assert_eq!(jpeg_orientation(270, 180, true), 90);
        assert_eq!(jpeg_orientation(270, 270, true), 0);
    }

    #[test]
    fn device_orientation_is_rounded_to_a_quarter_turn() {
        assert_eq!(jpeg_orientation(0, 44, false), 0);
        assert_eq!(jpeg_orientation(0, 45, false), 90);
        assert_eq!(jpeg_orientation(0, 134, false), 90);
        assert_eq!(jpeg_orientation(0, 135, false), 180);
        // 350° rounds to a full turn, which is no rotation at all.
        assert_eq!(jpeg_orientation(0, 350, false), 0);
    }

    #[test]
    fn quantize_normalises_any_angle() {
        assert_eq!(quantize(0), 0);
        assert_eq!(quantize(359), 0);
        assert_eq!(quantize(-90), 270);
        assert_eq!(quantize(450), 90);
    }

    #[test]
    fn out_of_range_orientations_are_treated_as_unknown() {
        set_device_orientation(90);
        assert_eq!(device_orientation(), 90);
        set_device_orientation(360);
        assert_eq!(device_orientation(), ORIENTATION_UNKNOWN);
        set_device_orientation(-5);
        assert_eq!(device_orientation(), ORIENTATION_UNKNOWN);
        set_device_orientation(ORIENTATION_UNKNOWN);
    }

    /// A minimal baseline JPEG: SOI, APP1/EXIF with an orientation tag, SOF0, SOS.
    fn fake_jpeg(width: u16, height: u16, orientation: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];

        // --- APP1 / EXIF, big-endian TIFF with a single IFD0 entry -----------
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"MM\0\x2a");
        tiff.extend_from_slice(&8u32.to_be_bytes()); // IFD0 right after the header
        tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
        tiff.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
        tiff.extend_from_slice(&3u16.to_be_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_be_bytes()); // count
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]); // padding of the 4-byte value field
        tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD

        let payload_len = 6 + tiff.len();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((payload_len + 2) as u16).to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&tiff);

        // --- SOF0 -----------------------------------------------------------
        out.extend_from_slice(&[0xFF, 0xC0]);
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(8); // precision
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.push(1); // one component
        out.extend_from_slice(&[1, 0x11, 0]);

        // --- SOS ------------------------------------------------------------
        out.extend_from_slice(&[0xFF, 0xDA]);
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&[1, 1, 0, 0, 0x3F, 0]);

        out
    }

    #[test]
    fn reads_dimensions_from_the_sof_marker() {
        assert_eq!(jpeg_pixel_size(&fake_jpeg(4080, 3060, 1)), Some((4080, 3060)));
        assert_eq!(jpeg_pixel_size(&[]), None);
        assert_eq!(jpeg_pixel_size(&[0xFF, 0xD8, 0xFF, 0xD9]), None);
    }

    #[test]
    fn reads_and_resets_the_exif_orientation_tag() {
        let mut jpeg = fake_jpeg(640, 480, 6);
        let found = exif_orientation(&jpeg).expect("orientation tag");
        assert_eq!(found.value, 6);
        assert!(!found.little_endian);

        assert!(reset_exif_orientation(&mut jpeg));
        assert_eq!(exif_orientation(&jpeg).map(|f| f.value), Some(1));
        // The rest of the header survived the patch.
        assert_eq!(jpeg_pixel_size(&jpeg), Some((640, 480)));
    }

    #[test]
    fn exif_tags_map_to_quarter_turns() {
        assert_eq!(exif_tag_for(0), Some(1));
        assert_eq!(exif_tag_for(90), Some(6));
        assert_eq!(exif_tag_for(180), Some(3));
        assert_eq!(exif_tag_for(270), Some(8));
    }

    #[test]
    fn residual_is_zero_when_nothing_was_asked() {
        let jpeg = fake_jpeg(640, 480, 1);
        assert_eq!(residual_rotation(&jpeg, 0, 640, 480), 0);
    }

    #[test]
    fn residual_detects_an_exif_only_hal() {
        // Asked for 90°, got untouched pixels and an EXIF tag saying "90°".
        let jpeg = fake_jpeg(640, 480, 6);
        assert_eq!(residual_rotation(&jpeg, 90, 640, 480), 90);

        // Same for a half turn, which the dimensions could never reveal.
        let jpeg = fake_jpeg(640, 480, 3);
        assert_eq!(residual_rotation(&jpeg, 180, 640, 480), 180);
    }

    #[test]
    fn residual_detects_a_hal_that_rotated_the_pixels() {
        // Asked for 90°, got swapped dimensions and a clean tag: already upright.
        let jpeg = fake_jpeg(480, 640, 1);
        assert_eq!(residual_rotation(&jpeg, 90, 640, 480), 0);

        // A half turn keeps the dimensions, so the clean tag is what tells us.
        let jpeg = fake_jpeg(640, 480, 1);
        assert_eq!(residual_rotation(&jpeg, 180, 640, 480), 0);
    }

    #[test]
    fn residual_falls_back_to_dimensions_without_exif() {
        let mut jpeg = fake_jpeg(640, 480, 6);
        // Blank out the EXIF marker identifier so only the SOF remains readable.
        jpeg[3] = 0xE2; // APP1 -> APP2
        assert_eq!(residual_rotation(&jpeg, 90, 640, 480), 90);

        let mut jpeg = fake_jpeg(480, 640, 6);
        jpeg[3] = 0xE2;
        assert_eq!(residual_rotation(&jpeg, 90, 640, 480), 0);
    }

    /// An encodable test image of an arbitrary size.
    fn jpeg_of(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x * 3) as u8, (y * 5) as u8, 128]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .expect("encode");
        out
    }

    /// The whole point of the fallback: a rotation must never drop a pixel, at
    /// any size, whichever route it takes.
    #[test]
    fn rotation_never_crops_whatever_the_size() {
        // 64x32 is a whole number of MCUs at any subsampling; 70x38 and 100x54
        // are not at 4:2:0, which is what forces the re-encode.
        for (width, height) in [(64, 32), (70, 38), (100, 54), (33, 17)] {
            let jpeg = jpeg_of(width, height);
            assert_eq!(jpeg_pixel_size(&jpeg), Some((width, height)));

            for degrees in [90, 180, 270] {
                let (out, _) = rotate_jpeg(&jpeg, degrees).expect("rotate");
                let expected = if degrees == 180 {
                    (width, height)
                } else {
                    (height, width)
                };
                assert_eq!(
                    jpeg_pixel_size(&out),
                    Some(expected),
                    "{width}x{height} rotated {degrees} deg must keep every pixel"
                );
            }
        }
    }

    /// The re-encoding fallback must turn the image the same way as the lossless
    /// route and as the live-view path in the C bridge — clockwise, not mirrored
    /// and not transposed. The scanline gather is hand-written index arithmetic,
    /// so this checks it against a reference rotation.
    ///
    /// Uses large flat blocks of colour and a tolerance, since the comparison
    /// goes through a JPEG round trip.
    #[test]
    fn the_re_encoded_fallback_turns_the_image_clockwise() {
        // Four quadrants, each a different flat colour. 70x38 is not a whole
        // number of MCUs, so this is exactly the shape that takes the fallback.
        let (width, height) = (70u32, 38u32);
        let mut source = image::RgbImage::new(width, height);
        for (x, y, px) in source.enumerate_pixels_mut() {
            *px = match (x < width / 2, y < height / 2) {
                (true, true) => image::Rgb([220, 30, 30]),
                (false, true) => image::Rgb([30, 220, 30]),
                (true, false) => image::Rgb([30, 30, 220]),
                (false, false) => image::Rgb([230, 230, 40]),
            };
        }

        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(source.clone())
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .expect("encode");

        for degrees in [90, 180, 270] {
            let (out, _) = rotate_jpeg(&jpeg, degrees).expect("rotate");
            let ours = image::load_from_memory(&out).expect("decode").into_rgb8();

            let reference = match degrees {
                90 => image::imageops::rotate90(&source),
                180 => image::imageops::rotate180(&source),
                _ => image::imageops::rotate270(&source),
            };

            assert_eq!(
                ours.dimensions(),
                reference.dimensions(),
                "{degrees} deg: wrong output size"
            );

            // Sample well inside each quadrant, away from the block edges where
            // JPEG ringing is strongest.
            let (w, h) = ours.dimensions();
            for (x, y) in [
                (w / 4, h / 4),
                (w * 3 / 4, h / 4),
                (w / 4, h * 3 / 4),
                (w * 3 / 4, h * 3 / 4),
            ] {
                let got = ours.get_pixel(x, y).0;
                let want = reference.get_pixel(x, y).0;
                for channel in 0..3 {
                    let delta = got[channel].abs_diff(want[channel]);
                    assert!(
                        delta <= 24,
                        "{degrees} deg at ({x},{y}): got {got:?}, expected {want:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_degrees_is_a_no_op() {
        let jpeg = jpeg_of(64, 32);
        let (out, path) = rotate_jpeg(&jpeg, 0).expect("rotate");
        assert_eq!(path, RotationPath::Unchanged);
        assert_eq!(out, jpeg);
    }

    #[cfg(feature = "jpeg-rotate")]
    mod lossless {
        use super::*;

        /// A real, encodable image: 64×32 so every rotation is MCU-exact.
        fn sample_jpeg() -> Vec<u8> {
            jpeg_of(64, 32)
        }

        #[test]
        fn an_mcu_exact_image_takes_the_lossless_route() {
            let (_, path) = rotate_jpeg(&sample_jpeg(), 90).expect("rotate");
            assert_eq!(path, RotationPath::Lossless);
        }

        #[test]
        fn an_unaligned_image_falls_back_to_a_re_encode_rather_than_cropping() {
            // 4:2:0 gives a 16-pixel MCU, and 38 is not a multiple of 16.
            let jpeg = jpeg_of(70, 38);
            assert!(rotate_jpeg_lossless(&jpeg, 90)
                .expect_err("should refuse")
                .contains("MCU"));

            let (out, path) = rotate_jpeg(&jpeg, 90).expect("rotate");
            assert_eq!(path, RotationPath::Reencoded);
            assert_eq!(jpeg_pixel_size(&out), Some((38, 70)));
        }

        #[test]
        fn quarter_turns_swap_the_dimensions() {
            let jpeg = sample_jpeg();
            assert_eq!(jpeg_pixel_size(&jpeg), Some((64, 32)));

            for degrees in [90, 270] {
                let out = rotate_jpeg_lossless(&jpeg, degrees).expect("rotate");
                assert_eq!(
                    jpeg_pixel_size(&out),
                    Some((32, 64)),
                    "{degrees}° should swap width and height"
                );
            }
        }

        #[test]
        fn half_turn_keeps_the_dimensions() {
            let out = rotate_jpeg_lossless(&sample_jpeg(), 180).expect("rotate");
            assert_eq!(jpeg_pixel_size(&out), Some((64, 32)));
        }

        #[test]
        fn rotating_four_times_restores_the_original_pixels() {
            let jpeg = sample_jpeg();
            let mut out = jpeg.clone();
            for _ in 0..4 {
                out = rotate_jpeg_lossless(&out, 90).expect("rotate");
            }
            // Lossless means the coefficients come back identical, so decoding
            // both must give the very same pixels.
            let before = image::load_from_memory(&jpeg).expect("decode").to_rgb8();
            let after = image::load_from_memory(&out).expect("decode").to_rgb8();
            assert_eq!(before.dimensions(), after.dimensions());
            assert_eq!(before.into_raw(), after.into_raw());
        }

        #[test]
        fn garbage_input_is_an_error_not_a_crash() {
            assert!(rotate_jpeg_lossless(b"not a jpeg at all", 90).is_err());
        }

        /// A libjpeg failure must leave the process intact.
        ///
        /// It did not when the error path was a Rust panic unwinding through
        /// libjpeg's frames: the half-built memory pools were then destroyed on
        /// the way out, and the next allocation — in a completely unrelated JPEG
        /// decode — died on a corrupt heap. Driving libjpeg from C behind a
        /// `setjmp` is what fixed it, so this walks that exact sequence.
        #[test]
        fn work_continues_normally_after_a_libjpeg_failure() {
            for truncated in [1usize, 64, 512] {
                let jpeg = sample_jpeg();
                let broken = &jpeg[..truncated.min(jpeg.len())];
                let _ = rotate_jpeg(broken, 90); // may fail, must not poison anything

                let (out, _) = rotate_jpeg(&sample_jpeg(), 90).expect("still works");
                assert_eq!(jpeg_pixel_size(&out), Some((32, 64)));
            }
        }
    }
}

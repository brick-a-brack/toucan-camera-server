//! Frame averaging for `POST /cameras/{id}/capture?frames=N`.
//!
//! Pure image code (only the `image` crate), shared verbatim with the
//! WebAssembly build in `wasm/`, which `#[path]`-includes this file.

/// Upper bound on the number of frames averaged in a single capture request.
/// Each extra frame is one more shutter actuation plus a full JPEG decode, so
/// the request time grows linearly — 20 is already several seconds on a DSLR.
pub const MAX_AVERAGING_FRAMES: u32 = 20;

/// JPEG quality used when re-encoding an averaged capture. Single-frame
/// captures are passed through untouched, so this only applies to merges.
const AVERAGED_JPEG_QUALITY: u8 = 95;

/// Resolve the `frames` query parameter: absent, malformed or zero means 1
/// (averaging disabled), and anything above `MAX_AVERAGING_FRAMES` is clamped.
pub fn parse_frame_count(raw: Option<&str>) -> u32 {
    match raw {
        None => 1,
        Some(s) => match s.trim().parse::<u32>() {
            Ok(0) | Err(_) => 1,
            Ok(n) => n.min(MAX_AVERAGING_FRAMES),
        },
    }
}

/// Average several JPEGs pixel by pixel and re-encode the result.
///
/// Frame averaging exploits the fact that sensor noise is random while the
/// scene is not: the mean of N shots keeps the signal and divides the noise
/// standard deviation by sqrt(N). All frames must share the same dimensions
/// (a camera that changed resolution mid-burst is a hard error, not something
/// to silently crop). The per-channel mean is truncated, matching the original
/// JS implementation.
pub fn average_frames(frames: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let first = frames
        .first()
        .ok_or_else(|| "no frame to merge".to_string())?;
    let mut acc = image::load_from_memory_with_format(first, image::ImageFormat::Jpeg)
        .map_err(|e| format!("cannot decode captured frame: {e}"))?
        .to_rgb8();
    let (width, height) = (acc.width(), acc.height());

    if frames.len() == 1 {
        return encode_rgb_jpeg(&acc);
    }

    // u32 accumulators: 20 frames x 255 fits with room to spare.
    let mut sums: Vec<u32> = acc.as_raw().iter().map(|&b| b as u32).collect();

    for frame in &frames[1..] {
        let img = image::load_from_memory_with_format(frame, image::ImageFormat::Jpeg)
            .map_err(|e| format!("cannot decode captured frame: {e}"))?
            .to_rgb8();
        if img.width() != width || img.height() != height {
            return Err(format!(
                "frame size inconsistency: expected {width}x{height}, got {}x{}",
                img.width(),
                img.height()
            ));
        }
        for (sum, &byte) in sums.iter_mut().zip(img.as_raw().iter()) {
            *sum += byte as u32;
        }
    }

    let n = frames.len() as u32;
    for (out, sum) in acc.as_mut().iter_mut().zip(sums.iter()) {
        *out = (*sum / n) as u8;
    }

    encode_rgb_jpeg(&acc)
}

fn encode_rgb_jpeg(img: &image::RgbImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, AVERAGED_JPEG_QUALITY)
        .encode_image(&image::DynamicImage::ImageRgb8(img.clone()))
        .map_err(|e| format!("cannot encode merged frame: {e}"))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb(rgb));
        encode_rgb_jpeg(&img).expect("encode")
    }

    #[test]
    fn frame_count_defaults_to_one() {
        assert_eq!(parse_frame_count(None), 1);
        assert_eq!(parse_frame_count(Some("")), 1);
        assert_eq!(parse_frame_count(Some("abc")), 1);
        assert_eq!(parse_frame_count(Some("-3")), 1);
        assert_eq!(parse_frame_count(Some("2.5")), 1);
        assert_eq!(parse_frame_count(Some("0")), 1);
    }

    #[test]
    fn frame_count_is_parsed_and_capped() {
        assert_eq!(parse_frame_count(Some("1")), 1);
        assert_eq!(parse_frame_count(Some(" 7 ")), 7);
        assert_eq!(parse_frame_count(Some("20")), 20);
        assert_eq!(parse_frame_count(Some("21")), 20);
        assert_eq!(parse_frame_count(Some("99999")), 20);
    }

    #[test]
    fn averaging_two_frames_returns_the_mean() {
        let frames = vec![
            solid_jpeg(16, 16, [100, 0, 50]),
            solid_jpeg(16, 16, [200, 0, 150]),
        ];
        let merged = average_frames(&frames).expect("merge");
        let img = image::load_from_memory_with_format(&merged, image::ImageFormat::Jpeg)
            .expect("decode")
            .to_rgb8();
        assert_eq!((img.width(), img.height()), (16, 16));
        let px = img.get_pixel(8, 8).0;
        // JPEG is lossy: allow a small tolerance around (150, 0, 100).
        assert!((px[0] as i32 - 150).abs() <= 3, "red was {}", px[0]);
        assert!((px[1] as i32).abs() <= 3, "green was {}", px[1]);
        assert!((px[2] as i32 - 100).abs() <= 3, "blue was {}", px[2]);
    }

    #[test]
    fn averaging_rejects_mismatched_sizes() {
        let frames = vec![
            solid_jpeg(16, 16, [10, 10, 10]),
            solid_jpeg(8, 8, [10, 10, 10]),
        ];
        let err = average_frames(&frames).expect_err("should fail");
        assert!(err.contains("frame size inconsistency"), "{err}");
    }

    #[test]
    fn averaging_no_frame_is_an_error() {
        assert!(average_frames(&[]).is_err());
    }
}

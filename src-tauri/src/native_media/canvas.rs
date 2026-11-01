use super::{EncodedInlayImage, InlayImageMetadata};
use std::borrow::Cow;

pub(super) fn encode_webp(
    data: &[u8], width: u32, height: u32, quality: f32,
) -> Result<EncodedInlayImage<'static>, String> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > 16383 || height > 16383
        || pixels > 16 * 1024 * 1024 || pixels * 4 != data.len() as u64
        || !quality.is_finite() || !(0.0..=1.0).contains(&quality)
    {
        return Err("invalid plugin WebP canvas input".to_owned());
    }
    let encoded = webp::Encoder::from_rgba(data, width, height)
        .encode_simple(false, quality * 100.0)
        .map_err(|error| format!("failed to encode plugin WebP canvas: {error:?}"))?
        .to_vec();
    Ok(EncodedInlayImage {
        metadata: InlayImageMetadata {
            key: "plugin-canvas".to_owned(),
            kind: "inlay".to_owned(),
            size: encoded.len() as u64,
            mime: "image/webp".to_owned(),
            name: "canvas.webp".to_owned(),
            ext: "webp".to_owned(),
            inlay_type: "image".to_owned(),
            width: Some(width), height: Some(height), preservation_reason: None,
        },
        data: Cow::Owned(encoded),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_canvas_pixels_with_transparency_at_quality_endpoints() {
        for quality in [0.0, 0.85, 1.0] {
            let result = super::encode_webp(&[240, 120, 60, 0, 20, 40, 80, 128], 2, 1, quality).unwrap();
            assert_eq!(result.metadata.mime, "image/webp");
            let image = image::load_from_memory(&result.data).unwrap().into_rgba8();
            assert_eq!(image.dimensions(), (2, 1));
            assert_eq!(image.get_pixel(0, 0)[3], 0);
            assert_eq!(image.get_pixel(1, 0)[3], 128);
        }
    }

    #[test]
    fn rejects_invalid_canvas_dimensions_bytes_and_quality() {
        for (width, height, quality) in [
            (0, 1, 0.85), (1, 0, 0.85), (16384, 1, 0.85),
            (u32::MAX, u32::MAX, 0.85), (4097, 4097, 0.85), (2, 1, 0.85),
            (1, 1, -0.1), (1, 1, 1.1), (1, 1, f32::NAN), (1, 1, f32::INFINITY),
        ] {
            assert!(super::encode_webp(&[0; 4], width, height, quality).is_err());
        }
    }
}

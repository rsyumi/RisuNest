use crate::native_log::logged;
use crate::asset_repository::PayloadCas;
use crate::trust_boundary::is_lower_hex_byte;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::metadata::{LoopCount, Orientation};
use image::{
    AnimationDecoder, ColorType, Delay, DynamicImage, ImageDecoder, ImageFormat, ImageReader,
    RgbaImage,
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{
    borrow::Cow,
    fs::{self, File},
    io::{Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use tauri::http::{
    header::{self, HeaderValue},
    Method, Request, Response, StatusCode,
};
use tauri::{AppHandle, Manager};

pub(crate) mod ipc;
mod canvas;
mod animation_policy;
mod thumbnail;

const MAX_BODY_BYTES: u64 = 1024 * 1024;
const EXPOSED_HEADERS: &str = "Accept-Ranges, Content-Length, Content-Range, Content-Type, ETag";
const MAX_INLAY_DIMENSION: u32 = u32::MAX;
const MAX_INLAY_ANIMATION_FPS: u32 = 240;
const MAX_ANIMATION_FRAMES: usize = 600;
const MAX_ANIMATION_RGBA_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ANIMATION_CANVAS_SIDE: u32 = 4096;
/// Browsers play a GIF frame at or under this delay as `SLOW_FRAME_DELAY_MS`.
const GIF_FAST_FRAME_DELAY_MS: u32 = 10;
const SLOW_FRAME_DELAY_MS: u32 = 100;
/// A leading frame without a delay of its own still has to advance the timeline.
const FIRST_FRAME_MIN_DELAY_MS: u32 = 10;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayImageMetadata {
    key: String,
    kind: String,
    size: u64,
    mime: String,
    name: String,
    ext: String,
    inlay_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preservation_reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EncodedInlayImage<'a> {
    /// Borrows the input when the source is kept as it was.
    data: Cow<'a, [u8]>,
    metadata: InlayImageMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayEncodeOptions {
    format: InlayEncodeFormat,
    #[serde(deserialize_with = "deserialize_inlay_quality")]
    quality: u8,
    #[serde(deserialize_with = "deserialize_inlay_max_dimension")]
    max_dimension: u32,
    skip_reencode: bool,
    /// Frames per second an animation is thinned down to, or 0 to keep the original rate.
    #[serde(deserialize_with = "deserialize_inlay_animation_fps")]
    animation_max_fps: u32,
    #[serde(default = "default_animation_decode_bytes")]
    animation_decode_bytes: u64,
}

fn default_animation_decode_bytes() -> u64 { 256 * 1024 * 1024 }

fn deserialize_inlay_animation_fps<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u32::deserialize(deserializer)?.min(MAX_INLAY_ANIMATION_FPS))
}

fn deserialize_inlay_quality<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u8::deserialize(deserializer)?.clamp(1, 100))
}

fn deserialize_inlay_max_dimension<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    struct MaxDimensionVisitor;

    impl Visitor<'_> for MaxDimensionVisitor {
        type Value = u32;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a numeric Inlay maximum dimension")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.clamp(0, i64::from(MAX_INLAY_DIMENSION)) as u32)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.min(u64::from(MAX_INLAY_DIMENSION)) as u32)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !value.is_finite() {
                return Ok(0);
            }
            Ok(value.round().clamp(0.0, f64::from(MAX_INLAY_DIMENSION)) as u32)
        }
    }

    deserializer.deserialize_any(MaxDimensionVisitor)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InlayEncodeFormat {
    Webp,
    Png,
    Original,
}

impl Default for InlayEncodeOptions {
    fn default() -> Self {
        Self {
            format: InlayEncodeFormat::Webp,
            quality: 85,
            max_dimension: 0,
            skip_reencode: false,
            animation_max_fps: 0,
            animation_decode_bytes: default_animation_decode_bytes(),
        }
    }
}

struct ResolvedBlob {
    payload_path: PathBuf,
    mime: String,
    size: u64,
    validator: String,
    cache_control: &'static str,
}

enum RequestedRange {
    Full,
    Partial { start: u64, end: u64 },
}

pub(crate) fn decode_physical_key(uri: &str) -> Option<String> {
    let parsed = url::Url::parse(uri).ok()?;
    let valid_origin = match parsed.scheme() {
        "risuasset" => parsed.host_str() == Some("localhost"),
        "http" | "https" => parsed.host_str() == Some("risuasset.localhost"),
        _ => false,
    };
    if !valid_origin {
        return None;
    }
    let encoded = parsed.path().strip_prefix('/')?;
    if encoded.is_empty() || encoded.contains('/') || encoded.len() % 2 != 0 {
        return None;
    }
    let physical_key = String::from_utf8(hex::decode(encoded).ok()?).ok()?;
    if valid_physical_key(&physical_key) {
        Some(physical_key)
    } else {
        None
    }
}

fn valid_physical_key(key: &str) -> bool {
    cas_content_hash(key).is_some()
}

fn cas_content_hash(key: &str) -> Option<String> {
    let (shard, suffix) = key.strip_prefix("assets/objects/")?.split_once('/')?;
    if shard.len() != 2
        || suffix.len() != 62
        || !shard.bytes().chain(suffix.bytes()).all(is_lower_hex_byte)
    {
        return None;
    }
    Some(format!("{shard}{suffix}"))
}

fn cas_descriptor(uri: &str) -> Option<(String, u64)> {
    let parsed = url::Url::parse(uri).ok()?;
    let mut mime = None;
    let mut size = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "mime" if mime.is_none() => mime = Some(value.into_owned()),
            "size" if size.is_none() => size = Some(value.parse::<u64>().ok()?),
            _ => return None,
        }
    }
    let mime = mime?;
    if mime.is_empty() || HeaderValue::from_str(&mime).is_err() {
        return None;
    }
    Some((mime, size?))
}

fn resolve_blob(root: &Path, uri: &str, physical_key: String) -> Option<ResolvedBlob> {
    let content_hash = cas_content_hash(&physical_key)?;
    let (mime, expected_size) = cas_descriptor(uri)?;
    let payload_path = PayloadCas::new(root)
        .ok()?
        .object_path(&content_hash)
        .ok()??;
    let file_metadata = fs::metadata(&payload_path).ok()?;
    if !file_metadata.is_file() || file_metadata.len() != expected_size {
        return None;
    }
    Some(ResolvedBlob {
        payload_path,
        mime,
        size: expected_size,
        validator: format!("\"{content_hash}\""),
        cache_control: "public, max-age=31536000, immutable",
    })
}

#[derive(Clone, Copy, PartialEq)]
enum InlayAnimation {
    Gif,
    Apng,
    WebP,
}

fn inlay_animation(format: ImageFormat, data: &[u8]) -> Option<InlayAnimation> {
    match format {
        ImageFormat::Gif => Some(InlayAnimation::Gif),
        ImageFormat::WebP => webp::BitstreamFeatures::new(data)
            .is_some_and(|features| features.has_animation())
            .then_some(InlayAnimation::WebP),
        ImageFormat::Png => PngDecoder::new(Cursor::new(data))
            .ok()
            .and_then(|decoder| decoder.is_apng().ok())
            .unwrap_or(false)
            .then_some(InlayAnimation::Apng),
        _ => None,
    }
}

fn inlay_media_type(format: Option<ImageFormat>, name: &str) -> (String, String) {
    let known = format.and_then(|format| match format {
        ImageFormat::Png => Some(("image/png", "png")),
        ImageFormat::Jpeg => Some(("image/jpeg", "jpg")),
        ImageFormat::WebP => Some(("image/webp", "webp")),
        ImageFormat::Gif => Some(("image/gif", "gif")),
        ImageFormat::Avif => Some(("image/avif", "avif")),
        ImageFormat::Bmp => Some(("image/bmp", "bmp")),
        ImageFormat::Tiff => Some(("image/tiff", "tiff")),
        _ => None,
    });
    if let Some((mime, ext)) = known {
        return (mime.to_owned(), ext.to_owned());
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    };
    (mime.to_owned(), ext)
}

/// Stores the input untouched. Every image this encoder cannot improve ends up
/// here, so an attachment never fails because of its format.
fn preserved_inlay_image<'a>(
    id: &str,
    data: &'a [u8],
    name: &str,
    size: Option<(u32, u32)>,
) -> EncodedInlayImage<'a> {
    let (mime, ext) = inlay_media_type(image::guess_format(data).ok(), name);
    EncodedInlayImage {
        data: Cow::Borrowed(data),
        metadata: InlayImageMetadata {
            key: id.to_owned(),
            kind: "inlay".to_owned(),
            size: data.len() as u64,
            mime,
            name: name.to_owned(),
            ext,
            inlay_type: "image".to_owned(),
            width: size.map(|(width, _)| width),
            height: size.map(|(_, height)| height),
            preservation_reason: None,
        },
    }
}

fn frame_delay_ms(delay: Delay, source: InlayAnimation) -> u32 {
    let (numerator, denominator) = delay.numer_denom_ms();
    let milliseconds = if denominator == 0 {
        0
    } else {
        numerator / denominator
    };
    match source {
        // Browsers play a GIF frame this short as a tenth of a second, so keeping
        // the stored delay would speed the animation up against what was on screen.
        InlayAnimation::Gif if milliseconds <= GIF_FAST_FRAME_DELAY_MS => SLOW_FRAME_DELAY_MS,
        InlayAnimation::Apng if milliseconds == 0 => SLOW_FRAME_DELAY_MS,
        // Animated WebP counts in milliseconds, where a short delay is meant literally.
        _ => milliseconds,
    }
}

struct DecodedInlayAnimation {
    /// Frames the timeline carries, already at the output canvas size.
    frames: Vec<(RgbaImage, u32)>,
    width: u32,
    height: u32,
    loop_count: i32,
}

/// Decodes one frame at a time and keeps only the frames the timeline can
/// carry, already scaled to the output canvas, so the source-size frames of a
/// scaled or thinned animation never pile up in memory.
fn decode_inlay_animation(
    data: &[u8],
    source: InlayAnimation,
    options: &InlayEncodeOptions,
) -> Result<DecodedInlayAnimation, String> {
    let (loop_count, frames) = match source {
        InlayAnimation::Gif => {
            let decoder = GifDecoder::new(Cursor::new(data))
                .map_err(|error| format!("failed to read GIF Inlay image: {error}"))?;
            (decoder.loop_count(), decoder.into_frames())
        }
        InlayAnimation::Apng => {
            let decoder = PngDecoder::new(Cursor::new(data))
                .map_err(|error| format!("failed to read APNG Inlay image: {error}"))?
                .apng()
                .map_err(|error| format!("failed to read APNG Inlay image: {error}"))?;
            (decoder.loop_count(), decoder.into_frames())
        }
        InlayAnimation::WebP => {
            let decoder = WebPDecoder::new(Cursor::new(data))
                .map_err(|error| format!("failed to read animated WebP Inlay image: {error}"))?;
            (decoder.loop_count(), decoder.into_frames())
        }
    };
    let min_delay_ms = if options.animation_max_fps > 0 {
        (1000f64 / f64::from(options.animation_max_fps)).ceil() as u32
    } else {
        0
    };
    let mut kept: Vec<(RgbaImage, u32)> = Vec::new();
    let mut canvas: Option<(u32, u32)> = None;
    let mut decoded_frames = 0usize;
    let mut rgba_bytes: u64 = 0;
    for frame in frames {
        let frame =
            frame.map_err(|error| format!("failed to decode Inlay animation frame: {error}"))?;
        if decoded_frames >= MAX_ANIMATION_FRAMES {
            return Err("Inlay animation has too many frames".to_owned());
        }
        decoded_frames += 1;
        let delay = frame_delay_ms(frame.delay(), source);
        let buffer = frame.into_buffer();
        rgba_bytes += buffer.as_raw().len() as u64;
        if rgba_bytes > MAX_ANIMATION_RGBA_BYTES {
            return Err("Inlay animation needs too much memory".to_owned());
        }
        let (width, height) = match canvas {
            Some(canvas) => canvas,
            None => *canvas.insert(animation_canvas(buffer.width(), buffer.height(), options)?),
        };
        // A frame the timeline cannot carry hands its time to the frame before
        // it, so the animation still runs for exactly as long as it did.
        match kept.last_mut() {
            Some(previous) if delay == 0 || previous.1 < min_delay_ms => previous.1 += delay,
            _ => {
                let delay = if kept.is_empty() && delay == 0 {
                    FIRST_FRAME_MIN_DELAY_MS
                } else {
                    delay
                };
                let buffer = if buffer.width() == width && buffer.height() == height {
                    buffer
                } else {
                    image::imageops::resize(&buffer, width, height, FilterType::Lanczos3)
                };
                kept.push((buffer, delay));
            }
        }
    }
    let Some((width, height)) = canvas else {
        return Err("Inlay animation has no frames".to_owned());
    };
    Ok(DecodedInlayAnimation {
        frames: kept,
        width,
        height,
        loop_count: match loop_count {
            LoopCount::Infinite => 0,
            LoopCount::Finite(count) => count.get().min(i32::MAX as u32) as i32,
        },
    })
}

fn animation_canvas(
    width: u32,
    height: u32,
    options: &InlayEncodeOptions,
) -> Result<(u32, u32), String> {
    let (width, height) = if options.max_dimension > 0 && width.max(height) > options.max_dimension
    {
        let scale = options.max_dimension as f64 / width.max(height) as f64;
        (
            (width as f64 * scale).round().max(1.0) as u32,
            (height as f64 * scale).round().max(1.0) as u32,
        )
    } else {
        (width, height)
    };
    if width.max(height) > MAX_ANIMATION_CANVAS_SIDE {
        return Err("Inlay animation canvas is too large".to_owned());
    }
    Ok((width, height))
}

/// libwebp reads a frame's duration from the timestamp of the frame after it,
/// and the encoder this crate exposes cannot pass an end timestamp, so it guesses
/// the last one. Writing the time that is left into the last frame keeps the
/// animation as long as it was, even where libwebp folded repeated frames together.
fn set_last_animation_frame_duration(
    data: &mut [u8],
    total_duration_ms: u32,
) -> Result<(), String> {
    let mut frames: Vec<(usize, u32)> = Vec::new();
    let mut offset = 12usize;
    while offset + 8 <= data.len() {
        let size = u32::from_le_bytes(
            data[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| "unreadable WebP chunk size".to_owned())?,
        ) as usize;
        let payload = offset + 8;
        if payload + size > data.len() {
            return Err("truncated WebP chunk in the Inlay animation".to_owned());
        }
        if &data[offset..offset + 4] == b"ANMF" {
            if size < 16 {
                return Err("truncated animation frame in the Inlay animation".to_owned());
            }
            let duration = u32::from_le_bytes([
                data[payload + 12],
                data[payload + 13],
                data[payload + 14],
                0,
            ]);
            frames.push((payload, duration));
        }
        offset = payload + size + (size & 1);
    }
    let Some((payload, _)) = frames.last().copied() else {
        return Err("the encoded Inlay animation has no frames".to_owned());
    };
    let earlier: u32 = frames[..frames.len() - 1]
        .iter()
        .map(|(_, duration)| *duration)
        .sum();
    let last = total_duration_ms
        .saturating_sub(earlier)
        .clamp(1, 0x00ff_ffff);
    data[payload + 12..payload + 15].copy_from_slice(&last.to_le_bytes()[..3]);
    Ok(())
}

fn encode_animated_webp(
    frames: &[(RgbaImage, u32)],
    width: u32,
    height: u32,
    quality: u8,
    loop_count: i32,
) -> Result<Vec<u8>, String> {
    let mut config = webp::WebPConfig::new()
        .map_err(|()| "failed to prepare the WebP animation encoder".to_owned())?;
    config.quality = f32::from(quality);
    let mut encoder = webp::AnimEncoder::new(width, height, &config);
    encoder.set_loop_count(loop_count);
    let mut timestamp: i32 = 0;
    for (buffer, delay) in frames {
        encoder.add_frame(webp::AnimFrame::from_rgba(
            buffer.as_raw(),
            width,
            height,
            timestamp,
        ));
        timestamp = timestamp.saturating_add(*delay as i32);
    }
    let mut encoded = encoder
        .try_encode()
        .map(|memory| memory.to_vec())
        .map_err(|error| format!("failed to encode the Inlay animation: {error:?}"))?;
    let total: u32 = frames
        .iter()
        .map(|(_, delay)| *delay)
        .fold(0u32, u32::saturating_add);
    set_last_animation_frame_duration(&mut encoded, total)?;
    Ok(encoded)
}

fn encode_animated_inlay_image(
    id: &str,
    data: &[u8],
    name: &str,
    options: &InlayEncodeOptions,
    source: InlayAnimation,
) -> Result<EncodedInlayImage<'static>, String> {
    let DecodedInlayAnimation {
        frames,
        width,
        height,
        loop_count,
    } = decode_inlay_animation(data, source, options)?;
    let encoded = if frames.len() == 1 {
        let (buffer, _) = &frames[0];
        webp::Encoder::from_rgba(buffer.as_raw(), width, height)
            .encode(f32::from(options.quality))
            .to_vec()
    } else {
        encode_animated_webp(&frames, width, height, options.quality, loop_count)?
    };
    // An animation that grows is not worth the quality it loses on the way.
    if encoded.len() >= data.len() {
        return Err("re-encoding the Inlay animation saved nothing".to_owned());
    }
    Ok(EncodedInlayImage {
        metadata: InlayImageMetadata {
            key: id.to_owned(),
            kind: "inlay".to_owned(),
            size: encoded.len() as u64,
            mime: "image/webp".to_owned(),
            name: name.to_owned(),
            ext: "webp".to_owned(),
            inlay_type: "image".to_owned(),
            width: Some(width),
            height: Some(height),
            preservation_reason: None,
        },
        data: Cow::Owned(encoded),
    })
}

fn encode_inlay_image<'a>(
    id: &str,
    data: &'a [u8],
    name: &str,
    options: Option<InlayEncodeOptions>,
) -> Result<EncodedInlayImage<'a>, String> {
    if id.is_empty() || id.starts_with("assets/") {
        return Err("invalid Inlay image id".to_owned());
    }
    let options = options.unwrap_or_default();
    let Ok(format) = image::guess_format(data) else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    if let Some(source) = inlay_animation(format, data) {
        if let Ok(reader) = ImageReader::new(Cursor::new(data)).with_guessed_format() {
            if let Ok(mut decoder) = reader.into_decoder() {
                if !matches!(decoder.icc_profile(), Ok(None)) {
                    let mut preserved = preserved_inlay_image(id, data, name, Some(decoder.dimensions()));
                    preserved.metadata.preservation_reason = Some("color-profile".to_owned());
                    return Ok(preserved);
                }
            }
        }

        if options.format != InlayEncodeFormat::Original {
            if !animation_policy::permits_decode(data, source, options.animation_decode_bytes) {
                let mut preserved = preserved_inlay_image(id, data, name, None);
                preserved.metadata.preservation_reason = Some("animation-cost".to_owned());
                return Ok(preserved);
            }
            if let Ok(encoded) = encode_animated_inlay_image(id, data, name, &options, source) {
                return Ok(encoded);
            }
        }
        return Ok(preserved_inlay_image(id, data, name, None));
    }
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Ok(preserved_inlay_image(id, data, name, None));
    }

    let Ok(reader) = ImageReader::new(Cursor::new(data)).with_guessed_format() else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (mut width, mut height) = decoder.dimensions();
    if matches!(orientation, Orientation::Rotate90 | Orientation::Rotate270
        | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH)
    {
        std::mem::swap(&mut width, &mut height);
    }
    let needs_resize = options.max_dimension > 0 && width.max(height) > options.max_dimension;
    if options.format == InlayEncodeFormat::Original
        || (options.format == InlayEncodeFormat::Webp && options.skip_reencode
            && format == ImageFormat::WebP && !needs_resize)
    {
        return Ok(preserved_inlay_image(id, data, name, Some((width, height))));
    }
    match decoder.icc_profile() {
        Ok(None) => {},
        _ => {
            let mut preserved = preserved_inlay_image(id, data, name, Some((width, height)));
            preserved.metadata.preservation_reason = Some("color-profile".to_owned());
            return Ok(preserved);
        }
    }
    let Ok(mut decoded) = DynamicImage::from_decoder(decoder) else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    decoded.apply_orientation(orientation);
    if needs_resize {
        let scale = options.max_dimension as f64 / width.max(height) as f64;
        decoded = decoded.resize(
            (width as f64 * scale).round().max(1.0) as u32,
            (height as f64 * scale).round().max(1.0) as u32,
            FilterType::Lanczos3,
        );
    }
    let (output_width, output_height) = (decoded.width(), decoded.height());
    // Encoding the decoded pixels directly avoids holding a converted copy beside them.
    let (encoded, mime, ext) = match options.format {
        InlayEncodeFormat::Original => unreachable!(),
        InlayEncodeFormat::Png => {
            let image = match decoded.color() {
                ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8 => decoded,
                _ => DynamicImage::ImageRgba8(decoded.into_rgba8()),
            };
            let mut value = Vec::new();
            image
                .write_to(&mut Cursor::new(&mut value), ImageFormat::Png)
                .map_err(|error| format!("failed to encode PNG Inlay image: {error}"))?;
            (value, "image/png", "png")
        }
        InlayEncodeFormat::Webp => {
            let value = if decoded.color().has_alpha() {
                let rgba = decoded.into_rgba8();
                webp::Encoder::from_rgba(rgba.as_raw(), output_width, output_height)
                    .encode(options.quality as f32)
                    .to_vec()
            } else {
                let rgb = decoded.into_rgb8();
                webp::Encoder::from_rgb(rgb.as_raw(), output_width, output_height)
                    .encode(options.quality as f32)
                    .to_vec()
            };
            (value, "image/webp", "webp")
        }
    };
    let metadata = InlayImageMetadata {
        key: id.to_owned(),
        kind: "inlay".to_owned(),
        size: encoded.len() as u64,
        mime: mime.to_owned(),
        name: name.to_owned(),
        ext: ext.to_owned(),
        inlay_type: "image".to_owned(),
        width: Some(output_width),
        height: Some(output_height),
        preservation_reason: None,
    };
    Ok(EncodedInlayImage {
        data: Cow::Owned(encoded),
        metadata,
    })
}

#[tauri::command(async)]
pub(crate) async fn native_media_encode_inlay_image(
    app: AppHandle,
    id: String,
    data: Vec<u8>,
    name: String,
    options: Option<InlayEncodeOptions>,
) -> Result<ipc::EncodedInlayIpcResult, String> {
    logged("native_media_encode_inlay_image", async move {
        if data.len() > ipc::NATIVE_MEDIA_IPC_CHUNK_BYTES {
            return Err("native Inlay direct encoder input exceeds one IPC chunk".to_owned());
        }
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app
                .state::<crate::persistent_store::PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            ipc::encode_direct(
                &app.state::<ipc::NativeMediaIpcState>(),
                &id,
                &data,
                &name,
                options,
            )
        })
        .await
        .map_err(|error| format!("failed to join native Inlay image encoder: {error}"))?
    }.await)
}

fn parse_range(value: Option<&HeaderValue>, size: u64) -> Option<RequestedRange> {
    let Some(value) = value else {
        return Some(RequestedRange::Full);
    };
    let value = value.to_str().ok()?.strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    let (start, requested_end) = if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 || size == 0 {
            return None;
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start = start.parse::<u64>().ok()?;
        if start >= size {
            return None;
        }
        let end = if end.is_empty() {
            size - 1
        } else {
            end.parse::<u64>().ok()?.min(size - 1)
        };
        if end < start {
            return None;
        }
        (start, end)
    };
    let end = requested_end.min(start.saturating_add(MAX_BODY_BYTES - 1));
    Some(RequestedRange::Partial { start, end })
}

fn base_response(status: StatusCode) -> tauri::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, EXPOSED_HEADERS)
}

fn not_found() -> Response<Option<std::io::Take<File>>> {
    base_response(StatusCode::NOT_FOUND).body(None).unwrap()
}

fn prepare_response(root: &Path, request: Request<()>) -> Response<Option<std::io::Take<File>>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return base_response(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(None)
            .unwrap();
    }
    let uri = request.uri().to_string();
    let Some(physical_key) = decode_physical_key(&uri) else {
        return not_found();
    };
    // Open while cleanup is excluded; the opened handle then owns the read
    // lifetime without holding a repository lock during a network transfer.
    let Ok(_guard) = crate::asset_repository::coordinator::lock_repository_mutation() else {
        return base_response(StatusCode::SERVICE_UNAVAILABLE)
            .body(None)
            .unwrap();
    };
    let Some(blob) = resolve_blob(root, &uri, physical_key) else {
        return not_found();
    };
    let Ok(file) = crate::trust_boundary::open_regular_source(&blob.payload_path) else {
        return not_found();
    };
    drop(_guard);
    prepare_file_response(request, blob, file)
}

fn prepare_file_response(request: Request<()>, blob: ResolvedBlob, mut file: File) -> Response<Option<std::io::Take<File>>> {
    if file.metadata().ok().map(|metadata| metadata.len()) != Some(blob.size) {
        return not_found();
    }
    let validator = blob.validator.clone();
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(&validator)
    {
        return base_response(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, validator)
            .header(header::CACHE_CONTROL, blob.cache_control)
            .body(None)
            .unwrap();
    }

    let range_header = request.headers().get(header::RANGE).filter(|_| {
        request.headers().get(header::IF_RANGE).is_none_or(|value| {
            !validator.starts_with("W/") && value.to_str().ok() == Some(&validator)
        })
    });
    let Some(range) = parse_range(range_header, blob.size) else {
        return base_response(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", blob.size))
            .header(header::ETAG, validator)
            .header(header::CACHE_CONTROL, blob.cache_control)
            .body(None)
            .unwrap();
    };
    let (status, start, end) = match range {
        RequestedRange::Full => (StatusCode::OK, 0, blob.size.saturating_sub(1)),
        RequestedRange::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end),
    };
    let length = if blob.size == 0 { 0 } else { end - start + 1 };
    let mut builder = base_response(status)
        .header(header::CONTENT_TYPE, blob.mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ETAG, validator)
        .header(header::CACHE_CONTROL, blob.cache_control);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", blob.size),
        );
    }
    if request.method() == Method::HEAD || length == 0 {
        return builder.body(None).unwrap();
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return not_found();
    }
    builder.body(Some(file.take(length))).unwrap()
}

// Byte collection exists only in tests, never in the WebView serving path.
#[cfg(test)]
fn respond(root: &Path, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    prepare_response(root, request.map(|_| ())).map(|body| {
        let mut bytes = Vec::new();
        if let Some(mut reader) = body {
            reader.read_to_end(&mut bytes).unwrap();
        }
        bytes
    })
}

pub(crate) mod streaming;

#[cfg(test)]
mod tests;

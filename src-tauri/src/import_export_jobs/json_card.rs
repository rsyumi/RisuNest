use super::{staging::CreatedPayloads, FormatError, ImportLimits, JobStaging, StagedPayload};
use base64::{engine::general_purpose::STANDARD, read::DecoderReader};
use serde_json::Value;
use std::{collections::HashSet, io::Read};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonCardPayload {
    pub json_pointer: String,
    pub reference_key: String,
    pub media_type: String,
    pub declared_extension: Option<String>,
    pub extension: String,
    pub payload: StagedPayload,
}

#[derive(Debug, PartialEq)]
pub struct ParsedJsonCard {
    pub metadata: Value,
    pub payloads: Vec<JsonCardPayload>,
}

pub fn parse_json_card(
    reader: &mut impl Read,
    staging: &JobStaging,
    limits: &ImportLimits,
    cancelled: &impl Fn() -> bool,
) -> Result<ParsedJsonCard, FormatError> {
    let bytes = read_bounded_json(reader, limits.max_metadata_bytes, cancelled)?;
    let mut metadata: Value = serde_json::from_slice(&bytes)
        .map_err(|_| FormatError::invalid("JSON card metadata is not valid JSON"))?;
    if !metadata.is_object() {
        return Err(FormatError::invalid("JSON card root must be an object"));
    }

    let mut payloads = Vec::new();
    let mut total_payload_bytes = 0_u64;
    let mut created = CreatedPayloads::new(staging);
    let mut reserved_references = collect_existing_references(&metadata);
    let Some(assets) = metadata
        .pointer_mut("/data/assets")
        .and_then(Value::as_array_mut)
    else {
        return Ok(ParsedJsonCard { metadata, payloads });
    };

    for (position, asset) in assets.iter_mut().enumerate() {
        if cancelled() {
            return Err(FormatError::cancelled());
        }
        let Some(asset) = asset.as_object_mut() else {
            return Err(FormatError::invalid(format!(
                "JSON card asset {position} is not an object"
            )));
        };
        let Some(uri) = asset.get("uri").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if !uri.starts_with("data:") {
            continue;
        }
        if payloads.len() >= limits.max_payload_count {
            return Err(FormatError::limit(
                "JSON card embedded payload count exceeds its limit",
            ));
        }

        let data_uri = parse_data_uri(&uri)?;
        let declared_extension = match asset.get("ext") {
            None => None,
            Some(Value::String(extension)) => Some(validate_extension(extension)?),
            Some(_) => {
                return Err(FormatError::invalid(
                    "JSON card asset declared extension must be a string",
                ))
            }
        };
        let extension = declared_extension
            .clone()
            .unwrap_or_else(|| extension_for_media_type(data_uri.media_type).to_string());
        let reference_key = format!("native-data-{position}");
        if !reserved_references.insert(reference_key.clone()) {
            return Err(FormatError::invalid(
                "JSON card contains a duplicate staged payload reference",
            ));
        }

        let mut decoder = DecoderReader::new(data_uri.base64.as_bytes(), &STANDARD);
        let payload = staging.stage_reader(&mut decoder, limits.max_payload_bytes, cancelled)?;
        total_payload_bytes = total_payload_bytes
            .checked_add(payload.byte_size)
            .ok_or_else(|| FormatError::limit("JSON card aggregate payload length overflow"))?;
        if total_payload_bytes > limits.max_aggregate_payload_bytes {
            staging.remove(&payload.staged_name);
            return Err(FormatError::limit(
                "JSON card payloads exceed the aggregate byte limit",
            ));
        }
        created.track(&payload);
        asset.insert(
            "uri".to_string(),
            Value::String(format!("__asset:{reference_key}")),
        );
        payloads.push(JsonCardPayload {
            json_pointer: format!("/data/assets/{position}/uri"),
            reference_key,
            media_type: data_uri.media_type.to_string(),
            declared_extension,
            extension,
            payload,
        });
    }

    if cancelled() {
        return Err(FormatError::cancelled());
    }
    created.commit();
    Ok(ParsedJsonCard { metadata, payloads })
}

struct DataUri<'a> {
    media_type: &'a str,
    base64: &'a str,
}

fn parse_data_uri(uri: &str) -> Result<DataUri<'_>, FormatError> {
    let value = uri
        .strip_prefix("data:")
        .ok_or_else(|| FormatError::invalid("embedded payload is not a data URI"))?;
    let (header, encoded) = value
        .split_once(',')
        .ok_or_else(|| FormatError::invalid("data URI has no payload separator"))?;
    let mut parts = header.split(';');
    let media_type = parts
        .next()
        .filter(|value| valid_media_type(value))
        .ok_or_else(|| FormatError::invalid("data URI has an invalid media type"))?;
    let parameters: Vec<_> = parts.collect();
    if parameters.last().copied() != Some("base64")
        || parameters[..parameters.len().saturating_sub(1)]
            .iter()
            .any(|value| !valid_parameter(value))
    {
        return Err(FormatError::invalid(
            "data URI must contain a valid base64 marker",
        ));
    }
    Ok(DataUri {
        media_type,
        base64: encoded,
    })
}

fn valid_media_type(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(kind), Some(subtype), None)
        if valid_token(kind) && valid_token(subtype))
}

fn valid_parameter(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, parameter)| {
        valid_token(name) && !parameter.is_empty() && parameter.is_ascii()
    })
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
}

fn validate_extension(extension: &str) -> Result<String, FormatError> {
    if !extension.is_empty()
        && extension.len() <= 32
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'_' | b'-'))
    {
        return Ok(extension.to_string());
    }
    Err(FormatError::invalid(
        "JSON card asset has an invalid declared extension",
    ))
}

fn extension_for_media_type(media_type: &str) -> &'static str {
    match media_type.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/avif" => "avif",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/json" => "json",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        _ => "bin",
    }
}

fn collect_existing_references(metadata: &Value) -> HashSet<String> {
    metadata
        .pointer("/data/assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|asset| asset.get("uri").and_then(Value::as_str))
        .filter_map(|uri| uri.strip_prefix("__asset:"))
        .map(str::to_owned)
        .collect()
}

fn read_bounded_json(
    reader: &mut impl Read,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<u8>, FormatError> {
    let mut result = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err(FormatError::cancelled());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| FormatError::io("read JSON card", error))?;
        if read == 0 {
            break;
        }
        let next_length = result
            .len()
            .checked_add(read)
            .ok_or_else(|| FormatError::limit("JSON card length overflow"))?;
        if next_length > max_bytes {
            return Err(FormatError::limit(
                "JSON card exceeds its metadata byte limit",
            ));
        }
        result.extend_from_slice(&buffer[..read]);
    }
    Ok(result)
}

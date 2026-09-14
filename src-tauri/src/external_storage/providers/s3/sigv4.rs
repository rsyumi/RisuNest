//! AWS Signature Version 4, header based. The payload hash is supplied by the
//! caller so a multi-gigabyte body never has to be buffered to be signed.
use crate::external_storage::contract::{ErrorKind, ProviderError, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(crate) const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub(crate) const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SERVICE: &str = "s3";
const TERMINATOR: &str = "aws4_request";

/// Access keys, held only while a call runs. No Debug and no Serialize.
pub(crate) struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: zeroize::Zeroizing<String>,
}

/// Signing intermediates are returned so a known-answer test can pin the exact
/// canonical request rather than only the final header.
pub(crate) struct Signature {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub authorization: String,
}

/// URI encoding as SigV4 defines it: every byte outside the unreserved set is
/// escaped with uppercase hexadecimal, and a key's own separators stay literal.
pub(crate) fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(*byte))
            }
            b'/' if !encode_slash => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("hmac takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// `YYYYMMDDTHHMMSSZ` in UTC, the only timestamp form SigV4 accepts.
pub(crate) fn amz_date(now_ms: u64) -> Result<String> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    let seconds = i64::try_from(now_ms / 1000).map_err(|_| corrupt())?;
    let moment = time::OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| corrupt())?;
    if !(1..=9999).contains(&moment.year()) {
        return Err(corrupt());
    }
    Ok(format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    ))
}

/// The `Host` header value the transport will send for this URL.
pub(crate) fn host_header(url: &url::Url) -> Result<String> {
    let host = url
        .host_str()
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// Signs one request. `headers` are exactly the headers that travel with it,
/// `host` excluded because the transport derives that from the URL. All of them
/// are signed, so any of them reaching the service altered invalidates it.
pub(crate) fn sign(
    credentials: &Credentials,
    region: &str,
    method: &str,
    url: &url::Url,
    headers: &BTreeMap<String, String>,
    payload_hash: &str,
    amz_date: &str,
) -> Result<Signature> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    if amz_date.len() != 16 || region.is_empty() || credentials.access_key_id.is_empty() {
        return Err(corrupt());
    }
    let date = &amz_date[..8];
    let mut canonical: BTreeMap<&str, String> = BTreeMap::new();
    canonical.insert("host", host_header(url)?);
    for (name, value) in headers {
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && byte != b'-')
            || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        {
            return Err(corrupt());
        }
        canonical.insert(name.as_str(), collapse_spaces(value));
    }
    let canonical_headers: String = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers = canonical
        .keys()
        .copied()
        .collect::<Vec<_>>()
        .join(";")
        .to_string();
    let canonical_uri = match url.path() {
        "" => "/",
        path => path,
    };
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        url.query().unwrap_or_default()
    );
    let scope = format!("{date}/{region}/{SERVICE}/{TERMINATOR}");
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );
    let seed = format!("AWS4{}", credentials.secret_access_key.as_str());
    let signing_key = hmac_sha256(
        &hmac_sha256(
            &hmac_sha256(
                &hmac_sha256(seed.as_bytes(), date.as_bytes()),
                region.as_bytes(),
            ),
            SERVICE.as_bytes(),
        ),
        TERMINATOR.as_bytes(),
    );
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    Ok(Signature {
        authorization: format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
        canonical_request,
        string_to_sign,
    })
}

fn collapse_spaces(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut spaced = false;
    for character in value.trim().chars() {
        if character == ' ' {
            spaced = true;
            continue;
        }
        if spaced && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        spaced = false;
        collapsed.push(character);
    }
    collapsed
}

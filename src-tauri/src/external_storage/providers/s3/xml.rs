//! The few S3 XML documents this adapter has to understand. Element contents
//! are read by local name so a namespaced gateway parses the same way. Server
//! error documents are never decoded into a message; only the presence of an
//! error root is reported, and the status classifies the failure.
use crate::external_storage::contract::{ErrorKind, ProviderError, Result};
use quick_xml::{events::Event, Reader};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListedObject {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListObjectsPage {
    pub objects: Vec<ListedObject>,
    pub next_continuation_token: Option<String>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListedPart {
    pub number: u32,
    pub size: u64,
    pub etag: String,
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

/// One text leaf with the path of local element names that contains it.
struct Leaf {
    path: Vec<String>,
    text: String,
}

fn scan(xml: &[u8]) -> Result<Vec<Leaf>> {
    let text = std::str::from_utf8(xml).map_err(|_| corrupt())?;
    let mut reader = Reader::from_str(text);
    let mut path: Vec<String> = Vec::new();
    let mut buffer = String::new();
    let mut leaves = Vec::new();
    loop {
        match reader.read_event().map_err(|_| corrupt())? {
            Event::Start(start) => {
                if path.len() >= 16 {
                    return Err(corrupt());
                }
                path.push(local_name(start.local_name().as_ref())?);
                buffer.clear();
            }
            Event::Empty(empty) => {
                if path.len() >= 16 {
                    return Err(corrupt());
                }
                path.push(local_name(empty.local_name().as_ref())?);
                leaves.push(Leaf {
                    path: path.clone(),
                    text: String::new(),
                });
                path.pop();
                buffer.clear();
            }
            Event::End(_) => {
                if path.is_empty() {
                    return Err(corrupt());
                }
                leaves.push(Leaf {
                    path: path.clone(),
                    text: std::mem::take(&mut buffer).trim().to_owned(),
                });
                path.pop();
            }
            Event::Text(value) => {
                buffer.push_str(&value.decode().map_err(|_| corrupt())?);
            }
            Event::CData(value) => {
                buffer.push_str(std::str::from_utf8(value.as_ref()).map_err(|_| corrupt())?);
            }
            Event::GeneralRef(reference) => {
                buffer.push(entity(&reference.decode().map_err(|_| corrupt())?)?);
            }
            Event::Eof => break,
            _ => {}
        }
        if leaves.len() > 64 * 1024 || buffer.len() > 64 * 1024 {
            return Err(corrupt());
        }
    }
    Ok(leaves)
}

fn local_name(bytes: &[u8]) -> Result<String> {
    let name = std::str::from_utf8(bytes).map_err(|_| corrupt())?;
    if name.is_empty() || name.len() > 64 {
        return Err(corrupt());
    }
    Ok(name.to_owned())
}

fn entity(name: &str) -> Result<char> {
    match name {
        "amp" => Ok('&'),
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "quot" => Ok('"'),
        "apos" => Ok('\''),
        _ => {
            let (digits, radix) = match name.strip_prefix('#') {
                Some(rest) => match rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
                    Some(hex) => (hex, 16),
                    None => (rest, 10),
                },
                None => return Err(corrupt()),
            };
            let point = u32::from_str_radix(digits, radix).map_err(|_| corrupt())?;
            char::from_u32(point).ok_or_else(corrupt)
        }
    }
}

fn root(leaves: &[Leaf]) -> Result<&str> {
    leaves
        .first()
        .and_then(|leaf| leaf.path.first())
        .map(String::as_str)
        .ok_or_else(corrupt)
}

fn field<'a>(leaves: &'a [Leaf], path: &[&str]) -> Option<&'a str> {
    leaves
        .iter()
        .find(|leaf| {
            leaf.path
                .iter()
                .map(String::as_str)
                .eq(path.iter().copied())
        })
        .map(|leaf| leaf.text.as_str())
}

/// Groups the leaves of each repeated child element of the root.
fn entries<'a>(leaves: &'a [Leaf], root_name: &str, child: &str) -> Vec<Vec<&'a Leaf>> {
    let mut groups: Vec<Vec<&Leaf>> = Vec::new();
    let mut open = false;
    for leaf in leaves {
        let inside = leaf.path.len() > 2 && leaf.path[0] == root_name && leaf.path[1] == child;
        if inside {
            if !open {
                groups.push(Vec::new());
                open = true;
            }
            if let Some(group) = groups.last_mut() {
                group.push(leaf);
            }
        } else {
            open = false;
        }
    }
    groups
}

fn child<'a>(group: &[&'a Leaf], name: &str) -> Option<&'a str> {
    group
        .iter()
        .find(|leaf| leaf.path.len() == 3 && leaf.path[2] == name)
        .map(|leaf| leaf.text.as_str())
}

fn require_root(leaves: &[Leaf], expected: &str) -> Result<()> {
    if root(leaves)? == expected {
        Ok(())
    } else {
        Err(corrupt())
    }
}

pub(crate) fn parse_initiate_multipart(xml: &[u8]) -> Result<String> {
    let leaves = scan(xml)?;
    require_root(&leaves, "InitiateMultipartUploadResult")?;
    let upload_id = field(&leaves, &["InitiateMultipartUploadResult", "UploadId"])
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or_else(corrupt)?;
    Ok(upload_id.to_owned())
}

/// `CompleteMultipartUpload` may answer 200 with an error document instead of a
/// result, so success needs the result root and nothing is read from an error.
pub(crate) fn parse_complete_multipart(xml: &[u8]) -> Result<Option<String>> {
    let leaves = scan(xml)?;
    require_root(&leaves, "CompleteMultipartUploadResult")?;
    Ok(field(&leaves, &["CompleteMultipartUploadResult", "ETag"])
        .filter(|value| !value.is_empty())
        .map(str::to_owned))
}

pub(crate) fn parse_list_objects(xml: &[u8]) -> Result<ListObjectsPage> {
    let leaves = scan(xml)?;
    require_root(&leaves, "ListBucketResult")?;
    let truncated = field(&leaves, &["ListBucketResult", "IsTruncated"]) == Some("true");
    let objects = entries(&leaves, "ListBucketResult", "Contents")
        .iter()
        .map(|group| {
            Ok(ListedObject {
                key: child(group, "Key")
                    .filter(|key| !key.is_empty())
                    .ok_or_else(corrupt)?
                    .to_owned(),
                size: child(group, "Size")
                    .ok_or_else(corrupt)?
                    .parse()
                    .map_err(|_| corrupt())?,
                etag: child(group, "ETag")
                    .filter(|etag| !etag.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let next_continuation_token = field(&leaves, &["ListBucketResult", "NextContinuationToken"])
        .filter(|token| truncated && !token.is_empty() && token.len() <= 4096)
        .map(str::to_owned);
    if truncated && next_continuation_token.is_none() {
        return Err(corrupt());
    }
    Ok(ListObjectsPage {
        objects,
        next_continuation_token,
    })
}

pub(crate) fn parse_list_parts(xml: &[u8]) -> Result<Vec<ListedPart>> {
    let leaves = scan(xml)?;
    require_root(&leaves, "ListPartsResult")?;
    entries(&leaves, "ListPartsResult", "Part")
        .iter()
        .map(|group| {
            Ok(ListedPart {
                number: child(group, "PartNumber")
                    .ok_or_else(corrupt)?
                    .parse()
                    .map_err(|_| corrupt())?,
                size: child(group, "Size")
                    .ok_or_else(corrupt)?
                    .parse()
                    .map_err(|_| corrupt())?,
                etag: child(group, "ETag")
                    .filter(|etag| !etag.is_empty())
                    .ok_or_else(corrupt)?
                    .to_owned(),
            })
        })
        .collect()
}

pub(crate) fn complete_multipart_body(parts: &[(u32, String)]) -> String {
    let mut body = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        body.push_str("<Part><PartNumber>");
        body.push_str(&number.to_string());
        body.push_str("</PartNumber><ETag>");
        body.push_str(&escape(etag));
        body.push_str("</ETag></Part>");
    }
    body.push_str("</CompleteMultipartUpload>");
    body
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

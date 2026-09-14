//! Reader for a `207 Multi-Status` body. Elements are matched by their resolved
//! `DAV:` namespace, so `D:`, `d:`, another prefix or a default namespace parse
//! the same way, and a status outside 2xx discards exactly what it covers: a
//! whole `response` for a response status, only that group for a `propstat`.
use super::paths::corrupt;
use crate::external_storage::contract::Result;
use quick_xml::{
    events::Event,
    name::{Namespace, ResolveResult},
    NsReader,
};

const DAV: Namespace<'static> = Namespace(b"DAV:");
const MAX_RESPONSES: usize = 20_000;
const MAX_DEPTH: usize = 64;
const MAX_TEXT_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Element {
    MultiStatus,
    Response,
    Href,
    Propstat,
    Prop,
    Status,
    GetEtag,
    GetContentLength,
    ResourceType,
    Collection,
    Other,
}
fn element(resolved: ResolveResult<'_>, local: &[u8]) -> Element {
    if resolved != ResolveResult::Bound(DAV) {
        return Element::Other;
    }
    match local {
        b"multistatus" => Element::MultiStatus,
        b"response" => Element::Response,
        b"href" => Element::Href,
        b"propstat" => Element::Propstat,
        b"prop" => Element::Prop,
        b"status" => Element::Status,
        b"getetag" => Element::GetEtag,
        b"getcontentlength" => Element::GetContentLength,
        b"resourcetype" => Element::ResourceType,
        b"collection" => Element::Collection,
        _ => Element::Other,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Entry {
    pub href: String,
    pub collection: bool,
    pub content_length: Option<u64>,
    /// The `getetag` value exactly as the server wrote it, quotes and any weak
    /// marker included. Strength is judged where a version token is minted.
    pub etag: Option<String>,
}

/// Property values stay unparsed until the group's status is known: a `404`
/// propstat legitimately lists the property names with empty values.
#[derive(Default)]
struct Pending {
    collection: bool,
    content_length: Option<String>,
    etag: Option<String>,
    status: Option<u16>,
}

/// `HTTP/1.1 200 OK` and friends. Anything else is a malformed status line.
fn status_code(line: &str) -> Result<u16> {
    line.split_whitespace()
        .nth(1)
        .filter(|code| code.len() == 3)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(corrupt)
}
fn successful(status: Option<u16>) -> bool {
    status.is_none_or(|status| (200..300).contains(&status))
}

pub(super) fn parse(body: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = NsReader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Element> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut entry: Option<Entry> = None;
    let mut response_status: Option<u16> = None;
    let mut pending = Pending::default();
    let mut text = String::new();
    let mut multistatus = false;
    loop {
        let (resolved, event) = reader.read_resolved_event().map_err(|_| corrupt())?;
        match event {
            Event::Eof => break,
            Event::Start(start) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(corrupt());
                }
                let current = element(resolved, start.local_name().as_ref());
                match (current, stack.last().copied()) {
                    (Element::MultiStatus, None) => multistatus = true,
                    (Element::Response, _) => {
                        entry = Some(Entry::default());
                        response_status = None;
                        pending = Pending::default();
                    }
                    (Element::Collection, Some(Element::ResourceType)) => pending.collection = true,
                    (
                        Element::Href
                        | Element::Status
                        | Element::GetEtag
                        | Element::GetContentLength,
                        _,
                    ) => text.clear(),
                    _ => {}
                }
                stack.push(current);
            }
            Event::End(_) => {
                let current = stack.pop().ok_or_else(corrupt)?;
                finish_element(
                    current,
                    stack.last().copied(),
                    &text,
                    &mut entry,
                    &mut response_status,
                    &mut pending,
                    &mut entries,
                )?;
            }
            Event::Text(chunk) => {
                text.push_str(&chunk.xml10_content().map_err(|_| corrupt())?);
            }
            Event::CData(chunk) => {
                text.push_str(&chunk.decode().map_err(|_| corrupt())?);
            }
            Event::GeneralRef(reference) => {
                let name = reference.decode().map_err(|_| corrupt())?;
                let reference = format!("&{name};");
                let expanded = quick_xml::escape::unescape(&reference).map_err(|_| corrupt())?;
                text.push_str(&expanded);
            }
            _ => {}
        }
        if text.len() > MAX_TEXT_BYTES || entries.len() > MAX_RESPONSES {
            return Err(corrupt());
        }
    }
    // A body that is not a multistatus document says nothing about members,
    // and an empty member list would read as "the collection is empty".
    if !stack.is_empty() || !multistatus {
        return Err(corrupt());
    }
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn finish_element(
    current: Element,
    parent: Option<Element>,
    text: &str,
    entry: &mut Option<Entry>,
    response_status: &mut Option<u16>,
    pending: &mut Pending,
    entries: &mut Vec<Entry>,
) -> Result<()> {
    if current == Element::Response {
        if let Some(finished) = entry.take() {
            if successful(*response_status) && !finished.href.is_empty() {
                entries.push(finished);
            }
        }
        *response_status = None;
        *pending = Pending::default();
        return Ok(());
    }
    let Some(open) = entry.as_mut() else {
        return Ok(());
    };
    match (current, parent) {
        (Element::Href, Some(Element::Response)) => {
            if open.href.is_empty() {
                open.href = text.trim().to_owned();
            }
        }
        (Element::Status, Some(Element::Response)) => {
            *response_status = Some(status_code(text)?);
        }
        (Element::Status, Some(Element::Propstat)) => {
            pending.status = Some(status_code(text)?);
        }
        (Element::GetEtag, Some(Element::Prop)) => {
            pending.etag = Some(text.trim().to_owned());
        }
        (Element::GetContentLength, Some(Element::Prop)) => {
            pending.content_length = Some(text.trim().to_owned());
        }
        (Element::Propstat, _) => {
            if successful(pending.status) {
                open.collection |= pending.collection;
                if let Some(raw) = pending.content_length.take().filter(|raw| !raw.is_empty()) {
                    open.content_length = Some(raw.parse::<u64>().map_err(|_| corrupt())?);
                }
                open.etag = pending
                    .etag
                    .take()
                    .filter(|etag| !etag.is_empty())
                    .or_else(|| open.etag.take());
            }
            *pending = Pending::default();
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_dav_prefix_parses_and_non_dav_elements_are_ignored() {
        let body = br#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:x="http://example.invalid/ns">
  <D:response>
    <D:href>/dav/root/packs/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype>
      <x:getcontentlength>9999</x:getcontentlength></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  </D:response>
  <d:response xmlns:d="DAV:">
    <d:href>/dav/root/packs/obj%20one</d:href>
    <d:propstat><d:prop><d:getcontentlength>12</d:getcontentlength>
      <d:getetag>"strong-1"</d:getetag>
      <d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <response xmlns="DAV:">
    <href>/dav/root/packs/obj%20two</href>
    <propstat><prop><getcontentlength>34</getcontentlength></prop>
      <status>HTTP/1.1 200 OK</status></propstat>
    <propstat><prop><getetag/></prop><status>HTTP/1.1 404 Not Found</status></propstat>
  </response>
  <D:response>
    <D:href>/dav/root/packs/gone</D:href>
    <D:status>HTTP/1.1 404 Not Found</D:status>
  </D:response>
</D:multistatus>"#;
        let entries = parse(body).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[0].collection);
        assert_eq!(entries[0].content_length, None);
        assert_eq!(entries[1].href, "/dav/root/packs/obj%20one");
        assert_eq!(entries[1].content_length, Some(12));
        assert_eq!(entries[1].etag.as_deref(), Some("\"strong-1\""));
        assert!(!entries[1].collection);
        assert_eq!(entries[2].content_length, Some(34));
        assert_eq!(entries[2].etag, None);
    }

    #[test]
    fn entities_cdata_and_malformed_documents() {
        let body = br#"<multistatus xmlns="DAV:"><response>
  <href>/dav/root/a&amp;b/c&#37;d</href>
  <propstat><prop><getcontentlength><![CDATA[7]]></getcontentlength>
    <getetag>W/&quot;weak&quot;</getetag></prop>
  <status>HTTP/1.1 200 OK</status></propstat>
</response></multistatus>"#;
        let entries = parse(body).unwrap();
        assert_eq!(entries[0].href, "/dav/root/a&b/c%d");
        assert_eq!(entries[0].content_length, Some(7));
        assert_eq!(entries[0].etag.as_deref(), Some("W/\"weak\""));
        assert!(parse(b"<multistatus xmlns=\"DAV:\"><response>").is_err());
        assert!(parse(b"not xml at all").is_err());
        let bad_status = br#"<multistatus xmlns="DAV:"><response><href>/a</href>
  <propstat><prop><getcontentlength>1</getcontentlength></prop>
  <status>garbage</status></propstat></response></multistatus>"#;
        assert!(parse(bad_status).is_err());
        let bad_length = br#"<multistatus xmlns="DAV:"><response><href>/a</href>
  <propstat><prop><getcontentlength>-1</getcontentlength></prop>
  <status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        assert!(parse(bad_length).is_err());
    }
}

//! Path handling for the DAV namespace. Every segment is percent-encoded with
//! the RFC 3986 unreserved set, so spaces, reserved delimiters and non-ASCII
//! names survive a round trip, and an `href` returned in a multistatus response
//! is resolved against the request URL before it is matched back to a member.
use crate::external_storage::contract::{ErrorKind, ProviderError, Result};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use url::Url;

/// RFC 3986 unreserved characters stay literal. Everything else, including the
/// segment separator and every non-ASCII byte, is escaped.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

const MAX_SEGMENT_BYTES: usize = 255;
const MAX_SEGMENTS: usize = 32;

pub(super) fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

/// A name usable as one DAV collection member. Unicode is allowed; separators,
/// relative markers, control characters and empty names are not.
pub(super) fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_BYTES
        && segment != "."
        && segment != ".."
        && !segment.contains('/')
        && !segment.chars().any(char::is_control)
}

/// Splits a configured root or a locator object into raw (decoded) segments.
pub(super) fn split_path(path: &str) -> Option<Vec<String>> {
    let segments: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .map(str::to_owned)
        .collect();
    let usable = !segments.is_empty()
        && segments.len() <= MAX_SEGMENTS
        && segments.iter().all(|segment| valid_segment(segment));
    usable.then_some(segments)
}

pub(super) fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT).to_string()
}

fn extend(base: &Url, relative: &[String], trailing_slash: bool) -> Url {
    let mut path = base.path().trim_end_matches('/').to_owned();
    for segment in relative {
        path.push('/');
        path.push_str(&encode_segment(segment));
    }
    if trailing_slash {
        path.push('/');
    }
    let mut url = base.clone();
    url.set_path(&path);
    url
}

/// A non-collection member below `base`.
pub(super) fn object_url(base: &Url, relative: &[String]) -> Url {
    extend(base, relative, false)
}

/// A collection below `base`. The trailing slash keeps servers that would
/// redirect a bare collection request from answering with a 3xx we refuse.
pub(super) fn collection_url(base: &Url, relative: &[String]) -> Url {
    extend(base, relative, true)
}

/// Decoded path segments, with the empty segments produced by a trailing or
/// duplicated separator removed.
pub(super) fn decoded_segments(url: &Url) -> Result<Vec<String>> {
    let mut segments = Vec::new();
    for raw in url.path_segments().ok_or_else(corrupt)? {
        if raw.is_empty() {
            continue;
        }
        segments.push(
            percent_decode_str(raw)
                .decode_utf8()
                .map_err(|_| corrupt())?
                .into_owned(),
        );
    }
    Ok(segments)
}

/// Resolves an `href` against the request URL. A different origin is ignored:
/// the response may name resources this connection has no business following.
pub(super) fn resolve_href(request: &Url, href: &str) -> Option<Url> {
    let resolved = Url::options()
        .base_url(Some(request))
        .parse(href.trim())
        .ok()?;
    (resolved.origin() == request.origin()).then_some(resolved)
}

/// The member name when `target` is a direct child of `collection`.
pub(super) fn direct_member(collection: &[String], target: &[String]) -> Option<String> {
    (target.len() == collection.len() + 1 && target.starts_with(collection))
        .then(|| target[collection.len()].clone())
}

/// Provider, endpoint, account and root in one string another device computes
/// identically from the same configuration. Lengths prefix each part so no
/// combination of separators inside a part can produce another identity.
pub(super) fn connection_identity(base: &Url, account: &str, root: &[String]) -> String {
    let mut identity = String::from("webdav");
    for part in [base.as_str(), account, &root.join("/")] {
        identity.push(':');
        identity.push_str(&part.len().to_string());
        identity.push(':');
        identity.push_str(part);
    }
    identity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://dav.invalid/dav/Koofr").unwrap()
    }

    #[test]
    fn segments_reject_relative_markers_and_separators_but_keep_unicode() {
        assert!(valid_segment("한글 이름 #1"));
        assert!(valid_segment("a+b;c%d?e"));
        for rejected in ["", ".", "..", "a/b", "a\u{0}b", "a\nb"] {
            assert!(!valid_segment(rejected), "{rejected:?}");
        }
        assert!(!valid_segment(&"x".repeat(MAX_SEGMENT_BYTES + 1)));
        assert_eq!(
            split_path("/packs/sub/").unwrap(),
            vec!["packs".to_owned(), "sub".to_owned()]
        );
        assert!(split_path("packs//sub").is_none());
        assert!(split_path("packs/../etc").is_none());
        assert!(split_path("").is_none());
    }

    #[test]
    fn reserved_and_unicode_names_are_escaped_per_segment() {
        assert_eq!(
            encode_segment("백업 폴더"),
            "%EB%B0%B1%EC%97%85%20%ED%8F%B4%EB%8D%94"
        );
        assert_eq!(
            encode_segment("obj #1?a%b+c;d&e"),
            "obj%20%231%3Fa%25b%2Bc%3Bd%26e"
        );
        assert_eq!(encode_segment("keep-._~"), "keep-._~");
        let url = object_url(&base(), &["백업 폴더".into(), "obj #1?a%b+c;d&e".into()]);
        assert_eq!(
            url.path(),
            format!(
                "/dav/Koofr/{}/{}",
                encode_segment("백업 폴더"),
                encode_segment("obj #1?a%b+c;d&e")
            )
        );
        assert_eq!(url.query(), None);
        assert!(collection_url(&base(), &["packs".into()])
            .as_str()
            .ends_with("/dav/Koofr/packs/"));
    }

    #[test]
    fn hrefs_resolve_relative_absolute_and_foreign_origins() {
        let request = collection_url(&base(), &["백업 폴더".into()]);
        let collection = decoded_segments(&request).unwrap();
        for href in [
            "/dav/Koofr/%EB%B0%B1%EC%97%85%20%ED%8F%B4%EB%8D%94/obj%20%231",
            "https://dav.invalid/dav/Koofr/%EB%B0%B1%EC%97%85%20%ED%8F%B4%EB%8D%94/obj%20%231",
            "  obj%20%231  ",
        ] {
            let resolved = resolve_href(&request, href).unwrap();
            let target = decoded_segments(&resolved).unwrap();
            assert_eq!(
                direct_member(&collection, &target).as_deref(),
                Some("obj #1")
            );
        }
        assert!(resolve_href(&request, "https://other.invalid/dav/Koofr/x").is_none());
        let itself = decoded_segments(&request).unwrap();
        assert_eq!(direct_member(&collection, &itself), None);
    }

    #[test]
    fn identity_separates_endpoint_account_and_root() {
        let ambiguous = connection_identity(&base(), "user:1", &["a".into()]);
        let other = connection_identity(&base(), "user", &["1:a".into()]);
        assert_ne!(ambiguous, other);
        assert_eq!(
            connection_identity(&base(), "user", &["a".into()]),
            connection_identity(&base(), "user", &["a".into()])
        );
    }
}

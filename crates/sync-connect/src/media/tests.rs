use super::*;

fn object() -> MediaObject {
    MediaObject {
        hash: "a".repeat(64),
        size: 42.into(),
        mime: "image/png".into(),
    }
}
fn claims(signer: &MediaSigner) -> MediaClaims {
    let object = object();
    MediaClaims {
        library_id: "library".into(),
        epoch: "epoch".into(),
        device_id: "device".into(),
        expires: 123.into(),
        request: MediaRequest {
            refresh_url: signer
                .refresh_url("http://127.0.0.1:12345", &object)
                .unwrap(),
            object,
        },
    }
}
#[test]
fn authenticated_tokens_are_scoped_and_tampering_is_rejected() {
    let signer = MediaSigner::new(&[7; 32]).unwrap();
    let claims = claims(&signer);
    let token = signer.sign_access(&claims).unwrap();
    assert_eq!(
        signer.verify_access(&token).unwrap().request.object,
        object()
    );
    assert!(MediaSigner::new(&[8; 32])
        .unwrap()
        .verify_access(&token)
        .is_err());
    assert!(signer.verify_refresh(&token).is_err());
    let (body, tag) = split_token(&token).unwrap();
    let mut value: MediaClaims = decode_value(&body).unwrap();
    value.request.object.mime = "text/html".into();
    let modified = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(canonical::encode(&value).unwrap()),
        URL_SAFE_NO_PAD.encode(tag)
    );
    assert!(signer.verify_access(&modified).is_err());
    assert!(signer
        .verify_access(&"a".repeat(MAX_TOKEN_BYTES + 1))
        .is_err());
}
#[test]
fn refresh_capability_cannot_be_changed_to_another_file_or_endpoint() {
    let signer = MediaSigner::new(&[7; 32]).unwrap();
    let claims = claims(&signer);
    let url = url::Url::parse(&claims.request.refresh_url).unwrap();
    let token = url.path().strip_prefix(REFRESH_PATH).unwrap();
    assert_eq!(signer.verify_refresh(token).unwrap(), object());
    assert!(MediaSigner::new(&[9; 32])
        .unwrap()
        .verify_refresh(token)
        .is_err());
    for refresh_url in [
        claims
            .request
            .refresh_url
            .replace("127.0.0.1", "outside.invalid"),
        claims.request.refresh_url.replace("http:", "https:"),
        claims.request.refresh_url.replace(REFRESH_PATH, "/admin/"),
        format!("{}?extra=1", claims.request.refresh_url),
        format!("{}#fragment", claims.request.refresh_url),
    ] {
        assert!(MediaRequest {
            object: object(),
            refresh_url
        }
        .validate()
        .is_err());
    }
    let mut request = claims.request;
    request.object.hash = "b".repeat(64);
    assert!(request.validate().is_err());
}
#[test]
fn rejects_header_injection_and_noncanonical_tokens() {
    let signer = MediaSigner::new(&[7; 32]).unwrap();
    for mime in ["", "image/png\r\nX-Header: injected", "image/png\0"] {
        let mut object = object();
        object.mime = mime.into();
        assert!(object.validate().is_err());
    }
    let token = signer.sign_access(&claims(&signer)).unwrap();
    assert!(signer.verify_access(&format!("{token}=")).is_err());
}

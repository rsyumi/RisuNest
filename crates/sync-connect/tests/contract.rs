use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use risunest_sync_connect::{open_endpoint, seal_endpoint, Directory, Registration};

fn directory() -> Directory {
    Directory {
        base_url: "https://registry.example".into(),
        uuid: "12345678-1234-4234-9234-123456789abc".into(),
        key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
    }
}

#[test]
fn registration_roundtrip_preserves_device_and_directory() {
    let value = Registration {
        endpoint: "https://sync.example/base".into(),
        library_id: "library".into(),
        device_id: "device".into(),
        token: "a".repeat(64),
        directory: Some(directory()),
    };
    let uri = value.encode_uri().unwrap();
    let decoded = Registration::parse_uri(&uri).unwrap();
    assert_eq!(decoded.endpoint, value.endpoint);
    assert_eq!(decoded.token, value.token);
    assert_eq!(decoded.directory.unwrap().uuid, directory().uuid);
}

#[test]
fn endpoint_roundtrip_requires_original_key_and_uuid() {
    let config = directory();
    let envelope = seal_endpoint(&config.uuid, &config.key, "https://sync.example/base").unwrap();
    assert_eq!(
        open_endpoint(&config.uuid, &config.key, &envelope).unwrap(),
        "https://sync.example/base"
    );
    assert!(open_endpoint(
        "12345678-1234-4234-8234-123456789abc",
        &config.key,
        &envelope
    )
    .is_err());
    assert!(open_endpoint(
        &config.uuid,
        "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE",
        &envelope
    )
    .is_err());
}

fn raw_uri(json: &str) -> String {
    format!(
        "{}{}",
        risunest_sync_connect::REGISTRATION_PREFIX,
        URL_SAFE_NO_PAD.encode(json)
    )
}
fn registration() -> Registration {
    Registration {
        endpoint: "https://sync.example/base".into(),
        library_id: "library".into(),
        device_id: "device".into(),
        token: "a".repeat(64),
        directory: None,
    }
}

#[test]
fn matches_the_worker_webcrypto_golden_vector() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../server/endpoint-registry/tests/envelope-vector.json"
    ))
    .unwrap();
    let hex = vector["keyHex"].as_str().unwrap();
    let key: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    assert_eq!(
        open_endpoint(
            vector["uuid"].as_str().unwrap(),
            &URL_SAFE_NO_PAD.encode(key),
            vector["envelope"].as_str().unwrap()
        )
        .unwrap(),
        vector["plaintext"].as_str().unwrap()
    );
}

#[test]
fn rejects_duplicate_unknown_partial_and_null_registration_fields() {
    let good = serde_json::to_string(&registration()).unwrap();
    for bad in [
        good.replacen('{', "{\"endpoint\":\"https://other.example\",", 1),
        good.replacen('{', "{\"extra\":1,", 1),
        good.replacen('{', "{\"directory\":null,", 1),
        good.replacen('{', "{\"directory\":{},", 1),
        good.replace("\"device\"", "null"),
    ] {
        assert!(Registration::parse_uri(&raw_uri(&bad)).is_err());
    }
}

#[test]
fn rejects_malformed_uri_and_noncanonical_encoding() {
    let uri = registration().encode_uri().unwrap();
    for bad in [
        format!("{uri}="),
        format!("{uri}\n"),
        uri.replace("register#", "register?token="),
        uri.replace("risunestlocal", "https"),
        raw_uri("[]"),
        raw_uri("{}"),
        "RISUNEST-CONNECT:abc".into(),
        format!(
            "{}{}",
            risunest_sync_connect::REGISTRATION_PREFIX,
            "a".repeat(2048)
        ),
    ] {
        assert!(Registration::parse_uri(&bad).is_err());
    }
    let bad_utf8 = format!(
        "{}{}",
        risunest_sync_connect::REGISTRATION_PREFIX,
        URL_SAFE_NO_PAD.encode([255])
    );
    assert!(Registration::parse_uri(&bad_utf8).is_err());
}

#[test]
fn bounded_uri_preserves_long_input_without_truncation() {
    let mut value = registration();
    let mut last = None;
    loop {
        value.endpoint.push('x');
        match value.encode_uri() {
            Ok(uri) => last = Some(uri),
            Err(error) => {
                assert_eq!(error.0, "registration-too-large");
                break;
            }
        }
    }
    let uri = last.unwrap();
    assert!(uri.len() <= risunest_sync_connect::MAX_REGISTRATION_URI_BYTES);
    assert!(uri.len() > risunest_sync_connect::MAX_REGISTRATION_URI_BYTES - 4);
    assert!(Registration::parse_uri(&uri).is_ok());
}

#[test]
fn rejects_unsafe_endpoints_and_invalid_directory_credentials() {
    for endpoint in [
        "http://192.168.1.1",
        "file:///test",
        "https://u:p@sync.example",
        "https://sync.example/?",
        "https://sync.example/#",
        " https://sync.example",
        "https://sync.example/\\x",
    ] {
        let mut value = registration();
        value.endpoint = endpoint.into();
        assert!(value.encode_uri().is_err());
    }
    let mut value = directory();
    for id in [
        "bad",
        "12345678-1234-1234-9234-123456789abc",
        "12345678-1234-4234-1234-123456789abc",
    ] {
        value.uuid = id.into();
        assert!(value.validate().is_err());
    }
    value = directory();
    value.key.push('=');
    assert!(value.validate().is_err());
}

#[test]
fn randomized_envelopes_detect_nonce_ciphertext_and_tag_corruption() {
    let config = directory();
    let a = seal_endpoint(&config.uuid, &config.key, "https://sync.example").unwrap();
    let b = seal_endpoint(&config.uuid, &config.key, "https://sync.example").unwrap();
    assert_ne!(a, b);
    let bytes = URL_SAFE_NO_PAD.decode(&a).unwrap();
    for position in [0, 12, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[position] ^= 1;
        assert!(open_endpoint(&config.uuid, &config.key, &URL_SAFE_NO_PAD.encode(bad)).is_err());
    }
    assert!(seal_endpoint(&config.uuid, &config.key, "http://127.0.0.1:4319").is_err());
}

#[test]
fn registry_wire_maximum_and_uuid_normalization() {
    let config = directory();
    let prefix = "https://sync.example/";
    let endpoint = format!("{prefix}{}", "a".repeat(4096 - prefix.len()));
    let envelope = seal_endpoint(&config.uuid.to_uppercase(), &config.key, &endpoint).unwrap();
    assert_eq!(envelope.len(), risunest_sync_connect::MAX_ENVELOPE_TEXT);
    assert_eq!(
        open_endpoint(&config.uuid, &config.key, &envelope).unwrap(),
        endpoint
    );
    assert!(seal_endpoint(&config.uuid, &config.key, &(endpoint + "x")).is_err());
}

#[test]
fn registration_matches_typescript_golden_vector() {
    let vector: serde_json::Value =
        serde_json::from_str(include_str!("registration-vector.json")).unwrap();
    let value: Registration = serde_json::from_value(vector["registration"].clone()).unwrap();
    assert_eq!(value.encode_uri().unwrap(), vector["uri"].as_str().unwrap());
}

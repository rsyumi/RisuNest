use serde_json::Value;
use sha2::{Digest, Sha256};

const COMPATIBILITY_MANIFEST: &str = include_str!("../fixtures/roadmap14-compatibility.json");
const CANONICAL_PARITY_FIXTURE: &str = include_str!("../fixtures/roadmap14-canonical-parity.json");

fn push_u32(output: &mut Vec<u8>, value: usize) {
    output.extend_from_slice(&(value as u32).to_be_bytes());
}

fn push_length_delimited(output: &mut Vec<u8>, value: &[u8]) {
    push_u32(output, value.len());
    output.extend_from_slice(value);
}

fn encode_canonical(value: &Value) -> Vec<u8> {
    let tag = value
        .get("tag")
        .and_then(Value::as_str)
        .expect("canonical value must have a string tag");
    let mut output = Vec::new();
    match tag {
        "absent" => output.push(b'A'),
        "array-hole" => output.push(b'H'),
        "undefined" => output.push(b'U'),
        "null" => output.push(b'N'),
        "boolean" => output.push(if value["value"].as_bool().unwrap() {
            b'T'
        } else {
            b'F'
        }),
        "number" => {
            output.push(b'D');
            push_length_delimited(&mut output, &value["value"].as_f64().unwrap().to_be_bytes());
        }
        "string" => {
            output.push(b'S');
            push_length_delimited(&mut output, value["value"].as_str().unwrap().as_bytes());
        }
        "array" => {
            let items = value["items"].as_array().unwrap();
            output.push(b'L');
            push_u32(&mut output, items.len());
            for item in items {
                push_length_delimited(&mut output, &encode_canonical(item));
            }
        }
        "object" => {
            let entries = value["entries"].as_array().unwrap();
            output.push(b'O');
            push_u32(&mut output, entries.len());
            for entry in entries {
                let pair = entry.as_array().unwrap();
                push_length_delimited(&mut output, pair[0].as_str().unwrap().as_bytes());
                push_length_delimited(&mut output, &encode_canonical(&pair[1]));
            }
        }
        _ => panic!("unknown canonical tag: {tag}"),
    }
    output
}

#[test]
fn rust_test_oracle_matches_the_roadmap14_canonical_fixture_hash() {
    let manifest: Value = serde_json::from_str(COMPATIBILITY_MANIFEST).unwrap();
    let fixture: Value = serde_json::from_str(CANONICAL_PARITY_FIXTURE).unwrap();
    let expected = manifest["paritySha256"].as_str().unwrap();

    assert_eq!(
        manifest["canonicalEncoding"],
        "roadmap14-length-delimited-v1"
    );
    assert_eq!(
        hex::encode(Sha256::digest(encode_canonical(&fixture))),
        expected
    );
}

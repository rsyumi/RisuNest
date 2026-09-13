//! Synthetic interoperability vector generator, excluded from normal builds.
use risunest_external_storage_format::{content_identity::hash, crypto, pack};
fn main() {
    if let Some(path) = std::env::args().nth(1) {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let bytes = |key: &str| serde_json::from_value::<Vec<u8>>(value[key].clone()).unwrap();
        let plaintext = bytes("plaintext");
        assert_eq!(
            pack::decompress(&bytes("compressed"), plaintext.len(), &hash(&plaintext)).unwrap(),
            plaintext
        );
        let code = crypto::RecoveryCode::parse(value["code"].as_str().unwrap()).unwrap();
        let recovered = crypto::RecoveryEnvelope::decode(&bytes("recovery"))
            .unwrap()
            .recover("synthetic-repository", &code)
            .unwrap();
        assert_eq!(*recovered.root, [7; 32]);
        assert_eq!(
            &*recovered.connection_metadata,
            "https://synthetic.invalid/folder"
        );
        println!("WASM/native compression and independent recovery vectors passed.");
        return;
    }
    let plaintext = (0..100_005)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let key = [7; 32];
    let binding = "synthetic-repository/synthetic-object/data/v1";
    let mut ciphertext = Vec::new();
    let code = crypto::RecoveryCode::generate().unwrap();
    let recovery = crypto::RecoveryEnvelope::protect(
        "synthetic-repository".into(),
        "https://synthetic.invalid/folder".into(),
        &key,
        &code,
    )
    .unwrap()
    .encode()
    .unwrap();
    let compressed = pack::compress(&plaintext).unwrap();
    crypto::encrypt(
        &mut std::io::Cursor::new(&plaintext),
        &mut ciphertext,
        &key,
        binding.as_bytes(),
        plaintext.len() as u64,
    )
    .unwrap();
    println!(
        "{}",
        serde_json::json!({"key":key,"binding":binding,"plaintext":plaintext,"ciphertext":ciphertext,"hash":hash(&plaintext),"compressed":compressed,"code":&*code.expose(),"recovery":recovery})
    );
}

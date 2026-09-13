//! Synthetic interoperability vector generator, excluded from normal builds.
use risunest_external_storage_format::{content_identity::hash, crypto};
fn main() {
    let plaintext = (0..100_005)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let key = [7; 32];
    let binding = "synthetic-repository/synthetic-object/data/v1";
    let mut ciphertext = Vec::new();
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
        serde_json::json!({"key":key,"binding":binding,"plaintext":plaintext,"ciphertext":ciphertext,"hash":hash(&plaintext)})
    );
}

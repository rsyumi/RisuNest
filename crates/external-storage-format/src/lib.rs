//! Bounded, provider/OS/PDS-independent byte formats shared by native and WASM.
pub mod catalog;
pub mod content_identity;
pub mod crypto;
pub mod format;
pub mod logical_records;
pub mod pack;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatError(pub &'static str);
impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for FormatError {}
impl From<std::io::Error> for FormatError {
    fn from(_: std::io::Error) -> Self {
        Self("object-io-failed")
    }
}
pub type Result<T> = std::result::Result<T, FormatError>;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use wasm_bindgen::prelude::*;
    #[wasm_bindgen]
    pub fn canonical_record_key(key: &str) -> std::result::Result<String, JsValue> {
        let locator = super::logical_records::decode_logical_record_key(key)
            .map_err(|_| JsValue::from_str("invalid-record-key"))?;
        super::logical_records::encode_logical_record_key(&locator)
            .map_err(|_| JsValue::from_str("invalid-record-key"))
    }
    #[wasm_bindgen]
    pub fn content_hash(bytes: &[u8]) -> std::result::Result<Vec<u8>, JsValue> {
        if bytes.len() > super::pack::MAX_CHUNK_BYTES {
            return Err(JsValue::from_str("chunk-too-large"));
        }
        Ok(super::content_identity::hash(bytes).to_vec())
    }
    #[wasm_bindgen]
    pub fn verify_encrypted_object(
        ciphertext: &[u8],
        key: &[u8],
        binding: &str,
    ) -> std::result::Result<Vec<u8>, JsValue> {
        if ciphertext.len() > 2 * super::pack::MAX_CHUNK_BYTES {
            return Err(JsValue::from_str("object-too-large"));
        }
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| JsValue::from_str("invalid-key"))?;
        let mut output = Vec::new();
        super::crypto::decrypt(
            &mut std::io::Cursor::new(ciphertext),
            &mut output,
            &key,
            binding.as_bytes(),
            super::pack::MAX_CHUNK_BYTES as u64,
        )
        .map_err(|e| JsValue::from_str(e.0))?;
        Ok(output)
    }
}

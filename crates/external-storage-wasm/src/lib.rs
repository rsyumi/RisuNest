//! Thin WASM ABI. All format and crypto logic lives in external-storage-format.
#[cfg(target_arch = "wasm32")]
mod wasm {
    use wasm_bindgen::prelude::*;
    #[wasm_bindgen]
    pub fn canonical_record_key(key: &str) -> std::result::Result<String, JsValue> {
        let locator =
            risunest_external_storage_format::logical_records::decode_logical_record_key(key)
                .map_err(|_| JsValue::from_str("invalid-record-key"))?;
        risunest_external_storage_format::logical_records::encode_logical_record_key(&locator)
            .map_err(|_| JsValue::from_str("invalid-record-key"))
    }
    #[wasm_bindgen]
    pub fn content_hash(bytes: &[u8]) -> std::result::Result<Vec<u8>, JsValue> {
        if bytes.len() > risunest_external_storage_format::pack::MAX_CHUNK_BYTES {
            return Err(JsValue::from_str("chunk-too-large"));
        }
        Ok(risunest_external_storage_format::content_identity::hash(bytes).to_vec())
    }
    #[wasm_bindgen]
    pub fn verify_encrypted_object(
        ciphertext: &[u8],
        key: &[u8],
        binding: &str,
    ) -> std::result::Result<Vec<u8>, JsValue> {
        if ciphertext.len() > 2 * risunest_external_storage_format::pack::MAX_CHUNK_BYTES {
            return Err(JsValue::from_str("object-too-large"));
        }
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| JsValue::from_str("invalid-key"))?;
        let mut output = Vec::new();
        risunest_external_storage_format::crypto::decrypt(
            &mut std::io::Cursor::new(ciphertext),
            &mut output,
            &key,
            binding.as_bytes(),
            risunest_external_storage_format::pack::MAX_CHUNK_BYTES as u64,
        )
        .map_err(|e| JsValue::from_str(e.0))?;
        Ok(output)
    }
}

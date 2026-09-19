//! Thin WASM ABI. All format and crypto logic lives in external-storage-format.
#[cfg(target_arch = "wasm32")]
mod wasm {
    use risunest_external_storage_format::{
        control::{
            BackupBundleDocument, BackupPointDocument, HeadDocument, MAX_CONTROL_BYTES,
        },
        pack,
        snapshot::{
            keyed_object_id as format_keyed_object_id, open_envelope, seal_envelope, ObjectRole,
            PublicObjectHeader, SyncStateDocument, MAX_METADATA_BYTES,
        },
    };
    use wasm_bindgen::prelude::*;

    fn parse_key(bytes: &[u8]) -> std::result::Result<[u8; 32], JsValue> {
        bytes
            .try_into()
            .map_err(|_| JsValue::from_str("invalid-key"))
    }

    fn role(value: &str) -> std::result::Result<ObjectRole, JsValue> {
        match value {
            "descriptor" => Ok(ObjectRole::Descriptor),
            "pack" => Ok(ObjectRole::Pack),
            "catalog" => Ok(ObjectRole::Catalog),
            "state" => Ok(ObjectRole::SyncState),
            "bundle" => Ok(ObjectRole::BackupBundle),
            "backupPoint" => Ok(ObjectRole::BackupPoint),
            "inventoryPage" => Ok(ObjectRole::InventoryPage),
            "head" => Ok(ObjectRole::Head),
            _ => Err(JsValue::from_str("invalid-object-role")),
        }
    }

    fn validate_document(role: ObjectRole, bytes: &[u8]) -> std::result::Result<(), JsValue> {
        match role {
            ObjectRole::SyncState => {
                SyncStateDocument::decode(bytes, MAX_METADATA_BYTES).map(|_| ())
            }
            ObjectRole::BackupBundle => {
                BackupBundleDocument::decode(bytes, MAX_CONTROL_BYTES).map(|_| ())
            }
            ObjectRole::Head => HeadDocument::decode(bytes, MAX_CONTROL_BYTES).map(|_| ()),
            ObjectRole::BackupPoint => {
                BackupPointDocument::decode(bytes, MAX_CONTROL_BYTES).map(|_| ())
            }
            ObjectRole::InventoryPage => {
                risunest_external_storage_format::control::InventoryPageDocument::decode(
                    bytes,
                    MAX_CONTROL_BYTES,
                )
                .map(|_| ())
            }
            _ => Ok(()),
        }
        .map_err(|e| JsValue::from_str(e.0))
    }
    #[wasm_bindgen]
    pub fn compress_chunk(
        bytes: &[u8],
        already_compressed: bool,
    ) -> std::result::Result<Vec<u8>, JsValue> {
        let policy = if already_compressed {
            pack::CompressionPolicy::AlreadyCompressed
        } else {
            pack::CompressionPolicy::Text
        };
        pack::ChunkEncoder::new()
            .and_then(|mut encoder| encoder.encode(bytes, policy))
            .map_err(|e| JsValue::from_str(e.0))
    }
    #[wasm_bindgen]
    pub fn decompress_chunk(
        bytes: &[u8],
        length: usize,
        hash: &[u8],
    ) -> std::result::Result<Vec<u8>, JsValue> {
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| JsValue::from_str("invalid-hash"))?;
        pack::decompress(bytes, length, &hash).map_err(|e| JsValue::from_str(e.0))
    }
    #[wasm_bindgen]
    pub fn seal_object_envelope(
        plaintext: &[u8],
        key_bytes: &[u8],
        repository_id: &str,
        object_id: &str,
        role_name: &str,
    ) -> std::result::Result<Vec<u8>, JsValue> {
        if plaintext.len() > MAX_METADATA_BYTES {
            return Err(JsValue::from_str("object-too-large"));
        }
        let key = parse_key(key_bytes)?;
        let object_role = role(role_name)?;
        validate_document(object_role, plaintext)?;
        let header = PublicObjectHeader::new(
            repository_id.into(),
            object_id.into(),
            object_role,
            plaintext.len() as u64,
        )
        .map_err(|e| JsValue::from_str(e.0))?;
        let mut output = Vec::new();
        seal_envelope(
            &mut std::io::Cursor::new(plaintext),
            &mut output,
            &key,
            &header,
        )
        .map_err(|e| JsValue::from_str(e.0))?;
        Ok(output)
    }

    #[wasm_bindgen]
    pub fn open_object_envelope(
        envelope: &[u8],
        key_bytes: &[u8],
        repository_id: &str,
        object_id: &str,
        role_name: &str,
        max_plaintext: usize,
    ) -> std::result::Result<Vec<u8>, JsValue> {
        let key = parse_key(key_bytes)?;
        let expected_role = role(role_name)?;
        let mut plaintext = Vec::new();
        let header = open_envelope(
            &mut std::io::Cursor::new(envelope),
            &mut plaintext,
            &key,
            max_plaintext.min(MAX_METADATA_BYTES) as u64,
        )
        .map_err(|e| JsValue::from_str(e.0))?;
        if header.repository_id != repository_id
            || header.object_id != object_id
            || header.role != expected_role
        {
            return Err(JsValue::from_str("object-header-mismatch"));
        }
        validate_document(header.role, &plaintext)?;
        Ok(plaintext)
    }

    #[wasm_bindgen]
    pub fn canonical_sync_state_document(bytes: &[u8]) -> std::result::Result<Vec<u8>, JsValue> {
        SyncStateDocument::decode(bytes, MAX_METADATA_BYTES)
            .and_then(|document| document.encode(MAX_METADATA_BYTES))
            .map_err(|e| JsValue::from_str(e.0))
    }

    #[wasm_bindgen]
    pub fn canonical_backup_bundle_document(bytes: &[u8]) -> std::result::Result<Vec<u8>, JsValue> {
        BackupBundleDocument::decode(bytes, MAX_CONTROL_BYTES)
            .and_then(|document| document.encode(MAX_CONTROL_BYTES))
            .map_err(|e| JsValue::from_str(e.0))
    }

    #[wasm_bindgen]
    pub fn canonical_head_document(bytes: &[u8]) -> std::result::Result<Vec<u8>, JsValue> {
        HeadDocument::decode(bytes, MAX_CONTROL_BYTES)
            .and_then(|document| document.encode(MAX_CONTROL_BYTES))
            .map_err(|e| JsValue::from_str(e.0))
    }

    #[wasm_bindgen]
    pub fn canonical_backup_point_document(bytes: &[u8]) -> std::result::Result<Vec<u8>, JsValue> {
        BackupPointDocument::decode(bytes, MAX_CONTROL_BYTES)
            .and_then(|document| document.encode(MAX_CONTROL_BYTES))
            .map_err(|e| JsValue::from_str(e.0))
    }

    #[wasm_bindgen]
    pub fn keyed_object_id(
        key_bytes: &[u8],
        namespace: &str,
        role_name: &str,
        plaintext_sha256: &[u8],
    ) -> std::result::Result<String, JsValue> {
        let digest: [u8; 32] = plaintext_sha256
            .try_into()
            .map_err(|_| JsValue::from_str("invalid-hash"))?;
        format_keyed_object_id(&parse_key(key_bytes)?, namespace, role(role_name)?, &digest)
            .map_err(|e| JsValue::from_str(e.0))
    }
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
        let key = parse_key(key)?;
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

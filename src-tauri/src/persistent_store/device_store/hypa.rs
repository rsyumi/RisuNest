//! Batch access to the embedding cache. One renderer call covers a whole
//! embedding round, so a library of chunks costs one transaction instead of one
//! per chunk.
use super::{begin_mutation, finish_mutation, invalid, issue_write_clock, DeviceStore, Section};
use crate::persistent_store::StoreResult;
use rusqlite::{params, OptionalExtension};

/// Float32 little endian. The renderer frames vectors with the same width.
pub(crate) const VECTOR_ELEMENT_BYTES: usize = 4;
pub(crate) const MAX_DIMENSIONS: i64 = 16_384;
pub(crate) const MAX_BATCH_ENTRIES: usize = 8_192;

pub(crate) struct HypaEmbeddingRead {
    pub(crate) cache_key: String,
    pub(crate) dimensions: i64,
    pub(crate) vector: Option<Vec<u8>>,
}

pub(crate) struct HypaEmbeddingWrite {
    pub(crate) cache_key: String,
    pub(crate) producer: String,
    pub(crate) model: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) preprocess_version: i64,
    pub(crate) dimensions: i64,
    pub(crate) vector: Vec<u8>,
    pub(crate) metadata: Option<String>,
}

fn validate_key(cache_key: &str) -> StoreResult<()> {
    if cache_key.is_empty() || cache_key.len() > 256 {
        return Err(invalid("embedding cache key is invalid"));
    }
    Ok(())
}

fn validate_write(entry: &HypaEmbeddingWrite) -> StoreResult<()> {
    validate_key(&entry.cache_key)?;
    if entry.producer.is_empty() || entry.producer.len() > 64 {
        return Err(invalid("embedding producer is invalid"));
    }
    if entry.model.is_empty() || entry.model.len() > 256 {
        return Err(invalid("embedding model is invalid"));
    }
    if entry.preprocess_version < 0 {
        return Err(invalid("embedding preprocess version is invalid"));
    }
    if entry.dimensions <= 0 || entry.dimensions > MAX_DIMENSIONS {
        return Err(invalid("embedding dimensions are out of range"));
    }
    if entry.vector.len() != entry.dimensions as usize * VECTOR_ELEMENT_BYTES {
        return Err(invalid("embedding vector length does not match dimensions"));
    }
    if entry.metadata.is_some() {
        return Err(invalid("embedding metadata is unsupported"));
    }
    Ok(())
}

impl DeviceStore {
    /// Answers in request order. A missing or tombstoned key reports no vector,
    /// so the caller can recompute exactly the gaps.
    pub(crate) fn read_hypa_embeddings(
        &self,
        keys: &[String],
    ) -> StoreResult<Vec<HypaEmbeddingRead>> {
        if keys.len() > MAX_BATCH_ENTRIES {
            return Err(invalid("embedding read batch is too large"));
        }
        for key in keys {
            validate_key(key)?;
        }
        let mut statement = self.connection.prepare(
            "SELECT dimensions,vector FROM hypa_embeddings
                WHERE cache_key=?1 AND tombstone=0 AND vector IS NOT NULL",
        )?;
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let row = statement
                .query_row([key], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .optional()?;
            results.push(match row {
                Some((dimensions, vector)) => HypaEmbeddingRead {
                    cache_key: key.clone(),
                    dimensions,
                    vector: Some(vector),
                },
                None => HypaEmbeddingRead {
                    cache_key: key.clone(),
                    dimensions: 0,
                    vector: None,
                },
            });
        }
        Ok(results)
    }

    /// Commits the whole batch or nothing. Every row takes its own write clock
    /// so a later publication can resume at a record boundary.
    pub(crate) fn write_hypa_embeddings(
        &mut self,
        entries: &[HypaEmbeddingWrite],
    ) -> StoreResult<()> {
        if entries.len() > MAX_BATCH_ENTRIES {
            return Err(invalid("embedding write batch is too large"));
        }
        for entry in entries {
            validate_write(entry)?;
        }
        if entries.is_empty() {
            return Ok(());
        }
        let transaction = self.transaction()?;
        let writer_id: String = transaction.query_row(
            "SELECT writer_id FROM device_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        begin_mutation(&transaction)?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO hypa_embeddings
                    (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                     metadata,tombstone,write_clock,writer_id,published_clock,
                     first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,NULL,NULL,NULL)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        producer=excluded.producer,
                        model=excluded.model,
                        endpoint=excluded.endpoint,
                        preprocess_version=excluded.preprocess_version,
                        dimensions=excluded.dimensions,
                        vector=excluded.vector,
                        metadata=excluded.metadata,
                        tombstone=0,
                        write_clock=excluded.write_clock,
                        writer_id=excluded.writer_id,
                        published_clock=NULL,
                        first_published_generation=NULL,
                        first_published_at_ms=NULL",
            )?;
            for entry in entries {
                let clock = issue_write_clock(&transaction, Section::Hypa)?;
                statement.execute(params![
                    entry.cache_key,
                    entry.producer,
                    entry.model,
                    entry.endpoint,
                    entry.preprocess_version,
                    entry.dimensions,
                    entry.vector,
                    entry.metadata,
                    clock.as_str(),
                    writer_id,
                ])?;
            }
        }
        finish_mutation(&transaction)?;
        transaction.commit()?;
        Ok(())
    }
}

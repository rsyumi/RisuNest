use super::*;

pub(super) enum CompressionReader<R: BufRead> {
    Gzip(flate2::bufread::GzDecoder<R>),
    Zlib(flate2::bufread::ZlibDecoder<R>),
    Deflate(flate2::bufread::DeflateDecoder<R>),
}
impl<R: BufRead> Read for CompressionReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self { Self::Gzip(r) => r.read(buffer), Self::Zlib(r) => r.read(buffer), Self::Deflate(r) => r.read(buffer) }
    }
}
impl<R: BufRead> CompressionReader<R> {
    pub(super) fn get_ref(&self) -> &R {
        match self { Self::Gzip(r) => r.get_ref(), Self::Zlib(r) => r.get_ref(), Self::Deflate(r) => r.get_ref() }
    }
    fn get_mut(&mut self) -> &mut R {
        match self { Self::Gzip(r) => r.get_mut(), Self::Zlib(r) => r.get_mut(), Self::Deflate(r) => r.get_mut() }
    }
}
impl<S: TrackedSource> TrackedSource for CompressionReader<BufReader<S>> {
    fn report_counts(&mut self, update: impl FnOnce(&mut ImportCounts)) -> Result<(), NativeJobError> {
        self.get_mut().get_mut().report_counts(update)
    }
}

pub(super) fn json_database(
    input: &mut impl Read, staging_id: &str, job: &JobControl,
    sink: &dyn ReplacementSink, directory: Option<&Path>,
) -> Result<LegacyDatabase, NativeJobError> {
    let root = RootSpool::new_in(directory, job.cancellation_flag()).map_err(store_error)?;
    let mut decoder = serde_json::Deserializer::from_reader(input);
    let (character_count, excluded) = DatabaseSeed { root: &root, staging_id, job, sink }
        .deserialize(&mut decoder).map_err(|error| json_error("historical database", 1, error, job))?;
    decoder.end().map_err(|error| json_error("historical database", 1, error, job))?;
    root.finish_decode().map_err(store_error)?;
    Ok(LegacyDatabase::Root { root, character_count, excluded })
}

struct DatabaseSeed<'a> { root: &'a RootSpool, staging_id: &'a str, job: &'a JobControl, sink: &'a dyn ReplacementSink }
impl<'de> DeserializeSeed<'de> for DatabaseSeed<'_> {
    type Value = (Option<u64>, HashSet<String>);
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> { d.deserialize_map(self) }
}
impl<'de> Visitor<'de> for DatabaseSeed<'_> {
    type Value = (Option<u64>, HashSet<String>);
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("a historical database object") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut count = None;
        let mut excluded = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if self.job.is_cancel_requested() { return Err(de::Error::custom("restore cancelled")); }
            if key == "characters" {
                if count.is_some() { return Err(de::Error::custom("duplicate characters field")); }
                count = Some(map.next_value_seed(CharactersSeed { database: &self, excluded: &mut excluded })?);
            } else {
                map.next_value_seed(FieldSeed { root: self.root, key: &key })?;
            }
        }
        Ok((count, excluded))
    }
}
struct CharactersSeed<'a, 'b> { database: &'a DatabaseSeed<'b>, excluded: &'a mut HashSet<String> }
impl<'de> DeserializeSeed<'de> for CharactersSeed<'_, '_> {
    type Value = u64;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<u64, D::Error> { d.deserialize_seq(self) }
}
impl<'de> Visitor<'de> for CharactersSeed<'_, '_> {
    type Value = u64;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("a character array") }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<u64, A::Error> {
        let mut count = 0;
        let mut ids = HashSet::new();
        while let Some(mut value) = seq.next_element::<Value>()? {
            if self.database.job.is_cancel_requested() { return Err(de::Error::custom("restore cancelled")); }
            if upstream_import::exclude_record(&value, self.excluded) { self.database.job.record_restore_omission("excludedCharacters", 1).map_err(de::Error::custom)?; continue; }
            value = self.database.sink.prepare_character(value).map_err(de::Error::custom)?;
            if !self.database.sink.expands_cold_payloads() { omit_unavailable_cold(&mut value, self.database.job).map_err(de::Error::custom)?; }
            if upstream_import::exclude_record(&value, self.excluded) { self.database.job.record_restore_omission("excludedCharacters", 1).map_err(de::Error::custom)?; continue; }
            if !prepare_legacy_character(&mut value, &mut ids) { continue; }
            pocket_features::character(&mut value, &format!("character:{count}"), pocket_features::Origin::External);
            assign_legacy_chat_ids(std::slice::from_mut(&mut value));
            self.database.sink.add_characters(self.database.staging_id, &[value]).map_err(de::Error::custom)?;
            count += 1;
        }
        Ok(count)
    }
}

struct FieldSeed<'a> { root: &'a RootSpool, key: &'a str }
impl<'de> DeserializeSeed<'de> for FieldSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> { d.deserialize_any(self) }
}
impl FieldSeed<'_> {
    fn scalar<E: de::Error>(self, value: Value) -> Result<(), E> { self.root.put(self.key, Some(&value)).map_err(E::custom) }
}
impl<'de> Visitor<'de> for FieldSeed<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("a saved root field") }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> { self.scalar(Value::Null) }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<(), E> { self.scalar(v.into()) }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<(), E> { self.scalar(v.into()) }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<(), E> { self.scalar(v.into()) }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<(), E> { self.scalar(v.into()) }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<(), E> { self.scalar(v.into()) }
    fn visit_string<E: de::Error>(self, v: String) -> Result<(), E> { self.scalar(v.into()) }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        self.root.begin(self.key, FieldKind::Array).map_err(de::Error::custom)?;
        let mut index = 0u64;
        while let Some(value) = seq.next_element::<Value>()? {
            self.root.put_member(self.key, &index.to_string(), Some(&value)).map_err(de::Error::custom)?;
            index += 1;
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        self.root.begin(self.key, FieldKind::Object).map_err(de::Error::custom)?;
        while let Some((key, value)) = map.next_entry::<String, Value>()? {
            self.root.put_member(self.key, &key, Some(&value)).map_err(de::Error::custom)?;
        }
        Ok(())
    }
}

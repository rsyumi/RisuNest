use super::{descriptors_index::PendingIndex, json, objects::ObjectPaths, parse, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical,
    descriptor::{RecordDescriptor, ReferencePage, MAX_DESCRIPTOR_REFERENCES, MAX_TREE_DEPTH},
    MAX_METADATA_BYTES,
};
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;

impl Store {
    /// Normalize immutable control pages before the transaction writer is acquired.
    /// Each page is bounded, published once, and reused by subsequent
    /// descriptors. Validation reads the objects it is given; the index rows it
    /// implies are written at bounded boundaries, not one statement per record
    /// and one transaction per reference node.
    pub(super) fn prepare_descriptors(
        &self,
        changes: &[risunest_sync_wire::lww::UnitChange],
    ) -> Result<()> {
        #[cfg(all(test, feature = "source-observer"))]
        let _observed = crate::source_observer::ingress(&self.root);
        let paths = self.object_paths();
        let mut pending = PendingIndex::default();
        for change in changes {
            if let risunest_sync_wire::unit::UnitValue::Object {
                descriptor_hash,
                descriptor,
            } = &change.value
            {
                let stored = self
                    .prepare_descriptor(&paths, descriptor_hash, &mut pending)
                    .map_err(|error| error.for_key(change.key.as_str()))?;
                if stored != *descriptor {
                    return Err(Error::new("descriptor-object-mismatch", 409));
                }
                if pending.is_full() {
                    pending.flush(&mut *self.db()?)?;
                }
            }
        }
        pending.flush(&mut *self.db()?)
    }
    fn prepare_descriptor(
        &self,
        paths: &ObjectPaths,
        digest: &str,
        pending: &mut PendingIndex,
    ) -> Result<RecordDescriptor> {
        if let Some(body) = pending.descriptor(digest) {
            return parse(body);
        }
        let cached = {
            let db = self.reader()?;
            db.query_row(
                "SELECT body FROM descriptors WHERE hash=?1",
                [digest],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        };
        if let Some(body) = cached {
            let descriptor: RecordDescriptor = parse(&body)?;
            descriptor.validate()?;
            if descriptor.hash()? != digest {
                return Err(Error::new("corrupt-metadata", 503));
            }
            for hash in std::iter::once(digest)
                .chain(std::iter::once(descriptor.object_hash.as_str()))
                .chain(descriptor.dependencies.iter().map(String::as_str))
            {
                if !self.object_presence_in(paths, hash)? {
                    return Err(Error::new("missing-dependency", 409));
                }
            }
            for root in [&descriptor.dependency_root, &descriptor.relation_root]
                .into_iter()
                .flatten()
            {
                self.validate_cached_reference(paths, root)?;
            }
            pending.remember_descriptor(digest.into(), body);
            return Ok(descriptor);
        }
        let descriptor: RecordDescriptor = canonical::decode(
            &self.get_object(digest).map_err(|error| {
                if error.code == "object-not-found" {
                    Error::new("missing-dependency", 409)
                } else {
                    error
                }
            })?,
            MAX_METADATA_BYTES,
        )?;
        descriptor.validate()?;
        if !self.object_presence_in(paths, &descriptor.object_hash)? {
            return Err(Error::new("missing-dependency", 409));
        }
        for hash in &descriptor.dependencies {
            if !self.object_presence_in(paths, hash)? {
                return Err(Error::new("missing-dependency", 409));
            }
        }
        if let Some(root) = &descriptor.dependency_root {
            self.prepare_reference_tree(paths, root, false, pending)?;
        }
        if let Some(root) = &descriptor.relation_root {
            self.prepare_reference_tree(paths, root, true, pending)?;
        }
        pending.add_descriptor(
            digest.to_owned(),
            descriptor.object_hash.clone(),
            json(&descriptor)?,
        );
        Ok(descriptor)
    }
    fn validate_cached_reference(&self, paths: &ObjectPaths, root: &str) -> Result<()> {
        let hashes: Vec<String> = {
            let db = self.reader()?;
            let mut stmt = db.prepare("WITH RECURSIVE nodes(hash) AS (SELECT ?1 UNION SELECT child FROM reference_children JOIN nodes ON root=nodes.hash) SELECT hash FROM nodes UNION SELECT object FROM reference_objects WHERE root IN nodes")?;
            let rows = stmt.query_map([root], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for hash in hashes {
            if !self.object_presence_in(paths, &hash)? {
                return Err(Error::new("missing-dependency", 409));
            }
        }
        Ok(())
    }
    fn prepare_reference_tree(
        &self,
        paths: &ObjectPaths,
        root: &str,
        relations: bool,
        pending: &mut PendingIndex,
    ) -> Result<()> {
        let kind = if relations { "relations" } else { "objects" };
        let mut queue = vec![(root.to_owned(), 0usize, false)];
        let mut seen = BTreeSet::new();
        while let Some((digest, depth, expanded)) = queue.pop() {
            let existing = match pending.node(&digest) {
                Some((stats, _, _)) => Some(stats.kind.to_owned()),
                None => self
                    .reader()?
                    .query_row(
                        "SELECT kind FROM reference_nodes WHERE hash=?1",
                        [&digest],
                        |r| r.get(0),
                    )
                    .optional()?,
            };
            if let Some(existing) = existing {
                if existing != kind {
                    return Err(Error::new("reference-kind-mismatch", 409));
                }
                if pending.node(&digest).is_none() {
                    self.validate_cached_reference(paths, &digest)?;
                }
                continue;
            }
            if depth >= MAX_TREE_DEPTH
                || (!expanded
                    && (!seen.insert(digest.clone()) || seen.len() > MAX_DESCRIPTOR_REFERENCES))
            {
                return Err(Error::new("invalid-reference-tree", 400));
            }
            let page: ReferencePage =
                canonical::decode(&self.get_object(&digest)?, MAX_METADATA_BYTES)?;
            page.validate()?;
            if let ReferencePage::Branches { children } = &page {
                if !expanded {
                    queue.push((digest, depth, true));
                    for child in children.iter().rev() {
                        queue.push((child.clone(), depth + 1, false));
                    }
                    continue;
                }
            } else if matches!(page, ReferencePage::Relations { .. }) != relations {
                return Err(Error::new("reference-kind-mismatch", 409));
            }
            let (count, tree_depth, first, last) = if let ReferencePage::Branches { children } =
                &page
            {
                let mut count = 0i64;
                let mut deepest = 0i64;
                let mut first = None;
                let mut last: Option<String> = None;
                for child in children {
                    // A child prepared earlier in this page is not written yet,
                    // so it is read from the pending rows exactly as a written
                    // one would be read from the table.
                    let (child_count, child_depth, child_first, child_last): (
                        i64,
                        i64,
                        String,
                        String,
                    ) = match pending.node(child).filter(|(stats, _, _)| stats.kind == kind) {
                        Some((stats, first, last)) => {
                            (stats.count, stats.depth, first.to_owned(), last.to_owned())
                        }
                        None => self.reader()?.query_row("SELECT item_count,tree_depth,first_value,last_value FROM reference_nodes WHERE hash=?1 AND kind=?2",params![child,kind],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?,
                    };
                    if last
                        .as_ref()
                        .is_some_and(|previous| previous >= &child_first)
                    {
                        return Err(Error::new("unordered-references", 400));
                    }
                    first.get_or_insert(child_first);
                    last = Some(child_last);
                    count += child_count;
                    deepest = deepest.max(child_depth);
                }
                (count, deepest + 1, first.unwrap(), last.unwrap())
            } else {
                (
                    page.values().len() as i64,
                    1,
                    page.values()[0].clone(),
                    page.values().last().unwrap().clone(),
                )
            };
            if count > MAX_DESCRIPTOR_REFERENCES as i64 || tree_depth > MAX_TREE_DEPTH as i64 {
                return Err(Error::new("reference-tree-too-large", 413));
            }
            pending.add_node(digest.clone(), kind, count, tree_depth, first, last);
            let table = match &page {
                ReferencePage::Objects { .. } => "reference_objects",
                ReferencePage::Relations { .. } => "reference_relations",
                ReferencePage::Branches { .. } => "reference_children",
            };
            for value in page.values() {
                if matches!(page, ReferencePage::Objects { .. })
                    && !self.object_presence_in(paths, value)?
                {
                    return Err(Error::new("missing-dependency", 409));
                }
                pending.add_edge(table, digest.clone(), value.clone());
            }
        }
        Ok(())
    }
}

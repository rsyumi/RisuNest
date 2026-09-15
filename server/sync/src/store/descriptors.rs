use super::{json, parse, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical,
    descriptor::{RecordDescriptor, ReferencePage, MAX_DESCRIPTOR_REFERENCES, MAX_TREE_DEPTH},
    ChangeSet, Domain, RecordVersion, MAX_METADATA_BYTES,
};
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;

impl Store {
    /// Normalize immutable control pages before the commit writer is acquired.
    /// Each page is bounded, published once, and reused by subsequent descriptors.
    pub(super) fn prepare_descriptors(&self, changes: &ChangeSet) -> Result<()> {
        for change in &changes.changes {
            if let RecordVersion::Live {
                object_hash,
                descriptor_hash: Some(digest),
            } = &change.after
            {
                let descriptor = self.prepare_descriptor(digest)?;
                if &descriptor.object_hash != object_hash {
                    return Err(Error::new("descriptor-object-mismatch", 409));
                }
            }
        }
        Ok(())
    }
    fn prepare_descriptor(&self, digest: &str) -> Result<RecordDescriptor> {
        if let Some(body) = self
            .db()?
            .query_row(
                "SELECT body FROM descriptors WHERE hash=?1",
                [digest],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return parse(&body);
        }
        let descriptor: RecordDescriptor =
            canonical::decode(&self.get_object(digest)?, MAX_METADATA_BYTES)?;
        descriptor.validate()?;
        if self.object_size(&descriptor.object_hash)?.is_none() {
            return Err(Error::new("missing-dependency", 409));
        }
        if let Some(root) = &descriptor.dependency_root {
            self.prepare_reference_tree(root, false)?;
        }
        if let Some(root) = &descriptor.relation_root {
            self.prepare_reference_tree(root, true)?;
        }
        let db = self.db()?;
        db.execute("INSERT INTO descriptors(hash,object,body) VALUES(?1,?2,?3) ON CONFLICT(hash) DO NOTHING",params![digest,descriptor.object_hash,json(&descriptor)?])?;
        Ok(descriptor)
    }
    fn prepare_reference_tree(&self, root: &str, relations: bool) -> Result<()> {
        let kind = if relations { "relations" } else { "objects" };
        let mut pending = vec![(root.to_owned(), 0usize, false)];
        let mut seen = BTreeSet::new();
        while let Some((digest, depth, expanded)) = pending.pop() {
            let existing: Option<String> = self
                .db()?
                .query_row(
                    "SELECT kind FROM reference_nodes WHERE hash=?1",
                    [&digest],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing != kind {
                    return Err(Error::new("reference-kind-mismatch", 409));
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
                    pending.push((digest, depth, true));
                    for child in children.iter().rev() {
                        pending.push((child.clone(), depth + 1, false));
                    }
                    continue;
                }
            } else if matches!(page, ReferencePage::Relations { .. }) != relations {
                return Err(Error::new("reference-kind-mismatch", 409));
            }
            let mut db = self.db()?;
            let tx = db.transaction()?;
            let (count, tree_depth, first, last) = if let ReferencePage::Branches { children } =
                &page
            {
                let mut count = 0i64;
                let mut deepest = 0i64;
                let mut first = None;
                let mut last: Option<String> = None;
                for child in children {
                    let (child_count,child_depth,child_first,child_last):(i64,i64,String,String)=tx.query_row("SELECT item_count,tree_depth,first_value,last_value FROM reference_nodes WHERE hash=?1 AND kind=?2",params![child,kind],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
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
            tx.execute("INSERT INTO reference_nodes VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(hash) DO NOTHING",params![digest,kind,count,tree_depth,first,last])?;
            let table = match &page {
                ReferencePage::Objects { .. } => "reference_objects",
                ReferencePage::Relations { .. } => "reference_relations",
                ReferencePage::Branches { .. } => "reference_children",
            };
            let sql = format!("INSERT INTO {table} VALUES(?1,?2) ON CONFLICT DO NOTHING");
            for value in page.values() {
                if matches!(page, ReferencePage::Objects { .. }) {
                    let present: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1)",
                        [value],
                        |r| r.get(0),
                    )?;
                    if !present {
                        return Err(Error::new("missing-dependency", 409));
                    }
                }
                tx.execute(&sql, params![digest, value])?;
            }
            tx.commit()?;
        }
        Ok(())
    }
    pub(super) fn update_relations(
        db: &rusqlite::Connection,
        domain: Domain,
        key: &str,
        version: &RecordVersion,
    ) -> Result<()> {
        db.execute(
            "DELETE FROM record_relations WHERE domain=?1 AND source=?2",
            params![domain.as_str(), key],
        )?;
        if let RecordVersion::Live {
            descriptor_hash: Some(digest),
            ..
        } = version
        {
            let body: String = db.query_row(
                "SELECT body FROM descriptors WHERE hash=?1",
                [digest],
                |r| r.get(0),
            )?;
            let descriptor: RecordDescriptor = parse(&body)?;
            if let Some(root) = descriptor.relation_root {
                db.execute("WITH RECURSIVE nodes(hash) AS (SELECT ?1 UNION SELECT child FROM reference_children JOIN nodes ON root=nodes.hash) INSERT INTO record_relations(domain,source,target) SELECT ?2,?3,target FROM reference_relations WHERE root IN nodes ON CONFLICT DO NOTHING",params![root,domain.as_str(),key])?;
            }
        }
        Ok(())
    }
    pub(super) fn validate_relations(db: &rusqlite::Connection, stage: &str) -> Result<()> {
        Self::each_change(db, stage, |change| {
            if !matches!(change.after, RecordVersion::Live { .. }) {
                let used: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM record_relations WHERE domain=?1 AND target=?2)",
                    params![change.domain.as_str(), change.key],
                    |r| r.get(0),
                )?;
                if used {
                    return Err(Error::new("referenced-record-deleted", 409));
                }
            }
            let mut statement =
                db.prepare("SELECT target FROM record_relations WHERE domain=?1 AND source=?2")?;
            let mut targets = statement.query(params![change.domain.as_str(), change.key])?;
            while let Some(row) = targets.next()? {
                let key: String = row.get(0)?;
                if !matches!(
                    Self::read_version(db, change.domain, &key)?,
                    RecordVersion::Live { .. }
                ) {
                    return Err(Error::new("missing-related-record", 409));
                }
            }
            Ok(())
        })
    }
}

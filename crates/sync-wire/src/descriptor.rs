//! Immutable, bounded control objects for arbitrarily large record dependency and
//! relationship sets. Application content remains opaque and is never rewritten.
use crate::{canonical, hash, validate_hash, Result, WireError, MAX_KEY_BYTES, MAX_METADATA_BYTES};
use serde::{Deserialize, Serialize};

pub const PAGE_FANOUT: usize = 1024;
pub const MAX_TREE_DEPTH: usize = 8;
pub const MAX_DESCRIPTOR_REFERENCES: usize = 1_000_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordDescriptor {
    pub object_hash: String,
    pub dependency_root: Option<String>,
    pub relation_root: Option<String>,
    /// Generic keyspace membership, for example the client's shared plugin store.
    pub scopes: Vec<String>,
}
impl RecordDescriptor {
    pub fn content(object_hash: String) -> Self {
        Self {
            object_hash,
            dependency_root: None,
            relation_root: None,
            scopes: Vec::new(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        validate_hash(&self.object_hash)?;
        for root in [&self.dependency_root, &self.relation_root]
            .into_iter()
            .flatten()
        {
            validate_hash(root)?;
        }
        if self.scopes.len() > 16 || self.scopes.windows(2).any(|w| w[0] >= w[1]) {
            return Err(WireError("invalid-scopes"));
        }
        for scope in &self.scopes {
            validate_scope(scope)?;
        }
        Ok(())
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        canonical::encode(self)
    }
    pub fn hash(&self) -> Result<String> {
        Ok(hash(&self.bytes()?))
    }
}
pub fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > 1024 {
        return Err(WireError("invalid-scope"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ReferencePage {
    Objects { hashes: Vec<String> },
    Relations { keys: Vec<String> },
    Branches { children: Vec<String> },
}
impl ReferencePage {
    pub fn values(&self) -> &[String] {
        match self {
            Self::Objects { hashes } => hashes,
            Self::Relations { keys } => keys,
            Self::Branches { children } => children,
        }
    }
    pub fn validate(&self) -> Result<()> {
        let values = self.values();
        if values.is_empty() || values.len() > PAGE_FANOUT {
            return Err(WireError("invalid-reference-page"));
        }
        if !matches!(self, Self::Branches { .. }) && values.windows(2).any(|w| w[0] >= w[1]) {
            return Err(WireError("unordered-references"));
        }
        for value in values {
            match self {
                Self::Relations { .. } if value.is_empty() || value.len() > MAX_KEY_BYTES => {
                    return Err(WireError("invalid-key"))
                }
                Self::Relations { .. } => (),
                _ => validate_hash(value)?,
            }
        }
        if canonical::encode(self)?.len() > MAX_METADATA_BYTES {
            return Err(WireError("metadata-too-large"));
        }
        Ok(())
    }
}

pub type BuiltReferenceTree = (Option<String>, Vec<(String, Vec<u8>)>);

/// Produce bounded CAS control pages. Returns their exact bytes and the root.
/// This is a client-side builder, never a server-side application transform.
pub fn build_reference_tree(values: &[String], relations: bool) -> Result<BuiltReferenceTree> {
    if values.len() > MAX_DESCRIPTOR_REFERENCES || values.windows(2).any(|w| w[0] >= w[1]) {
        return Err(WireError("invalid-reference-list"));
    }
    let mut objects = Vec::new();
    let mut leaves = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for value in values {
        let bytes = canonical::encode(value)?.len() + 1;
        if !current.is_empty()
            && (current.len() == PAGE_FANOUT
                || used + bytes > MAX_METADATA_BYTES - 128
                || (current.len() >= 64 && reference_boundary(value, relations)))
        {
            leaves.push(publish_leaf(
                std::mem::take(&mut current),
                relations,
                &mut objects,
            )?);
            used = 0;
        }
        current.push(value.clone());
        used += bytes;
    }
    if !current.is_empty() {
        leaves.push(publish_leaf(current, relations, &mut objects)?);
    }
    let mut depth = 1;
    while leaves.len() > 1 {
        if depth >= MAX_TREE_DEPTH {
            return Err(WireError("reference-tree-too-deep"));
        }
        let mut next = Vec::new();
        let mut groups = Vec::new();
        let mut children = Vec::new();
        for child in leaves {
            if !children.is_empty()
                && (children.len() == PAGE_FANOUT
                    || (children.len() >= 64 && reference_boundary(&child, false)))
            {
                groups.push(std::mem::take(&mut children));
            }
            children.push(child);
        }
        if !children.is_empty() {
            groups.push(children);
        }
        for children in groups {
            let page = ReferencePage::Branches { children };
            page.validate()?;
            let bytes = canonical::encode(&page)?;
            let digest = hash(&bytes);
            next.push(digest.clone());
            objects.push((digest, bytes));
        }
        leaves = next;
        depth += 1;
    }
    Ok((leaves.pop(), objects))
}
// Stable boundaries prevent inserting one reference from shifting every later
// page. Ordering is still semantic key/hash order, never branch-hash order.
fn reference_boundary(value: &str, relations: bool) -> bool {
    let digest = if relations {
        hash(value.as_bytes())
    } else {
        value.to_owned()
    };
    digest
        // Hash inventories are lexically sorted. Prefix bytes cluster at the
        // beginning of a range, turning almost every later page into a fixed
        // count page whose boundary shifts on insertion. Suffix bytes retain
        // content-defined boundaries throughout the sorted inventory.
        .get(62..64)
        .and_then(|suffix| u8::from_str_radix(suffix, 16).ok())
        .is_some_and(|byte| byte & 127 == 0)
}
fn publish_leaf(
    values: Vec<String>,
    relations: bool,
    objects: &mut Vec<(String, Vec<u8>)>,
) -> Result<String> {
    let page = if relations {
        ReferencePage::Relations { keys: values }
    } else {
        ReferencePage::Objects { hashes: values }
    };
    page.validate()?;
    let bytes = canonical::encode(&page)?;
    let digest = hash(&bytes);
    objects.push((digest.clone(), bytes));
    Ok(digest)
}

/// Walk immutable references with bounded depth/count and verify every page hash.
/// The callback can register edges transactionally without materializing a library.
pub fn visit_reference_tree(
    root: &str,
    relations: bool,
    mut load: impl FnMut(&str) -> Result<Vec<u8>>,
    mut visit: impl FnMut(&str, bool) -> Result<()>,
) -> Result<()> {
    validate_hash(root)?;
    let mut pending = vec![(root.to_owned(), 0usize)];
    let mut count = 0usize;
    let mut prior: Option<String> = None;
    let mut pages = std::collections::BTreeSet::new();
    while let Some((digest, depth)) = pending.pop() {
        if depth >= MAX_TREE_DEPTH
            || !pages.insert(digest.clone())
            || pages.len() > MAX_DESCRIPTOR_REFERENCES
        {
            return Err(WireError("invalid-reference-tree"));
        }
        let bytes = load(&digest)?;
        if hash(&bytes) != digest {
            return Err(WireError("hash-mismatch"));
        }
        let page: ReferencePage = canonical::decode(&bytes, MAX_METADATA_BYTES)?;
        page.validate()?;
        visit(&digest, true)?;
        match page {
            ReferencePage::Branches { children } => {
                for child in children.into_iter().rev() {
                    pending.push((child, depth + 1));
                }
            }
            leaf => {
                if matches!(leaf, ReferencePage::Relations { .. }) != relations {
                    return Err(WireError("reference-kind-mismatch"));
                }
                for value in leaf.values() {
                    if prior.as_ref().is_some_and(|p| p >= value) {
                        return Err(WireError("unordered-references"));
                    }
                    count += 1;
                    if count > MAX_DESCRIPTOR_REFERENCES {
                        return Err(WireError("too-many-references"));
                    }
                    visit(value, false)?;
                    prior = Some(value.clone());
                }
            }
        }
    }
    Ok(())
}

use super::*;
use risunest_sync_wire::descriptor::{ReferencePage, MAX_DESCRIPTOR_REFERENCES, MAX_TREE_DEPTH};
use std::collections::BTreeMap;

impl Transfer<'_> {
    pub(crate) fn record_reference_hints(
        &self,
        version: &risunest_sync_wire::RecordVersion,
        previous: &risunest_sync_wire::RecordVersion,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let roots = |version: &risunest_sync_wire::RecordVersion| -> Result<_> {
            let risunest_sync_wire::RecordVersion::Live {
                descriptor_hash: Some(hash),
                ..
            } = version
            else {
                return Ok((None, None));
            };
            let descriptor: risunest_sync_wire::descriptor::RecordDescriptor = canonical::decode(
                &self.cache.read(hash, MAX_METADATA_BYTES)?,
                MAX_METADATA_BYTES,
            )?;
            descriptor.validate()?;
            Ok((descriptor.dependency_root, descriptor.relation_root))
        };
        let current = roots(version)?;
        let previous = roots(previous).unwrap_or((None, None));
        self.reference_tree_hints(
            [(current.0, previous.0), (current.1, previous.1)]
                .into_iter()
                .filter_map(|(root, base)| root.map(|root| (root, base.into_iter().collect())))
                .collect(),
        )
    }

    /// Uploads already have both trees locally. Corresponding pages are better
    /// bases than unrelated pages of the same size in a large hash inventory.
    pub(crate) fn reference_tree_hints(
        &self,
        roots: Vec<(String, Vec<String>)>,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut pending = roots
            .into_iter()
            .map(|(hash, bases)| (hash, bases, 0usize))
            .collect::<Vec<_>>();
        let mut hints = BTreeMap::new();
        while let Some((hash, bases, depth)) = pending.pop() {
            self.client.ensure_active()?;
            if depth >= MAX_TREE_DEPTH || hints.len() >= MAX_DESCRIPTOR_REFERENCES {
                return Err(SyncError::new("invalid-descriptor-tree", 409));
            }
            if bases.contains(&hash) || hints.contains_key(&hash) {
                continue;
            }
            let page: ReferencePage = canonical::decode(
                &self.cache.read(&hash, MAX_METADATA_BYTES)?,
                MAX_METADATA_BYTES,
            )?;
            page.validate()?;
            if let ReferencePage::Branches { children } = page {
                let old_children = self.previous_reference_children(&bases);
                for (index, child) in children.iter().enumerate() {
                    pending.push((
                        child.clone(),
                        adjacent_bases(&children, index, &old_children),
                        depth + 1,
                    ));
                }
            }
            hints.insert(hash, bases);
        }
        Ok(hints)
    }

    fn previous_reference_children(&self, bases: &[String]) -> Vec<String> {
        let mut children = Vec::new();
        for base in bases {
            let old = self
                .cache
                .read(base, MAX_METADATA_BYTES)
                .ok()
                .and_then(|bytes| {
                    canonical::decode::<ReferencePage>(&bytes, MAX_METADATA_BYTES).ok()
                })
                .filter(|p| p.validate().is_ok());
            match old {
                Some(ReferencePage::Branches { children: old }) => children.extend(old),
                Some(_) => children.push(base.clone()),
                None => (),
            }
        }
        children
    }
    /// Follow the previous tree's ordered children locally. Only the few bases
    /// adjacent to a changed child cross the wire, never the entire inventory.
    pub(crate) fn download_reference_tree(
        &self,
        roots: Vec<(String, Vec<String>)>,
    ) -> Result<Vec<String>> {
        let mut pending = roots
            .into_iter()
            .map(|(hash, bases)| (hash, bases, 0usize))
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let mut dependencies = BTreeSet::new();
        while !pending.is_empty() {
            let current = std::mem::take(&mut pending);
            let hints: BTreeMap<_, _> = current
                .iter()
                .map(|(hash, bases, _)| (hash.clone(), bases.clone()))
                .collect();
            self.download_with_hints(
                &current
                    .iter()
                    .map(|(h, _, _)| h.clone())
                    .collect::<Vec<_>>(),
                &[],
                &hints,
            )?;
            for (digest, bases, depth) in current {
                if !seen.insert(digest.clone())
                    || depth >= MAX_TREE_DEPTH
                    || seen.len() > MAX_DESCRIPTOR_REFERENCES
                {
                    return Err(SyncError::new("invalid-descriptor-tree", 409));
                }
                let page: ReferencePage = canonical::decode(
                    &self.cache.read(&digest, MAX_METADATA_BYTES)?,
                    MAX_METADATA_BYTES,
                )?;
                page.validate()?;
                match page {
                    ReferencePage::Branches { children } => {
                        let old_children = self.previous_reference_children(&bases);
                        for (index, child) in children.iter().enumerate() {
                            pending.push((
                                child.clone(),
                                adjacent_bases(&children, index, &old_children),
                                depth + 1,
                            ));
                        }
                    }
                    ReferencePage::Objects { hashes } => dependencies.extend(hashes),
                    ReferencePage::Relations { .. } => (),
                }
                if dependencies.len() > MAX_DESCRIPTOR_REFERENCES {
                    return Err(SyncError::new("invalid-descriptor-tree", 409));
                }
            }
        }
        Ok(dependencies.into_iter().collect())
    }
}

fn adjacent_bases(current: &[String], index: usize, previous: &[String]) -> Vec<String> {
    if previous.contains(&current[index]) {
        return vec![current[index].clone()];
    }
    let left_neighbor = current[..index]
        .iter()
        .rev()
        .find_map(|h| previous.iter().position(|old| old == h));
    let right = current[index + 1..]
        .iter()
        .find_map(|h| previous.iter().position(|old| old == h))
        .unwrap_or(previous.len());
    let mut left = left_neighbor.map_or(0, |i| i + 1);
    // A branch split may begin with a changed child whose only unchanged
    // neighbor is on the right, far into the previous parents' combined list.
    if left_neighbor.is_none() && right < previous.len() {
        left = right.saturating_sub(delta::MAX_BASES);
    }
    if left >= right {
        return Vec::new();
    }
    previous[left..right]
        .iter()
        .take(delta::MAX_BASES)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn values(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn unchanged_neighbors_bound_replacements_and_splits() {
        let old = values(&["a", "b", "c", "d"]);
        assert_eq!(
            adjacent_bases(&values(&["a", "x", "c", "d"]), 1, &old),
            values(&["b"])
        );
        let split = values(&["a", "x", "y", "c", "d"]);
        assert_eq!(adjacent_bases(&split, 1, &old), values(&["b"]));
        assert_eq!(adjacent_bases(&split, 2, &old), values(&["b"]));
        assert!(adjacent_bases(&values(&["a", "b", "x", "c", "d"]), 2, &old).is_empty());
        assert!(adjacent_bases(&values(&["x"]), 0, &[]).is_empty());
        assert_eq!(
            adjacent_bases(&values(&["x"]), 0, &values(&["a", "b", "c", "d", "e"])).len(),
            delta::MAX_BASES
        );
    }

    #[test]
    fn split_parent_first_child_uses_bases_nearest_its_right_neighbor() {
        let previous: Vec<_> = (0..200).map(|i| format!("old-{i}")).collect();
        let current = values(&["changed", "old-119", "old-120"]);
        assert_eq!(adjacent_bases(&current, 0, &previous), previous[115..119]);
        let current = values(&["old-118", "changed"]);
        assert_eq!(adjacent_bases(&current, 1, &previous), previous[119..123]);
        assert!(adjacent_bases(&values(&["new", "old-0"]), 0, &previous).is_empty());
    }
}

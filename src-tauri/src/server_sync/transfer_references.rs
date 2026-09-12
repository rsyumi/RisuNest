use super::*;
use risunest_sync_wire::descriptor::{ReferencePage, MAX_DESCRIPTOR_REFERENCES, MAX_TREE_DEPTH};
use std::collections::BTreeMap;

impl Transfer<'_> {
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
                        let mut old_children = Vec::new();
                        for base in &bases {
                            let old = self
                                .cache
                                .read(base, MAX_METADATA_BYTES)
                                .ok()
                                .and_then(|bytes| {
                                    canonical::decode::<ReferencePage>(&bytes, MAX_METADATA_BYTES)
                                        .ok()
                                })
                                .filter(|p| p.validate().is_ok());
                            match old {
                                Some(ReferencePage::Branches { children }) => {
                                    old_children.extend(children)
                                }
                                Some(_) => old_children.push(base.clone()),
                                None => (),
                            }
                        }
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
    let left = current[..index]
        .iter()
        .rev()
        .find_map(|h| previous.iter().position(|old| old == h))
        .map_or(0, |i| i + 1);
    let right = current[index + 1..]
        .iter()
        .find_map(|h| previous.iter().position(|old| old == h))
        .unwrap_or(previous.len());
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
}

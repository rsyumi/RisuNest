use crate::{canonical, Domain, ReadFence, RecordChange, Result, ScopeFence, WireError};
use sha2::{Digest, Sha256};

/// Same restricted-JCS digest as a flat ChangeSet, independent of page boundaries.
pub struct ChangeDigest {
    hash: Sha256,
    phase: u8,
    first: bool,
    prior: Option<(Option<Domain>, String)>,
}
impl Default for ChangeDigest {
    fn default() -> Self {
        Self::new()
    }
}
impl ChangeDigest {
    pub fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(b"{\"changes\":[");
        Self {
            hash,
            phase: 0,
            first: true,
            prior: None,
        }
    }
    fn advance(&mut self, phase: u8) -> Result<()> {
        if self.phase > phase {
            return Err(WireError("unordered-digest-sections"));
        }
        while self.phase < phase {
            self.hash.update(if self.phase == 0 {
                b"],\"readFences\":[".as_slice()
            } else {
                b"],\"scopeFences\":[".as_slice()
            });
            self.phase += 1;
            self.first = true;
            self.prior = None;
        }
        Ok(())
    }
    fn add(&mut self, order: (Option<Domain>, &str), value: &impl serde::Serialize) -> Result<()> {
        if self
            .prior
            .as_ref()
            .is_some_and(|p| (p.0, p.1.as_str()) >= order)
        {
            return Err(WireError("unordered-keys"));
        }
        if !self.first {
            self.hash.update(b",");
        }
        self.first = false;
        self.prior = Some((order.0, order.1.into()));
        self.hash.update(canonical::encode(value)?);
        Ok(())
    }
    pub fn change(&mut self, value: &RecordChange) -> Result<()> {
        self.advance(0)?;
        self.add((Some(value.domain), &value.key), value)
    }
    pub fn read_fence(&mut self, value: &ReadFence) -> Result<()> {
        self.advance(1)?;
        self.add((Some(value.domain), &value.key), value)
    }
    pub fn scope_fence(&mut self, value: &ScopeFence) -> Result<()> {
        self.advance(2)?;
        self.add((None, &value.scope), value)
    }
    pub fn finish(mut self) -> Result<String> {
        self.advance(2)?;
        self.hash.update(b"]}");
        Ok(format!("{:x}", self.hash.finalize()))
    }
}

//! Not integrated yet. This module never reports a successful connection.
use super::Dependencies;
use crate::external_storage::contract::{ErrorKind, Provider, ProviderError, Result};
use std::sync::Arc;

pub(crate) fn create(_dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Err(ProviderError::new(ErrorKind::Unsupported))
}

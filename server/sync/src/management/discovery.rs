//! The locator is a credential, not a public status file.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::Path,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub address: SocketAddr,
    pub token: String,
}

impl Discovery {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("management-session");
        for part in path.ancestors() {
            let meta = std::fs::symlink_metadata(part)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err(Error::new("unsafe-management-path", 409));
                }
            }
            if meta.file_type().is_symlink() {
                return Err(Error::new("unsafe-management-path", 409));
            }
        }
        let meta = std::fs::metadata(&path)?;
        if !meta.is_file() || meta.len() > 8192 {
            return Err(Error::new("invalid-management-session", 409));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(Error::new("private-management-session-required", 409));
            }
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(8193)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            return Err(Error::new("invalid-management-session", 409));
        }
        let bytes = crate::store::management::protect(&bytes, false)?;
        let value: Self = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("invalid-management-session", 409))?;
        if value.address.ip() != Ipv4Addr::LOCALHOST
            || value.address.port() == 0
            || value.token.len() != 64
            || !value.token.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::new("invalid-management-session", 409));
        }
        Ok(value)
    }
    pub(crate) fn save(&self, root: &Path) -> Result<()> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| Error::new("invalid-management-session", 409))?;
        let bytes = crate::store::management::protect(&bytes, true)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(root.join("management-session"))
            .map_err(|_| Error::new("management-session-write-failed", 503))?;
        Ok(())
    }
}

pub fn request_id() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::new("entropy-unavailable", 503))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

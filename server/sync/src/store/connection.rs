use super::{
    objects::{check_path, publish},
    Store,
};
use crate::{
    connection::{
        ConnectionOptions, ConnectionState, ConnectionStatus, PendingPublication, Publication,
    },
    Error, Result,
};
use risunest_sync_connect::{generate_directory, seal_endpoint, validate_endpoint, Registration};
use std::{
    fs,
    io::{Read, Write},
};

const MAX_STATE_BYTES: usize = 32768;

impl Store {
    fn read_connection(&self) -> Result<ConnectionState> {
        let path = self.root.join("connection-state");
        check_path(&path)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ConnectionState::default())
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES as u64 {
            return Err(Error::new("invalid-connection-state", 409));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::new("private-connection-state-required", 409));
            }
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(Error::new("invalid-connection-state", 409));
        }
        let state: ConnectionState = serde_json::from_slice(&protect(&bytes, false)?)
            .map_err(|_| Error::new("invalid-connection-state", 409))?;
        state.validate()?;
        Ok(state)
    }
    fn write_connection(&self, state: &ConnectionState) -> Result<()> {
        state.validate()?;
        let bytes = protect(
            &serde_json::to_vec(state).map_err(|_| Error::new("invalid-connection-state", 409))?,
            true,
        )?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(Error::new("connection-state-too-large", 413));
        }
        let path = self.root.join("connection-state");
        check_path(&path)?;
        let staging = self.root.join("staging");
        check_path(&staging)?;
        let mut temp = tempfile::NamedTempFile::new_in(staging)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        publish(temp.path(), &path)
    }
    pub fn configure_connection(&self, options: ConnectionOptions) -> Result<()> {
        options.validate()?;
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let mut state = self.read_connection()?;
        let same_mode = state.cloudflared == options.cloudflared;
        let directory_changed =
            state.directory.as_ref().map(|d| &d.base_url) != options.registry_url.as_ref();
        state.directory = match options.registry_url {
            Some(url) => Some(match state.directory.take() {
                Some(mut directory) => {
                    directory.base_url = url;
                    directory
                }
                None => generate_directory(url)?,
            }),
            None => None,
        };
        let endpoint = if options.cloudflared.is_some() && same_mode {
            state.endpoint.clone()
        } else {
            options.endpoint
        };
        if endpoint != state.endpoint || directory_changed {
            state.pending = None;
        }
        if directory_changed {
            state.last_published = None;
        }
        state.endpoint = endpoint;
        state.cloudflared = options.cloudflared;
        self.write_connection(&state)
    }
    pub fn connection_status(&self) -> Result<ConnectionStatus> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let state = self.read_connection()?;
        let publication = if state.directory.is_none() {
            "disabled"
        } else if state.endpoint.is_none() {
            "waiting-for-address"
        } else if state.pending.is_some() || state.last_published != state.endpoint {
            "pending"
        } else {
            "published"
        };
        Ok(ConnectionStatus {
            mode: if state.cloudflared.is_some() {
                "managed"
            } else if state.endpoint.is_some() {
                "fixed"
            } else {
                "unconfigured"
            },
            endpoint: state.endpoint,
            directory_enabled: state.directory.is_some(),
            publication,
        })
    }
    pub fn managed_cloudflared(&self) -> Result<Option<std::path::PathBuf>> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        Ok(self.read_connection()?.cloudflared)
    }
    pub fn observe_tunnel_endpoint(&self, endpoint: &str) -> Result<()> {
        validate_endpoint(endpoint, false)?;
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let mut state = self.read_connection()?;
        if state.cloudflared.is_none() {
            return Err(Error::new("managed-tunnel-not-configured", 409));
        }
        if state.endpoint.as_deref() != Some(endpoint) {
            state.pending = None;
            state.endpoint = Some(endpoint.to_owned());
            self.write_connection(&state)?;
        }
        Ok(())
    }
    pub fn plan_publication(&self) -> Result<Option<Publication>> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let mut state = self.read_connection()?;
        let (Some(directory), Some(endpoint)) = (state.directory.clone(), state.endpoint.clone())
        else {
            return Ok(None);
        };
        if state.pending.is_none() && state.last_published.as_ref() == Some(&endpoint) {
            return Ok(None);
        }
        if state.pending.is_none() {
            state.pending = Some(PendingPublication {
                envelope: seal_endpoint(&directory.uuid, &directory.key, &endpoint)?,
                endpoint,
            });
            self.write_connection(&state)?;
        }
        Ok(Some(Publication {
            directory,
            envelope: state.pending.as_ref().unwrap().envelope.clone(),
        }))
    }
    pub fn confirm_publication(&self, sent: &Publication) -> Result<()> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let mut state = self.read_connection()?;
        let current = state
            .directory
            .as_ref()
            .ok_or(Error::new("publication-superseded", 409))?;
        if current.base_url != sent.directory.base_url
            || current.uuid != sent.directory.uuid
            || current.key != sent.directory.key
            || state
                .pending
                .as_ref()
                .is_none_or(|p| p.envelope != sent.envelope)
        {
            return Err(Error::new("publication-superseded", 409));
        }
        state.last_published = state.pending.take().map(|p| p.endpoint);
        self.write_connection(&state)
    }
    pub fn request_republication(&self) -> Result<()> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let mut state = self.read_connection()?;
        if state.directory.is_none() {
            return Err(Error::new("directory-not-configured", 409));
        }
        state.last_published = None;
        self.write_connection(&state)
    }
    pub fn issue_registration(&self) -> Result<String> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let state = self.read_connection()?;
        let endpoint = state
            .endpoint
            .ok_or(Error::new("public-endpoint-not-ready", 409))?;
        let mut registration = Registration {
            endpoint,
            library_id: self.head()?.library_id,
            device_id: "0".repeat(64),
            token: "0".repeat(64),
            directory: state.directory,
        };
        registration.encode_uri()?; // Reject size/validation before allocating a device.
        let issued = self.add_device()?;
        registration.device_id = issued.device_id;
        registration.token = issued.token;
        registration.encode_uri().map_err(Into::into)
    }
}

#[cfg(not(windows))]
fn protect(bytes: &[u8], _: bool) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(windows)]
fn protect(bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .map_err(|_| Error::new("connection-state-too-large", 413))?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // Bind private operating state to this Windows user; no machine-wide fallback.
    unsafe {
        let ok = if seal {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(Error::new("connection-state-protection-failed", 503));
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        Ok(result)
    }
}

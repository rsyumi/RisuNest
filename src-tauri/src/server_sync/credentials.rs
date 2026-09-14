//! Device credentials never enter PDS, CAS, exports or sync projections.
use super::{client::ServerConfig, Result, SyncError};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredConfig {
    pub endpoint: String,
    pub library_id: String,
    pub device_id: String,
    credential_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Secrets {
    token: String,
    directory: Option<risunest_sync_connect::Directory>,
}

fn unavailable() -> SyncError {
    SyncError::new("device-credential-unavailable", 409)
}
impl StoredConfig {
    pub fn persist(root: &Path, config: &ServerConfig) -> Result<Self> {
        config.validate()?;
        let stored = Self {
            endpoint: config.endpoint.clone(),
            library_id: config.library_id.clone(),
            device_id: config.device_id.clone(),
            credential_id: uuid::Uuid::new_v4().to_string(),
        };
        let bytes = serde_json::to_vec(&Secrets {
            token: config.token.clone(),
            directory: config.directory.clone(),
        })
        .map_err(|_| unavailable())?;
        platform::write(root, &stored.credential_id, &bytes)?;
        Ok(stored)
    }
    pub fn resolve(&self, root: &Path) -> Result<ServerConfig> {
        self.validate_id()?;
        let secrets: Secrets = serde_json::from_slice(&platform::read(root, &self.credential_id)?)
            .map_err(|_| unavailable())?;
        let config = ServerConfig {
            directory: secrets.directory,
            endpoint: self.endpoint.clone(),
            library_id: self.library_id.clone(),
            device_id: self.device_id.clone(),
            token: secrets.token,
        };
        config.validate()?;
        Ok(config)
    }
    pub fn remove(&self, root: &Path) -> Result<()> {
        self.validate_id()?;
        platform::remove(root, &self.credential_id)
    }
    fn validate_id(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.credential_id)
            .is_ok_and(|id| id.to_string() == self.credential_id)
        {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}

#[cfg(any(windows, target_os = "android"))]
mod platform {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        path::PathBuf,
    };
    fn path(root: &Path, id: &str) -> PathBuf {
        root.join("server-sync-credentials").join(id)
    }
    pub fn write(root: &Path, id: &str, token: &[u8]) -> Result<()> {
        let bytes = super::protection::transform(token, true)?;
        let path = path(root, id);
        fs::create_dir_all(path.parent().ok_or_else(unavailable)?)?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }
    pub fn read(root: &Path, id: &str) -> Result<Vec<u8>> {
        let path = path(root, id);
        let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        if !metadata.is_file() || metadata.len() > 16384 {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(unavailable());
        }
        super::protection::transform(&bytes, false)
    }
    pub fn remove(root: &Path, id: &str) -> Result<()> {
        match fs::remove_file(path(root, id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(windows)]
mod protection {
    use super::*;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    pub fn transform(bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len().try_into().map_err(|_| unavailable())?,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // DPAPI binds to this Windows user, never CRYPTPROTECT_LOCAL_MACHINE.
        // Input lives across the call; output is owned by LocalFree on success.
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
                return Err(unavailable());
            }
            let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(output.pbData.cast());
            Ok(bytes)
        }
    }
}

#[cfg(target_os = "android")]
mod protection {
    use super::*;
    use jni::{
        objects::{GlobalRef, JByteArray, JClass, JObject, JValue},
        JNIEnv, JavaVM,
    };
    use std::sync::OnceLock;
    static JAVA: OnceLock<(JavaVM, GlobalRef)> = OnceLock::new();
    #[no_mangle]
    pub extern "system" fn Java_io_github_rsyumi_risunest_ServerSyncSecrets_initialize(
        env: JNIEnv,
        class: JClass,
    ) {
        if let (Ok(vm), Ok(class)) = (env.get_java_vm(), env.new_global_ref(class)) {
            let _ = JAVA.set((vm, class));
        }
    }
    pub fn transform(bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
        let (vm, class) = JAVA.get().ok_or_else(unavailable)?;
        let mut env = vm.attach_current_thread().map_err(|_| unavailable())?;
        let result = (|| {
            let input = env.byte_array_from_slice(bytes)?;
            let class: &JClass = class.as_obj().into();
            let output = env
                .call_static_method(
                    class,
                    if seal { "seal" } else { "open" },
                    "([B)[B",
                    &[JValue::Object(&JObject::from(input))],
                )?
                .l()?;
            env.convert_byte_array(JByteArray::from(output))
        })();
        if result.is_err() {
            let _ = env.exception_clear();
        }
        result.map_err(|_| unavailable())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    use super::*;
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };
    const SERVICE: &str = "io.github.rsyumi.risunest.server-sync";
    pub fn write(_: &Path, id: &str, token: &[u8]) -> Result<()> {
        set_generic_password(SERVICE, id, token).map_err(|_| unavailable())
    }
    pub fn read(_: &Path, id: &str) -> Result<Vec<u8>> {
        get_generic_password(SERVICE, id).map_err(|_| unavailable())
    }
    pub fn remove(_: &Path, id: &str) -> Result<()> {
        delete_generic_password(SERVICE, id).map_err(|_| unavailable())
    }
}

#[cfg(not(any(windows, target_os = "android", target_os = "macos", target_os = "ios")))]
mod platform {
    use super::*;
    // A Linux desktop needs an unlocked Secret Service. Never downgrade to a
    // plaintext file when that service is absent or locked.
    pub fn write(_: &Path, id: &str, token: &[u8]) -> Result<()> {
        keyring::Entry::new("io.github.rsyumi.risunest.server-sync", id)
            .and_then(|entry| entry.set_secret(token))
            .map_err(|_| unavailable())
    }
    pub fn read(_: &Path, id: &str) -> Result<Vec<u8>> {
        keyring::Entry::new("io.github.rsyumi.risunest.server-sync", id)
            .and_then(|entry| entry.get_secret())
            .map_err(|_| unavailable())
    }
    pub fn remove(_: &Path, id: &str) -> Result<()> {
        keyring::Entry::new("io.github.rsyumi.risunest.server-sync", id)
            .and_then(|entry| entry.delete_credential())
            .map_err(|_| unavailable())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn synthetic_credentials_roundtrip_through_keychain_and_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            directory: Some(
                risunest_sync_connect::generate_directory("https://registry.example".into())
                    .unwrap(),
            ),
            endpoint: "http://127.0.0.1:1".into(),
            library_id: "macos-synthetic-library".into(),
            device_id: "macos-synthetic-device".into(),
            token: "ab".repeat(32),
        };
        let stored = StoredConfig::persist(root.path(), &config).unwrap();
        // Only the newly generated credential ID is touched, including on assertion failure.
        struct Cleanup<'a>(&'a StoredConfig, &'a Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.remove(self.1);
            }
        }
        let _cleanup = Cleanup(&stored, root.path());
        let metadata = serde_json::to_string(&stored).unwrap();
        let directory = config.directory.as_ref().unwrap();
        assert!(!metadata.contains(&config.token));
        assert!(!metadata.contains(&directory.key));
        assert!(!metadata.contains(&directory.uuid));
        assert_eq!(stored.resolve(root.path()).unwrap().token, config.token);
        assert_eq!(
            stored.resolve(root.path()).unwrap().directory.unwrap().key,
            directory.key
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        stored.remove(root.path()).unwrap();
        assert!(stored.resolve(root.path()).is_err());
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn credential_is_os_protected_separate_and_corruption_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            directory: Some(
                risunest_sync_connect::generate_directory("https://registry.example".into())
                    .unwrap(),
            ),
            endpoint: "http://127.0.0.1:1".into(),
            library_id: "fixture".into(),
            device_id: "device".into(),
            token: "ab".repeat(32),
        };
        let stored = StoredConfig::persist(root.path(), &config).unwrap();
        let text = serde_json::to_string(&stored).unwrap();
        assert!(!text.contains(&config.token));
        let directory = config.directory.as_ref().unwrap();
        assert!(!text.contains(&directory.key));
        assert!(!text.contains(&directory.uuid));
        assert_eq!(
            stored.resolve(root.path()).unwrap().directory.unwrap().key,
            directory.key
        );
        assert_eq!(stored.resolve(root.path()).unwrap().token, config.token);
        let path = root
            .path()
            .join("server-sync-credentials")
            .join(&stored.credential_id);
        let mut bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.windows(64).any(|b| b == config.token.as_bytes()));
        bytes[20] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(stored.resolve(root.path()).is_err());
        stored.remove(root.path()).unwrap();
        assert!(stored.resolve(root.path()).is_err());
    }
}

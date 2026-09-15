//! OS-protected storage for external provider credentials, repository keys and
//! the account token.
//!
//! Every purpose intentionally uses a different directory, keychain service,
//! Android Keystore alias and reference prefix. A repository-key reference
//! therefore cannot be resolved through the provider vault, or vice versa.
use super::{
    auth::{SecretBytes, SecretVault},
    contract::{ErrorKind, ProviderError, ProviderFuture, Result, SecretRef},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_PLAINTEXT_BYTES: usize = 65_508;
const MAX_ENVELOPE_BYTES: u64 = 65_536;

#[derive(Clone, Copy)]
enum Purpose {
    Provider,
    RepositoryKey,
    AccountCredential,
}

impl Purpose {
    fn reference_prefix(self) -> &'static str {
        match self {
            Self::Provider => "provider-v1:",
            Self::RepositoryKey => "repository-key-v1:",
            Self::AccountCredential => "account-credential-v1:",
        }
    }

    fn directory(self) -> &'static str {
        match self {
            Self::Provider => "external-storage-secrets",
            Self::RepositoryKey => "external-storage-root-keys",
            Self::AccountCredential => "account-credentials",
        }
    }

    fn os_name(self) -> &'static str {
        match self {
            Self::Provider => "external-storage-secrets",
            Self::RepositoryKey => "external-storage-root-keys",
            Self::AccountCredential => "account-credentials",
        }
    }
}

pub(crate) struct NativeSecretVault {
    root: PathBuf,
    purpose: Purpose,
}

/// Provider credentials and sealed resumable-upload state share this vault.
/// The returned references contain only an opaque random identifier.
pub(crate) fn provider_vault(root: &Path) -> Arc<dyn SecretVault> {
    Arc::new(NativeSecretVault {
        root: root.to_owned(),
        purpose: Purpose::Provider,
    })
}

/// Repository root keys use an independent OS namespace from account tokens.
/// Independent recovery envelopes are created by the portable format core and
/// do not depend on this local vault.
pub(crate) fn repository_key_vault(root: &Path) -> Arc<dyn SecretVault> {
    Arc::new(NativeSecretVault {
        root: root.to_owned(),
        purpose: Purpose::RepositoryKey,
    })
}

/// One well-known secret addressed by a fixed name instead of an issued
/// reference, so nothing has to be stored beside it to find it again.
pub(crate) struct NamedSecretSlot {
    root: PathBuf,
    purpose: Purpose,
    name: &'static str,
}

/// The account token. An installation holds at most one, and a new device
/// obtains its own by signing in, so it never travels with stored data.
pub(crate) fn account_credential_slot(root: &Path) -> NamedSecretSlot {
    NamedSecretSlot {
        root: root.to_owned(),
        purpose: Purpose::AccountCredential,
        name: "official-account",
    }
}

impl NamedSecretSlot {
    /// An unreadable slot reads as an absent one: the account signs in again.
    pub(crate) fn read(&self) -> Option<SecretBytes> {
        platform::read(&self.root, self.purpose, self.name)
            .ok()
            .map(|bytes| SecretBytes(zeroize::Zeroizing::new(bytes)))
    }

    pub(crate) fn write(&self, bytes: &SecretBytes) -> Result<()> {
        if bytes.0.is_empty() || bytes.0.len() > MAX_PLAINTEXT_BYTES {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        match platform::replace(&self.root, self.purpose, self.name, &bytes.0) {
            // Every platform reports a missing destination this way, and only
            // that case may create the slot.
            Err(error) if error.kind == ErrorKind::ReauthRequired => {
                platform::write_new(&self.root, self.purpose, self.name, &bytes.0)
            }
            result => result,
        }
    }

    pub(crate) fn remove(&self) -> Result<()> {
        platform::remove(&self.root, self.purpose, self.name)
    }
}

fn unavailable() -> ProviderError {
    ProviderError::new(ErrorKind::ReauthRequired)
}

fn transient() -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

impl NativeSecretVault {
    fn parse_reference(&self, reference: &SecretRef) -> Result<String> {
        let id = reference
            .0
            .strip_prefix(self.purpose.reference_prefix())
            .ok_or_else(unavailable)?;
        let parsed = uuid::Uuid::parse_str(id).map_err(|_| unavailable())?;
        if parsed.get_version_num() != 4 || parsed.to_string() != id {
            return Err(unavailable());
        }
        Ok(id.to_owned())
    }

    fn reference(&self, id: &str) -> SecretRef {
        SecretRef(format!("{}{id}", self.purpose.reference_prefix()))
    }
}

impl SecretVault for NativeSecretVault {
    fn read<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move {
            let id = self.parse_reference(reference)?;
            let bytes = platform::read(&self.root, self.purpose, &id)?;
            Ok(SecretBytes(zeroize::Zeroizing::new(bytes)))
        })
    }

    fn store<'a>(&'a self, bytes: &'a SecretBytes) -> ProviderFuture<'a, SecretRef> {
        Box::pin(async move {
            if bytes.0.is_empty() || bytes.0.len() > MAX_PLAINTEXT_BYTES {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let id = uuid::Uuid::new_v4().to_string();
            platform::write_new(&self.root, self.purpose, &id, &bytes.0)?;
            Ok(self.reference(&id))
        })
    }

    fn replace<'a>(
        &'a self,
        reference: &'a SecretRef,
        bytes: &'a SecretBytes,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            if bytes.0.is_empty() || bytes.0.len() > MAX_PLAINTEXT_BYTES {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let id = self.parse_reference(reference)?;
            platform::replace(&self.root, self.purpose, &id, &bytes.0)
        })
    }

    fn remove<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let id = self.parse_reference(reference)?;
            platform::remove(&self.root, self.purpose, &id)
        })
    }
}

#[cfg(any(windows, target_os = "android"))]
mod platform {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
    };

    fn directory(root: &Path, purpose: Purpose) -> PathBuf {
        root.join(purpose.directory())
    }

    fn path(root: &Path, purpose: Purpose, id: &str) -> PathBuf {
        directory(root, purpose).join(id)
    }

    fn seal_and_sync(path: &Path, purpose: Purpose, bytes: &[u8], create_new: bool) -> Result<()> {
        let sealed = super::protection::transform(purpose, bytes, true)?;
        if sealed.len() as u64 > MAX_ENVELOPE_BYTES {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if create_new {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        let mut file = options.open(path).map_err(|_| transient())?;
        file.write_all(&sealed).map_err(|_| transient())?;
        file.sync_all().map_err(|_| transient())?;
        Ok(())
    }

    pub fn write_new(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        let directory = directory(root, purpose);
        fs::create_dir_all(&directory).map_err(|_| transient())?;
        seal_and_sync(&path(root, purpose, id), purpose, bytes, true)
    }

    pub fn read(root: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        let path = path(root, purpose, id);
        let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ENVELOPE_BYTES {
            return Err(unavailable());
        }
        let mut sealed = Vec::with_capacity(metadata.len() as usize);
        fs::File::open(path)
            .map_err(|_| unavailable())?
            .take(MAX_ENVELOPE_BYTES + 1)
            .read_to_end(&mut sealed)
            .map_err(|_| unavailable())?;
        if sealed.len() as u64 > MAX_ENVELOPE_BYTES {
            return Err(unavailable());
        }
        super::protection::transform(purpose, &sealed, false)
    }

    pub fn replace(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        let destination = path(root, purpose, id);
        if !fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.is_file()) {
            return Err(unavailable());
        }
        let temporary =
            directory(root, purpose).join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4()));
        seal_and_sync(&temporary, purpose, bytes, true)?;
        if let Err(error) = replace_file(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(target_os = "android")]
    fn replace_file(source: &Path, destination: &Path) -> Result<()> {
        fs::rename(source, destination).map_err(|_| transient())
    }

    #[cfg(windows)]
    fn replace_file(source: &Path, destination: &Path) -> Result<()> {
        use std::{iter, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let source: Vec<u16> = source
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();
        // The replacement file is already flushed. ReplaceFileW preserves a
        // single readable old-or-new destination across credential rotation.
        let ok = unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(transient())
        } else {
            Ok(())
        }
    }

    pub fn remove(root: &Path, purpose: Purpose, id: &str) -> Result<()> {
        match fs::remove_file(path(root, purpose, id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(transient()),
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

    pub fn transform(purpose: Purpose, bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len().try_into().map_err(|_| transient())?,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let entropy_bytes = purpose.os_name().as_bytes();
        let entropy = CRYPT_INTEGER_BLOB {
            cbData: entropy_bytes.len().try_into().map_err(|_| transient())?,
            pbData: entropy_bytes.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            if seal {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    &entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    &entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        if ok == 0 || output.pbData.is_null() {
            return Err(unavailable());
        }
        let transformed = unsafe {
            let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(output.pbData.cast());
            result
        };
        Ok(transformed)
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
    pub extern "system" fn Java_io_github_rsyumi_risunest_ExternalStorageSecrets_initialize(
        env: JNIEnv,
        class: JClass,
    ) {
        if let (Ok(vm), Ok(class)) = (env.get_java_vm(), env.new_global_ref(class)) {
            let _ = JAVA.set((vm, class));
        }
    }

    pub fn transform(purpose: Purpose, bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
        let (vm, class) = JAVA.get().ok_or_else(unavailable)?;
        let mut env = vm.attach_current_thread().map_err(|_| unavailable())?;
        let result = (|| {
            let purpose = env.new_string(purpose.os_name())?;
            let input = env.byte_array_from_slice(bytes)?;
            let purpose = JObject::from(purpose);
            let input = JObject::from(input);
            let class: &JClass = class.as_obj().into();
            let output = env
                .call_static_method(
                    class,
                    if seal { "seal" } else { "open" },
                    "(Ljava/lang/String;[B)[B",
                    &[JValue::Object(&purpose), JValue::Object(&input)],
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

    // Security.framework exposes OSStatus through its typed Error. This is
    // errSecItemNotFound from Security.framework/SecBase.h.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    fn service(purpose: Purpose) -> String {
        format!("io.github.rsyumi.risunest.{}", purpose.os_name())
    }

    pub fn write_new(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        if get_generic_password(&service(purpose), id).is_ok() {
            return Err(transient());
        }
        set_generic_password(&service(purpose), id, bytes).map_err(|_| transient())
    }

    pub fn read(_: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        get_generic_password(&service(purpose), id).map_err(|_| unavailable())
    }

    pub fn replace(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        if get_generic_password(&service(purpose), id).is_err() {
            return Err(unavailable());
        }
        set_generic_password(&service(purpose), id, bytes).map_err(|_| transient())
    }

    pub fn remove(_: &Path, purpose: Purpose, id: &str) -> Result<()> {
        match delete_generic_password(&service(purpose), id) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(_) => Err(unavailable()),
        }
    }
}

#[cfg(not(any(windows, target_os = "android", target_os = "macos", target_os = "ios")))]
mod platform {
    use super::*;

    fn entry(purpose: Purpose, id: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(
            &format!("io.github.rsyumi.risunest.{}", purpose.os_name()),
            id,
        )
        .map_err(|_| unavailable())
    }

    pub fn write_new(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        if entry(purpose, id)?.get_secret().is_ok() {
            return Err(transient());
        }
        entry(purpose, id)?
            .set_secret(bytes)
            .map_err(|_| transient())
    }

    pub fn read(_: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        entry(purpose, id)?.get_secret().map_err(|_| unavailable())
    }

    pub fn replace(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        if entry(purpose, id)?.get_secret().is_err() {
            return Err(unavailable());
        }
        entry(purpose, id)?
            .set_secret(bytes)
            .map_err(|_| transient())
    }

    pub fn remove(_: &Path, purpose: Purpose, id: &str) -> Result<()> {
        match entry(purpose, id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(unavailable()),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn purpose_bound_dpapi_vault_roundtrips_replaces_and_removes() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider_vault(root.path());
        let keys = repository_key_vault(root.path());
        let original = SecretBytes(zeroize::Zeroizing::new(
            b"synthetic-provider-token".to_vec(),
        ));
        let replacement = SecretBytes(zeroize::Zeroizing::new(b"rotated-provider-token".to_vec()));

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let reference = provider.store(&original).await.unwrap();
            assert_eq!(
                provider.read(&reference).await.unwrap().0.as_slice(),
                original.0.as_slice()
            );
            assert!(keys.read(&reference).await.is_err());
            provider.replace(&reference, &replacement).await.unwrap();
            assert_eq!(
                provider.read(&reference).await.unwrap().0.as_slice(),
                replacement.0.as_slice()
            );

            let path = root
                .path()
                .join("external-storage-secrets")
                .join(reference.0.strip_prefix("provider-v1:").unwrap());
            let sealed = std::fs::read(path).unwrap();
            assert!(!sealed
                .windows(replacement.0.len())
                .any(|part| part == replacement.0.as_slice()));

            provider.remove(&reference).await.unwrap();
            assert!(provider.read(&reference).await.is_err());
        });
    }

    #[test]
    fn the_account_slot_keeps_one_named_secret_in_its_own_namespace() {
        let root = tempfile::tempdir().unwrap();
        let slot = account_credential_slot(root.path());
        let token = SecretBytes(zeroize::Zeroizing::new(
            br#"{"id":"synthetic","token":"synthetic-account-token"}"#.to_vec(),
        ));
        let rotated = SecretBytes(zeroize::Zeroizing::new(
            br#"{"id":"synthetic","token":"rotated-account-token"}"#.to_vec(),
        ));

        assert!(slot.read().is_none());
        slot.write(&token).unwrap();
        assert_eq!(slot.read().unwrap().0.as_slice(), token.0.as_slice());
        slot.write(&rotated).unwrap();
        assert_eq!(slot.read().unwrap().0.as_slice(), rotated.0.as_slice());

        let sealed = std::fs::read(
            root.path()
                .join("account-credentials")
                .join("official-account"),
        )
        .unwrap();
        assert!(!sealed
            .windows(rotated.0.len())
            .any(|part| part == rotated.0.as_slice()));

        // The provider vault namespace cannot reach the account slot.
        let provider = provider_vault(root.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            assert!(provider
                .read(&SecretRef("provider-v1:official-account".into()))
                .await
                .is_err());
        });

        slot.remove().unwrap();
        assert!(slot.read().is_none());
        slot.remove().unwrap();
    }

    #[test]
    fn corrupt_cross_purpose_and_oversized_values_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let provider = provider_vault(root.path());
        let keys = repository_key_vault(root.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let key = SecretBytes(zeroize::Zeroizing::new(vec![7; 32]));
            let reference = keys.store(&key).await.unwrap();
            assert!(provider.read(&reference).await.is_err());
            assert!(provider
                .read(&SecretRef("provider-v1:../outside".into()))
                .await
                .is_err());
            let too_large = SecretBytes(zeroize::Zeroizing::new(vec![0; MAX_PLAINTEXT_BYTES + 1]));
            assert!(provider.store(&too_large).await.is_err());
            keys.remove(&reference).await.unwrap();
        });
    }
}

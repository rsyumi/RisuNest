//! Purpose-scoped device secrets shared by the account, external storage and Sync.
pub(crate) use crate::cleanup_secrets::{Backend, Purpose};
use std::path::{Path, PathBuf};

pub(crate) const MAX_PLAINTEXT_BYTES: usize = 65_508;
#[cfg(any(windows, target_os = "android"))]
const MAX_ENVELOPE_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Missing,
    Unavailable,
    Invalid,
}
type Result<T> = std::result::Result<T, Error>;
fn missing() -> Error {
    Error::Missing
}
fn unavailable() -> Error {
    Error::Unavailable
}

impl Purpose {
    pub(crate) fn reference_prefix(self) -> &'static str {
        match self {
            Self::Provider => "provider-v1:",
            Self::RepositoryKey => "repository-key-v1:",
            Self::AccountCredential => "account-credential-v1:",
            Self::ServerSync => "server-sync-v1:",
        }
    }
    fn directory(self) -> &'static str {
        match self {
            Self::Provider => "external-storage-secrets",
            Self::RepositoryKey => "external-storage-root-keys",
            Self::AccountCredential => "account-credentials",
            Self::ServerSync => "server-sync-credentials",
        }
    }
    fn os_name(self) -> &'static str {
        if self == Self::ServerSync {
            "server-sync"
        } else {
            self.directory()
        }
    }
    fn max_plaintext(self) -> usize {
        if self == Self::ServerSync {
            16_356
        } else {
            MAX_PLAINTEXT_BYTES
        }
    }
}

fn check_bytes(purpose: Purpose, bytes: &[u8]) -> Result<()> {
    if (bytes.is_empty() && purpose != Purpose::Provider) || bytes.len() > purpose.max_plaintext() {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

fn route(root: &Path, purpose: Purpose, id: &str) -> Result<Backend> {
    crate::cleanup_secrets::backend(root, purpose, id)
        .map_err(|_| unavailable())?
        .ok_or_else(missing)
}

pub(crate) fn read(root: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
    let bytes = match route(root, purpose, id)? {
        Backend::System => platform::read(root, purpose, id)?,
        #[cfg(target_os = "linux")]
        Backend::PrivateFile => private_file::read(root, purpose, id)?,
        #[cfg(not(target_os = "linux"))]
        Backend::PrivateFile => return Err(Error::Unavailable),
    };
    check_bytes(purpose, &bytes)?;
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum Write {
    New,
    Replace,
    Upsert,
}
fn write(root: &Path, purpose: Purpose, id: &str, bytes: &[u8], mode: Write) -> Result<()> {
    check_bytes(purpose, bytes)?;
    crate::cleanup_secrets::routed_write(
        root,
        purpose,
        id,
        || {
            if matches!(mode, Write::Replace) {
                return Err("secret-index-missing".into());
            }
            #[cfg(target_os = "linux")]
            {
                platform::select_backend().map_err(|_| "secret-store-unavailable".into())
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok(Backend::System)
            }
        },
        |backend| {
            let operation = |replace| match backend {
                Backend::System => {
                    if replace {
                        platform::replace(root, purpose, id, bytes)
                    } else {
                        platform::write_new(root, purpose, id, bytes)
                    }
                }
                #[cfg(target_os = "linux")]
                Backend::PrivateFile => private_file::write(root, purpose, id, bytes, replace),
                #[cfg(not(target_os = "linux"))]
                Backend::PrivateFile => Err(Error::Unavailable),
            };
            Ok(match mode {
                Write::New => operation(false),
                Write::Replace => operation(true),
                Write::Upsert => match operation(true) {
                    Err(Error::Missing) => operation(false),
                    result => result,
                },
            })
        },
    )
    .map_err(|_| unavailable())?
}
pub(crate) fn write_new(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
    write(root, purpose, id, bytes, Write::New)
}
pub(crate) fn replace(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
    write(root, purpose, id, bytes, Write::Replace)
}
pub(crate) fn upsert(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
    write(root, purpose, id, bytes, Write::Upsert)
}
pub(crate) fn remove(root: &Path, purpose: Purpose, id: &str) -> Result<()> {
    crate::cleanup_secrets::remove_one(root, purpose, id, |backend| {
        remove_owned(root, purpose, id, backend)
    })
    .map_err(|_| unavailable())
}
pub(crate) fn remove_owned(
    root: &Path,
    purpose: Purpose,
    id: &str,
    backend: Backend,
) -> std::result::Result<(), String> {
    let result = match backend {
        Backend::System => platform::remove(root, purpose, id),
        #[cfg(target_os = "linux")]
        Backend::PrivateFile => private_file::remove(root, purpose, id),
        #[cfg(not(target_os = "linux"))]
        Backend::PrivateFile => Err(Error::Unavailable),
    };
    result.map_err(|_| "secret-cleanup-unavailable".into())
}
#[cfg(target_os = "android")]
pub(crate) fn remove_android_keys() -> std::result::Result<(), String> {
    protection::remove_keys().map_err(|_| "secret-cleanup-unavailable".into())
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "linux"))]
fn service_name(purpose: Purpose) -> String {
    #[cfg(test)]
    {
        static PREFIX: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!("io.github.rsyumi.risunest.test.{}", uuid::Uuid::new_v4())
        });
        format!("{}.{}", *PREFIX, purpose.os_name())
    }
    #[cfg(not(test))]
    {
        format!("io.github.rsyumi.risunest.{}", purpose.os_name())
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
            return Err(Error::Invalid);
        }
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if create_new {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        let mut file = options.open(path).map_err(|_| unavailable())?;
        file.write_all(&sealed).map_err(|_| unavailable())?;
        file.sync_all().map_err(|_| unavailable())?;
        Ok(())
    }

    pub fn write_new(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        let directory = directory(root, purpose);
        fs::create_dir_all(&directory).map_err(|_| unavailable())?;
        seal_and_sync(&path(root, purpose, id), purpose, bytes, true)
    }

    pub fn read(root: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        let path = path(root, purpose, id);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                missing()
            } else {
                unavailable()
            }
        })?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ENVELOPE_BYTES {
            return Err(missing());
        }
        let mut sealed = Vec::with_capacity(metadata.len() as usize);
        fs::File::open(path)
            .map_err(|_| unavailable())?
            .take(MAX_ENVELOPE_BYTES + 1)
            .read_to_end(&mut sealed)
            .map_err(|_| unavailable())?;
        if sealed.len() as u64 > MAX_ENVELOPE_BYTES {
            return Err(missing());
        }
        super::protection::transform(purpose, &sealed, false)
    }

    pub fn replace(root: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        let destination = path(root, purpose, id);
        let metadata = fs::symlink_metadata(&destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                missing()
            } else {
                unavailable()
            }
        })?;
        if !metadata.is_file() {
            return Err(missing());
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
        fs::rename(source, destination).map_err(|_| unavailable())
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
            Err(unavailable())
        } else {
            Ok(())
        }
    }

    pub fn remove(root: &Path, purpose: Purpose, id: &str) -> Result<()> {
        match fs::remove_file(path(root, purpose, id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(unavailable()),
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
            cbData: bytes.len().try_into().map_err(|_| unavailable())?,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let entropy_bytes = purpose.os_name().as_bytes();
        let entropy = CRYPT_INTEGER_BLOB {
            cbData: entropy_bytes.len().try_into().map_err(|_| unavailable())?,
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
    pub extern "system" fn Java_io_github_rsyumi_risunest_DeviceSecrets_initialize(
        env: JNIEnv,
        class: JClass,
    ) {
        if let (Ok(vm), Ok(class)) = (env.get_java_vm(), env.new_global_ref(class)) {
            let _ = JAVA.set((vm, class));
        }
    }

    pub fn remove_keys() -> Result<()> {
        let (vm, class) = JAVA.get().ok_or_else(unavailable)?;
        let mut env = vm.attach_current_thread().map_err(|_| unavailable())?;
        let class: &JClass = class.as_obj().into();
        let result = env.call_static_method(class, "removeKeys", "()V", &[]);
        if result.is_err() {
            let _ = env.exception_clear();
        }
        result.map(|_| ()).map_err(|_| unavailable())
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
    #[cfg(target_os = "macos")]
    use security_framework::passwords::set_generic_password;
    use security_framework::passwords::{delete_generic_password, get_generic_password};

    // Security.framework exposes OSStatus through its typed Error. This is
    // errSecItemNotFound from Security.framework/SecBase.h.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    fn service(purpose: Purpose) -> String {
        service_name(purpose)
    }

    fn read_error(error: security_framework::base::Error) -> Error {
        if error.code() == ERR_SEC_ITEM_NOT_FOUND {
            missing()
        } else {
            unavailable()
        }
    }

    #[test]
    fn apple_access_failures_are_not_missing_credentials() {
        assert_eq!(
            read_error(security_framework::base::Error::from(
                ERR_SEC_ITEM_NOT_FOUND
            )),
            Error::Missing
        );
        for status in [-25308, -25293, -128] {
            assert_eq!(
                read_error(security_framework::base::Error::from(status)),
                Error::Unavailable
            );
        }
        assert!(service(Purpose::Provider).starts_with("io.github.rsyumi.risunest.test."));
    }

    pub fn write_new(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        match get_generic_password(&service(purpose), id) {
            Ok(_) => return Err(unavailable()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {}
            Err(error) => return Err(read_error(error)),
        }
        write_password(purpose, id, bytes, false)
    }

    pub fn read(_: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        get_generic_password(&service(purpose), id).map_err(read_error)
    }

    pub fn replace(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        get_generic_password(&service(purpose), id).map_err(read_error)?;
        write_password(purpose, id, bytes, true)
    }

    pub fn remove(_: &Path, purpose: Purpose, id: &str) -> Result<()> {
        match delete_generic_password(&service(purpose), id) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(_) => Err(unavailable()),
        }
    }

    #[cfg(target_os = "macos")]
    fn write_password(purpose: Purpose, id: &str, bytes: &[u8], replace: bool) -> Result<()> {
        if replace {
            let (_, mut item) = security_framework::os::macos::passwords::find_generic_password(
                None, &service(purpose), id,
            ).map_err(read_error)?;
            // A non-null slice pointer lets the keychain clear an existing password.
            item.set_password(bytes).map_err(|_| unavailable())
        } else {
            set_generic_password(&service(purpose), id, bytes).map_err(|_| unavailable())
        }
    }

    #[cfg(target_os = "ios")]
    fn write_password(purpose: Purpose, id: &str, bytes: &[u8], replace: bool) -> Result<()> {
        use core_foundation::{
            base::TCFType, data::CFData, dictionary::CFDictionary, string::CFString,
        };
        use security_framework_sys::access_control::{
            kSecAttrAccessibleAfterFirstUnlock, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        };
        use security_framework_sys::item::*;
        use security_framework_sys::keychain_item::{SecItemAdd, SecItemUpdate};
        extern "C" {
            static kSecAttrAccessible: core_foundation::string::CFStringRef;
        }
        let pair = |key, value| unsafe { (CFString::wrap_under_get_rule(key), value) };
        let mut query = vec![
            pair(
                unsafe { kSecClass },
                unsafe { CFString::wrap_under_get_rule(kSecClassGenericPassword) }.into_CFType(),
            ),
            pair(
                unsafe { kSecAttrService },
                CFString::new(&service(purpose)).into_CFType(),
            ),
            pair(unsafe { kSecAttrAccount }, CFString::new(id).into_CFType()),
        ];
        let attributes = vec![
            pair(
                unsafe { kSecValueData },
                CFData::from_buffer(bytes).into_CFType(),
            ),
            pair(
                unsafe { kSecAttrAccessible },
                unsafe {
                    CFString::wrap_under_get_rule(if purpose == Purpose::ServerSync {
                        kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
                    } else {
                        kSecAttrAccessibleAfterFirstUnlock
                    })
                }
                .into_CFType(),
            ),
        ];
        let status = if replace {
            let query = CFDictionary::from_CFType_pairs(&query);
            let attributes = CFDictionary::from_CFType_pairs(&attributes);
            unsafe {
                SecItemUpdate(
                    query.as_concrete_TypeRef(),
                    attributes.as_concrete_TypeRef(),
                )
            }
        } else {
            query.extend(attributes);
            let query = CFDictionary::from_CFType_pairs(&query);
            unsafe { SecItemAdd(query.as_concrete_TypeRef(), std::ptr::null_mut()) }
        };
        if status == 0 {
            Ok(())
        } else if status == ERR_SEC_ITEM_NOT_FOUND {
            Err(missing())
        } else {
            Err(unavailable())
        }
    }
}

#[cfg(not(any(windows, target_os = "android", target_os = "macos", target_os = "ios")))]
mod platform {
    use super::*;

    fn missing_bus(error: &dbus::Error) -> bool {
        matches!(
            error.name(),
            Some("org.freedesktop.DBus.Error.FileNotFound" | "org.freedesktop.DBus.Error.NoServer")
        )
    }

    pub(super) fn select_backend() -> Result<Backend> {
        use dbus::blocking::Connection;
        // Avoid D-Bus X11 autolaunch on machines with no session bus.
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
            && std::env::var_os("XDG_RUNTIME_DIR")
                .is_none_or(|directory| !Path::new(&directory).join("bus").exists())
        {
            return Ok(Backend::PrivateFile);
        }
        let connection = match Connection::new_session() {
            Ok(connection) => connection,
            Err(error) if missing_bus(&error) => return Ok(Backend::PrivateFile),
            Err(error) => return Err(bus_failed("connection", &error)),
        };
        let bus = connection.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            std::time::Duration::from_secs(5),
        );
        let (owned,): (bool,) = bus
            .method_call(
                "org.freedesktop.DBus",
                "NameHasOwner",
                ("org.freedesktop.secrets",),
            )
            .map_err(|error| bus_failed("lookup", &error))?;
        let present = if owned {
            true
        } else {
            let (names,): (Vec<String>,) = bus
                .method_call("org.freedesktop.DBus", "ListActivatableNames", ())
                .map_err(|error| bus_failed("lookup", &error))?;
            names.iter().any(|name| name == "org.freedesktop.secrets")
        };
        Ok(if present {
            Backend::System
        } else {
            Backend::PrivateFile
        })
    }

    #[test]
    fn only_absent_bus_errors_allow_file_storage() {
        for name in [
            "org.freedesktop.DBus.Error.FileNotFound",
            "org.freedesktop.DBus.Error.NoServer",
        ] {
            assert!(missing_bus(&dbus::Error::new_custom(name, "synthetic")));
        }
        for name in [
            "org.freedesktop.DBus.Error.AccessDenied",
            "org.freedesktop.DBus.Error.AuthFailed",
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.BadAddress",
        ] {
            assert!(!missing_bus(&dbus::Error::new_custom(name, "synthetic")));
        }
    }

    /// The D-Bus error name, or else the keyring error kind, behind a failed Secret Service call.
    fn failure_cause(error: &keyring::Error) -> String {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(current) = source {
            if let Some(name) = current.downcast_ref::<dbus::Error>().and_then(dbus::Error::name) {
                return name.to_owned();
            }
            source = current.source();
        }
        match error {
            keyring::Error::NoStorageAccess(_) => "no-storage-access",
            keyring::Error::PlatformFailure(_) => "platform-failure",
            keyring::Error::BadEncoding(_) => "bad-encoding",
            keyring::Error::TooLong(..) => "too-long",
            keyring::Error::Invalid(..) => "invalid",
            keyring::Error::Ambiguous(_) => "ambiguous",
            _ => "other",
        }
        .to_owned()
    }

    fn record_failure(operation: &str, cause: &str) -> Error {
        let message = format!("Secret Service {operation} failed: {cause}");
        // Bypass libtest capture: the call that precedes a daemon abort may belong to a passing test.
        // libtest names each test's thread after the test.
        #[cfg(test)]
        {
            use std::io::Write;
            let thread = std::thread::current();
            let _ = writeln!(std::io::stderr(), "{message} [{}]", thread.name().unwrap_or("unnamed thread"));
        }
        crate::native_log::log_global("warn", "device-secrets", message);
        unavailable()
    }

    fn failed(operation: &str, error: keyring::Error) -> Error {
        record_failure(operation, &failure_cause(&error))
    }

    fn bus_failed(operation: &str, error: &dbus::Error) -> Error {
        record_failure(operation, error.name().unwrap_or("unnamed-dbus-error"))
    }

    fn entry(purpose: Purpose, id: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&service_name(purpose), id).map_err(|error| failed("entry", error))
    }

    fn read_error(error: keyring::Error) -> Error {
        match error {
            keyring::Error::NoEntry => missing(),
            error => failed("read", error),
        }
    }

    #[test]
    fn linux_access_failures_are_not_missing_credentials() {
        assert_eq!(read_error(keyring::Error::NoEntry), Error::Missing);
        assert_eq!(
            read_error(keyring::Error::NoStorageAccess(Box::new(
                std::io::Error::other("synthetic locked collection")
            ))),
            Error::Unavailable
        );
        assert_eq!(
            read_error(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other("synthetic absent service")
            ))),
            Error::Unavailable
        );
        assert!(service_name(Purpose::Provider).starts_with("io.github.rsyumi.risunest.test."));
    }

    #[test]
    fn unavailable_secret_service_calls_log_their_cause() {
        // A real NoReply in the test log must come from a real Secret Service call.
        let synthetic = dbus::Error::new_custom("io.github.rsyumi.risunest.test.SyntheticError", "synthetic failure");
        let wrapped = dbus_secret_service_failure(synthetic);
        assert_eq!(read_error(keyring::Error::PlatformFailure(wrapped)), Error::Unavailable);
        assert_eq!(
            read_error(keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("synthetic")))),
            Error::Unavailable
        );
        let messages = crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .filter(|entry| entry.target == "device-secrets")
            .map(|entry| entry.message)
            .collect::<Vec<_>>();
        assert!(messages.contains(&"Secret Service read failed: io.github.rsyumi.risunest.test.SyntheticError".to_owned()), "{messages:?}");
        assert!(messages.contains(&"Secret Service read failed: no-storage-access".to_owned()), "{messages:?}");
    }

    /// Wraps a D-Bus error the way the Secret Service client does before keyring boxes it.
    #[cfg(test)]
    fn dbus_secret_service_failure(error: dbus::Error) -> Box<dyn std::error::Error + Send + Sync> {
        #[derive(Debug)]
        struct ClientError(dbus::Error);
        impl std::fmt::Display for ClientError {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "DBus error: {}", self.0)
            }
        }
        impl std::error::Error for ClientError {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        Box::new(ClientError(error))
    }

    pub fn write_new(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        match entry(purpose, id)?.get_secret() {
            Ok(_) => return Err(unavailable()),
            Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(read_error(error)),
        }
        entry(purpose, id)?
            .set_secret(bytes)
            .map_err(|error| failed("write", error))
    }

    pub fn read(_: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        entry(purpose, id)?.get_secret().map_err(read_error)
    }

    pub fn replace(_: &Path, purpose: Purpose, id: &str, bytes: &[u8]) -> Result<()> {
        entry(purpose, id)?.get_secret().map_err(read_error)?;
        entry(purpose, id)?
            .set_secret(bytes)
            .map_err(|error| failed("write", error))
    }

    pub fn remove(_: &Path, purpose: Purpose, id: &str) -> Result<()> {
        match entry(purpose, id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(failed("remove", error)),
        }
    }
}

#[cfg(target_os = "linux")]
mod private_file {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    };

    fn io_error(error: std::io::Error) -> Error {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing()
        } else {
            unavailable()
        }
    }
    fn check(metadata: &fs::Metadata, directory: bool) -> Result<()> {
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || if directory {
                !metadata.is_dir()
            } else {
                !metadata.is_file() || metadata.nlink() != 1
            }
        {
            return Err(unavailable());
        }
        Ok(())
    }
    fn directory(root: &Path, purpose: Purpose, create: bool) -> Result<PathBuf> {
        let base = root.join("device-secret-files");
        let path = base.join(purpose.directory());
        for current in [&base, &path] {
            if create {
                match fs::DirBuilder::new().mode(0o700).create(current) {
                    Ok(()) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(error) => return Err(io_error(error)),
                }
            }
            check(&fs::symlink_metadata(current).map_err(io_error)?, true)?;
        }
        Ok(path)
    }
    pub(super) fn read(root: &Path, purpose: Purpose, id: &str) -> Result<Vec<u8>> {
        let path = directory(root, purpose, false)?.join(id);
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        check(&metadata, false)?;
        if metadata.len() > purpose.max_plaintext() as u64 {
            return Err(Error::Invalid);
        }
        let mut bytes = Vec::new();
        file.take(purpose.max_plaintext() as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        check_bytes(purpose, &bytes)?;
        Ok(bytes)
    }
    pub(super) fn write(
        root: &Path,
        purpose: Purpose,
        id: &str,
        bytes: &[u8],
        replace: bool,
    ) -> Result<()> {
        let directory = directory(root, purpose, !replace)?;
        let destination = directory.join(id);
        if replace {
            check(
                &fs::symlink_metadata(&destination).map_err(io_error)?,
                false,
            )?;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(io_error)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(io_error)?;
        temporary
            .write_all(bytes)
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(io_error)?;
        if replace {
            temporary
                .persist(&destination)
                .map_err(|error| io_error(error.error))?;
        } else {
            temporary
                .persist_noclobber(&destination)
                .map_err(|error| io_error(error.error))?;
        }
        fs::File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(io_error)?;
        fs::File::open(directory.parent().unwrap())
            .and_then(|file| file.sync_all())
            .map_err(io_error)?;
        fs::File::open(root)
            .and_then(|file| file.sync_all())
            .map_err(io_error)?;
        Ok(())
    }
    pub(super) fn remove(root: &Path, purpose: Purpose, id: &str) -> Result<()> {
        let directory = match directory(root, purpose, false) {
            Err(Error::Missing) => return Ok(()),
            result => result?,
        };
        let path = directory.join(id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => check(&metadata, false)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        }
        fs::remove_file(path).map_err(io_error)?;
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(io_error)
    }

    #[test]
    fn private_files_roundtrip_rotate_and_reject_links_and_public_permissions() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        write(
            root.path(),
            Purpose::Provider,
            &id,
            b"synthetic-token",
            false,
        )
        .unwrap();
        let directory = directory(root.path(), Purpose::Provider, false).unwrap();
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        let path = directory.join(&id);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            read(root.path(), Purpose::Provider, &id).unwrap(),
            b"synthetic-token"
        );
        assert!(write(root.path(), Purpose::Provider, &id, b"duplicate", false).is_err());
        write(root.path(), Purpose::Provider, &id, b"rotated", true).unwrap();
        assert_eq!(
            read(root.path(), Purpose::Provider, &id).unwrap(),
            b"rotated"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(root.path(), Purpose::Provider, &id).is_err());
        assert!(write(root.path(), Purpose::Provider, &id, b"bad", true).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        remove(root.path(), Purpose::Provider, &id).unwrap();
        std::os::unix::fs::symlink(root.path().join("outside"), &path).unwrap();
        assert!(read(root.path(), Purpose::Provider, &id).is_err());
        assert!(write(root.path(), Purpose::Provider, &id, b"bad", true).is_err());
        assert!(!root.path().join("outside").exists());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn linux_backend_roundtrip_and_stable_routing() {
        let selected = platform::select_backend().unwrap();
        if let Ok(expected) = std::env::var("RISUNEST_TEST_SECRET_BACKEND") {
            assert_eq!(
                selected,
                match expected.as_str() {
                    "system" => Backend::System,
                    "file" => Backend::PrivateFile,
                    _ => panic!("invalid test backend"),
                }
            );
        }
        let root = tempfile::tempdir().unwrap();
        for purpose in [
            Purpose::Provider,
            Purpose::RepositoryKey,
            Purpose::AccountCredential,
            Purpose::ServerSync,
        ] {
            let id = if purpose == Purpose::AccountCredential {
                crate::cleanup_secrets::account_id(root.path())
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            write_new(root.path(), purpose, &id, b"synthetic-secret").unwrap();
            assert_eq!(
                crate::cleanup_secrets::backend(root.path(), purpose, &id).unwrap(),
                Some(selected)
            );
            assert_eq!(
                read(root.path(), purpose, &id).unwrap(),
                b"synthetic-secret"
            );
            replace(root.path(), purpose, &id, b"rotated-secret").unwrap();
            assert_eq!(read(root.path(), purpose, &id).unwrap(), b"rotated-secret");
        }
        crate::cleanup_secrets::remove_all(root.path()).unwrap();
        assert!(!root.path().join("owned-secret-index").exists());

        // Files selected earlier remain readable even when a keyring is now present.
        let id = uuid::Uuid::new_v4().to_string();
        crate::cleanup_secrets::routed_write(
            root.path(),
            Purpose::Provider,
            &id,
            || Ok(Backend::PrivateFile),
            |_| {
                private_file::write(root.path(), Purpose::Provider, &id, b"existing-file", false)
                    .map_err(|_| "failed".into())
            },
        )
        .unwrap();
        assert_eq!(
            read(root.path(), Purpose::Provider, &id).unwrap(),
            b"existing-file"
        );
        replace(root.path(), Purpose::Provider, &id, b"still-file").unwrap();
        assert_eq!(
            crate::cleanup_secrets::backend(root.path(), Purpose::Provider, &id).unwrap(),
            Some(Backend::PrivateFile)
        );
        remove(root.path(), Purpose::Provider, &id).unwrap();

        if selected == Backend::PrivateFile {
            let id = uuid::Uuid::new_v4().to_string();
            crate::cleanup_secrets::routed_write(
                root.path(),
                Purpose::Provider,
                &id,
                || Ok(Backend::System),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(
                read(root.path(), Purpose::Provider, &id),
                Err(Error::Unavailable)
            );
            assert_eq!(
                upsert(root.path(), Purpose::Provider, &id, b"must-not-downgrade"),
                Err(Error::Unavailable)
            );
            assert!(remove(root.path(), Purpose::Provider, &id).is_err());
            assert_eq!(
                crate::cleanup_secrets::backend(root.path(), Purpose::Provider, &id).unwrap(),
                Some(Backend::System)
            );
            assert!(!root
                .path()
                .join("device-secret-files/external-storage-secrets")
                .join(&id)
                .exists());
        }
    }
}

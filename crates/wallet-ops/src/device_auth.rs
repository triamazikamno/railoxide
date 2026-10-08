//! Local device authentication for small secrets such as the vault password.
//!
//! A secret is encrypted to a Secure Enclave P-256 key whose private half can
//! only be used after a Touch ID match or an explicit Apple Watch approval,
//! according to the method chosen when the key was created. Each method has
//! its own key. The private key never leaves the Secure Enclave: callers store
//! only its opaque handle and the ECIES ciphertext, and both are useless on any
//! other Mac. Other platforms report [`DeviceAuthError::Unavailable`].

use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthMethod {
    TouchId,
    AppleWatch,
}

impl DeviceAuthMethod {
    pub const ALL: [Self; 2] = [Self::TouchId, Self::AppleWatch];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::TouchId => "Touch ID",
            Self::AppleWatch => "Apple Watch",
        }
    }
}

#[derive(Debug, Error)]
pub enum DeviceAuthError {
    #[error("Device authentication is not available right now")]
    Unavailable,
    #[error("Device authentication was cancelled")]
    Cancelled,
    #[error("Another authentication request is already in progress")]
    InProgress,
    #[error(
        "Touch ID is locked. Unlock your Mac with its login password, then try Touch ID again."
    )]
    LockedOut,
    #[error("Device authentication failed: {0}")]
    Failed(String),
}

/// A secret sealed to a device-authenticated Secure Enclave key.
#[non_exhaustive]
pub struct SealedSecret {
    /// Opaque Secure Enclave key handle. It only works on the Mac that made it.
    pub key_handle: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Whether the selected method can authenticate right now.
#[must_use]
pub fn device_auth_available(method: DeviceAuthMethod) -> bool {
    platform::available(method)
}

/// Whether the method is configured, independent of temporary unavailability
/// where the operating system exposes that distinction.
#[must_use]
pub fn device_auth_supported(method: DeviceAuthMethod) -> bool {
    platform::supported(method)
}

/// An opaque enrollment state, when exposed by this macOS version.
#[must_use]
pub fn device_auth_domain_state(method: DeviceAuthMethod) -> Option<Vec<u8>> {
    platform::domain_state(method)
}

/// Seals `secret` without prompting. Only [`open_secret`] authenticates.
pub fn seal_secret(
    method: DeviceAuthMethod,
    secret: &[u8],
) -> Result<SealedSecret, DeviceAuthError> {
    platform::seal(method, secret)
}

/// Requests a fresh approval with `reason` and returns the sealed secret.
///
/// Blocks the calling thread until the prompt is answered, so call it off the
/// UI thread.
pub fn open_secret(
    method: DeviceAuthMethod,
    sealed: &SealedSecret,
    reason: &str,
) -> Result<Zeroizing<Vec<u8>>, DeviceAuthError> {
    // Dialogs may be replaced while an OS prompt is still outstanding. Never
    // queue a second native prompt behind a request whose view has closed.
    static PROMPT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _prompt = PROMPT.try_lock().map_err(|_| DeviceAuthError::InProgress)?;
    platform::open(method, sealed, reason)
}

#[cfg(target_os = "macos")]
mod platform {
    use block2::RcBlock;
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::error::CFError;
    use core_foundation::string::CFString;
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, sel};
    use objc2_foundation::{NSData, NSString};
    use security_framework::access_control::{ProtectionMode, SecAccessControl};
    use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey, Token};
    use security_framework_sys::access_control::{
        kSecAccessControlBiometryCurrentSet, kSecAccessControlPrivateKeyUsage,
        kSecAccessControlWatch,
    };
    use security_framework_sys::item::{
        kSecAttrKeyClass, kSecAttrKeyClassPrivate, kSecAttrKeyType,
        kSecAttrKeyTypeECSECPrimeRandom, kSecAttrTokenID, kSecAttrTokenIDSecureEnclave,
        kSecUseAuthenticationContext,
    };
    use security_framework_sys::key::SecKeyCreateWithData;
    use zeroize::Zeroizing;

    use super::{DeviceAuthError, DeviceAuthMethod, SealedSecret};

    #[link(name = "LocalAuthentication", kind = "framework")]
    unsafe extern "C" {}

    /// `LAPolicyDeviceOwnerAuthenticationWithBiometrics`
    const LA_POLICY_BIOMETRICS: isize = 1;
    /// `LAPolicyDeviceOwnerAuthenticationWithWatch`, renamed to
    /// `LAPolicyDeviceOwnerAuthenticationWithCompanion` with the same value.
    const LA_POLICY_WATCH: isize = 3;
    /// `LACompanionTypeWatch`
    const LA_COMPANION_WATCH: isize = 1;
    /// `LABiometryTypeTouchID`
    const LA_BIOMETRY_TOUCH_ID: isize = 1;
    const ALGORITHM: Algorithm = Algorithm::ECIESEncryptionCofactorVariableIVX963SHA256AESGCM;
    /// `kSecAttrTokenOID`: the Secure Enclave key handle. The SDK does not export
    /// the constant, but `SecKeyCopyAttributes` returns it under this name and
    /// `SecKeyCreateWithData` accepts it to reopen the key.
    const TOKEN_OBJECT_ID: &str = "toid";
    const LA_ERROR_DOMAIN: &str = "com.apple.LocalAuthentication";
    /// `LAErrorBiometryLockout`
    const LA_BIOMETRY_LOCKOUT: isize = -8;
    const LA_CANCEL_CODES: [isize; 4] = [
        -2, // LAErrorUserCancel
        -3, // LAErrorUserFallback
        -4, // LAErrorSystemCancel
        -9, // LAErrorAppCancel
    ];
    const OS_STATUS_ERROR_DOMAIN: &str = "NSOSStatusErrorDomain";
    const ERR_SEC_USER_CANCELED: isize = -128;
    const TOKEN_ERROR_DOMAIN: &str = "CryptoTokenKit";
    const TOKEN_CANCELED_BY_USER: isize = -9;

    fn la_context() -> Option<Retained<AnyObject>> {
        let class = AnyClass::get(c"LAContext")?;
        Some(unsafe { msg_send![class, new] })
    }

    fn can_evaluate(context: &AnyObject, method: DeviceAuthMethod) -> Result<(), DeviceAuthError> {
        let mut error: Option<Retained<AnyObject>> = None;
        let available: bool = unsafe {
            msg_send![
                context,
                canEvaluatePolicy: match method {
                    DeviceAuthMethod::TouchId => LA_POLICY_BIOMETRICS,
                    DeviceAuthMethod::AppleWatch => LA_POLICY_WATCH,
                },
                error: &mut error
            ]
        };
        if available {
            return Ok(());
        }
        if let Some(error) = error {
            let domain: Retained<NSString> = unsafe { msg_send![&*error, domain] };
            let code: isize = unsafe { msg_send![&*error, code] };
            if domain.to_string() == LA_ERROR_DOMAIN && code == LA_BIOMETRY_LOCKOUT {
                return Err(DeviceAuthError::LockedOut);
            }
        }
        Err(DeviceAuthError::Unavailable)
    }

    pub(super) fn available(method: DeviceAuthMethod) -> bool {
        la_context().is_some_and(|context| can_evaluate(&context, method).is_ok())
    }

    pub(super) fn supported(method: DeviceAuthMethod) -> bool {
        la_context().is_some_and(|context| {
            let available = can_evaluate(&context, method).is_ok();
            match method {
                DeviceAuthMethod::TouchId => {
                    // The type is populated even when the policy reports lockout.
                    let biometry_type: isize = unsafe { msg_send![&*context, biometryType] };
                    biometry_type == LA_BIOMETRY_TOUCH_ID
                }
                DeviceAuthMethod::AppleWatch => available || watch_domain_state(&context).is_some(),
            }
        })
    }

    fn watch_domain_state(context: &AnyObject) -> Option<Vec<u8>> {
        // Companion enrollment hashes were added in macOS 15. Older releases
        // still support Watch approval but do not expose this enrollment hash.
        let supported: bool = unsafe { msg_send![context, respondsToSelector: sel!(domainState)] };
        if !supported {
            return None;
        }
        let domain: Retained<AnyObject> = unsafe { msg_send![context, domainState] };
        let companion: Retained<AnyObject> = unsafe { msg_send![&*domain, companion] };
        let state: Option<Retained<NSData>> =
            unsafe { msg_send![&*companion, stateHashForCompanionType: LA_COMPANION_WATCH] };
        state.map(|state| state.to_vec())
    }

    pub(super) fn domain_state(method: DeviceAuthMethod) -> Option<Vec<u8>> {
        let context = la_context()?;
        let _ = can_evaluate(&context, method);
        if method == DeviceAuthMethod::AppleWatch {
            return watch_domain_state(&context);
        }
        let state: Option<Retained<NSData>> =
            unsafe { msg_send![&*context, evaluatedPolicyDomainState] };
        state.map(|state| state.to_vec())
    }

    pub(super) fn seal(
        method: DeviceAuthMethod,
        secret: &[u8],
    ) -> Result<SealedSecret, DeviceAuthError> {
        let context = la_context().ok_or(DeviceAuthError::Unavailable)?;
        can_evaluate(&context, method)?;
        let access_control = SecAccessControl::create_with_protection(
            Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
            match method {
                DeviceAuthMethod::TouchId => kSecAccessControlBiometryCurrentSet,
                DeviceAuthMethod::AppleWatch => kSecAccessControlWatch,
            } | kSecAccessControlPrivateKeyUsage,
        )
        .map_err(|error| DeviceAuthError::Failed(error.to_string()))?;
        let mut options = GenerateKeyOptions::default();
        options
            .set_key_type(KeyType::ec_sec_prime_random())
            .set_size_in_bits(256)
            .set_token(Token::SecureEnclave)
            .set_access_control(access_control);
        // No location: the key stays out of the keychain, which would need
        // entitlements that source builds do not have.
        let key = SecKey::new(&options).map_err(|error| failed(&error))?;
        let key_handle = key
            .attributes()
            .find(CFString::from_static_string(TOKEN_OBJECT_ID).as_CFTypeRef())
            .map(|handle| unsafe { CFData::wrap_under_get_rule(handle.cast()) }.to_vec())
            .ok_or_else(|| DeviceAuthError::Failed("Secure Enclave key handle missing".into()))?;
        let ciphertext = key
            .public_key()
            .ok_or_else(|| DeviceAuthError::Failed("Secure Enclave public key missing".into()))?
            .encrypt_data(ALGORITHM, secret)
            .map_err(|error| failed(&error))?;
        Ok(SealedSecret {
            key_handle,
            ciphertext,
        })
    }

    pub(super) fn open(
        method: DeviceAuthMethod,
        sealed: &SealedSecret,
        reason: &str,
    ) -> Result<Zeroizing<Vec<u8>>, DeviceAuthError> {
        let context = la_context().ok_or(DeviceAuthError::Unavailable)?;
        can_evaluate(&context, method)?;
        let reason = NSString::from_str(reason);
        let no_fallback = NSString::from_str("");
        unsafe {
            let () = msg_send![&*context, setLocalizedReason: &*reason];
            let () = msg_send![&*context, setLocalizedFallbackTitle: &*no_fallback];
        }
        // Never reuse a context between operations. A previous Watch approval,
        // including macOS Auto Unlock, must not authorize this request.
        if method == DeviceAuthMethod::AppleWatch {
            approve_with_watch(&context, &reason)?;
            // This operation may consume only the approval just obtained. If
            // the key rejects it, do not start another or broader prompt.
            unsafe {
                let () = msg_send![&*context, setInteractionNotAllowed: true];
            }
        }
        let key = reopen_private_key(&sealed.key_handle, &context)?;
        key.decrypt_data(ALGORITHM, &sealed.ciphertext)
            .map(Zeroizing::new)
            .map_err(|error| {
                if is_cancel(&error) {
                    DeviceAuthError::Cancelled
                } else {
                    failed(&error)
                }
            })
    }

    fn approve_with_watch(context: &AnyObject, reason: &NSString) -> Result<(), DeviceAuthError> {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let reply = RcBlock::new(move |success: Bool, error: *mut AnyObject| {
            let outcome = if success.as_bool() {
                Ok(())
            } else if let Some(error) = unsafe { error.as_ref() } {
                let domain: Retained<NSString> = unsafe { msg_send![error, domain] };
                let code: isize = unsafe { msg_send![error, code] };
                if domain.to_string() == LA_ERROR_DOMAIN && LA_CANCEL_CODES.contains(&code) {
                    Err(DeviceAuthError::Cancelled)
                } else if domain.to_string() == LA_ERROR_DOMAIN && code == -1000 {
                    // LAErrorWatchNotAvailable / LAErrorCompanionNotAvailable.
                    Err(DeviceAuthError::Unavailable)
                } else {
                    // Report the public error identity, never authentication material.
                    Err(DeviceAuthError::Failed(format!("{domain} ({code})")))
                }
            } else {
                Err(DeviceAuthError::Failed(
                    "Apple Watch approval failed".into(),
                ))
            };
            let _ = send.send(outcome);
        });
        unsafe {
            let () = msg_send![context,
                evaluatePolicy: LA_POLICY_WATCH,
                localizedReason: reason,
                reply: &*reply
            ];
        }
        // This function runs on the blocking pool. Timeout cancels the context
        // and cannot release a secret even if a late reply reports success.
        if let Ok(result) = receive.recv_timeout(std::time::Duration::from_secs(120)) {
            result
        } else {
            unsafe {
                let () = msg_send![context, invalidate];
            }
            Err(DeviceAuthError::Cancelled)
        }
    }

    fn reopen_private_key(handle: &[u8], context: &AnyObject) -> Result<SecKey, DeviceAuthError> {
        let string = |value| unsafe { CFString::wrap_under_get_rule(value) };
        // LAContext is an Objective-C object, which CoreFoundation can retain.
        let context =
            unsafe { CFType::wrap_under_get_rule(std::ptr::from_ref(context) as CFTypeRef) };
        let handle = CFData::from_buffer(handle);
        let attributes = CFDictionary::from_CFType_pairs(&[
            (
                string(unsafe { kSecAttrTokenID }),
                string(unsafe { kSecAttrTokenIDSecureEnclave }).as_CFType(),
            ),
            (
                CFString::from_static_string(TOKEN_OBJECT_ID),
                handle.as_CFType(),
            ),
            (
                string(unsafe { kSecAttrKeyType }),
                string(unsafe { kSecAttrKeyTypeECSECPrimeRandom }).as_CFType(),
            ),
            (
                string(unsafe { kSecAttrKeyClass }),
                string(unsafe { kSecAttrKeyClassPrivate }).as_CFType(),
            ),
            (string(unsafe { kSecUseAuthenticationContext }), context),
        ]);
        let mut error = std::ptr::null_mut();
        let key = unsafe {
            SecKeyCreateWithData(
                handle.as_concrete_TypeRef(),
                attributes.as_concrete_TypeRef(),
                &raw mut error,
            )
        };
        if key.is_null() {
            return Err(if error.is_null() {
                DeviceAuthError::Failed("Secure Enclave key could not be opened".into())
            } else {
                failed(&unsafe { CFError::wrap_under_create_rule(error) })
            });
        }
        Ok(unsafe { SecKey::wrap_under_create_rule(key) })
    }

    fn is_cancel(error: &CFError) -> bool {
        let domain = error.domain().to_string();
        let code = error.code();
        (domain == LA_ERROR_DOMAIN && LA_CANCEL_CODES.contains(&code))
            || (domain == OS_STATUS_ERROR_DOMAIN && code == ERR_SEC_USER_CANCELED)
            || (domain == TOKEN_ERROR_DOMAIN && code == TOKEN_CANCELED_BY_USER)
    }

    fn failed(error: &CFError) -> DeviceAuthError {
        if error.domain() == LA_ERROR_DOMAIN && error.code() == LA_BIOMETRY_LOCKOUT {
            return DeviceAuthError::LockedOut;
        }
        DeviceAuthError::Failed(format!(
            "{} ({} {})",
            error.description(),
            error.domain(),
            error.code()
        ))
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    // Not `const`, so the public wrappers keep the same signature on every platform.
    #![allow(clippy::missing_const_for_fn)]

    use zeroize::Zeroizing;

    use super::{DeviceAuthError, DeviceAuthMethod, SealedSecret};

    pub(super) fn available(_method: DeviceAuthMethod) -> bool {
        false
    }

    pub(super) fn supported(_method: DeviceAuthMethod) -> bool {
        false
    }

    pub(super) fn domain_state(_method: DeviceAuthMethod) -> Option<Vec<u8>> {
        None
    }

    pub(super) fn seal(
        _method: DeviceAuthMethod,
        _secret: &[u8],
    ) -> Result<SealedSecret, DeviceAuthError> {
        Err(DeviceAuthError::Unavailable)
    }

    pub(super) fn open(
        _method: DeviceAuthMethod,
        _sealed: &SealedSecret,
        _reason: &str,
    ) -> Result<Zeroizing<Vec<u8>>, DeviceAuthError> {
        Err(DeviceAuthError::Unavailable)
    }
}

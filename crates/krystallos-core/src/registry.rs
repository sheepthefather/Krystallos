use crate::{Error, Result, StorageBackend};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};

/// Per-connection settings that are neither credentials nor part of the URI.
///
/// This exists because some settings are genuinely the caller's business and
/// belong in neither of the obvious places. SMB3 encryption is the motivating
/// case: it is requested per connection, it is not a credential, and it cannot
/// sensibly be spelled into a share path.
///
/// Keys are namespaced by protocol so that a caller configuring several
/// backends cannot have one protocol's setting silently apply to another. A key
/// a backend does not recognise is ignored — the alternative, failing, would
/// make every option a breaking change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionOptions {
    settings: BTreeMap<String, String>,
}

impl ConnectionOptions {
    pub fn new() -> Self {
        ConnectionOptions::default()
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.settings.insert(key.into(), value.into());
        self
    }

    /// Builder form, for use where the options are assembled inline.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set(key, value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }

    /// Read a boolean setting.
    ///
    /// Returns `None` when the key is absent, and also when its value is not a
    /// recognised boolean — a typo must not be silently read as `false` for a
    /// setting whose whole point is to turn something on.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)?.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.settings.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.settings.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Credentials for a connection attempt.
///
/// Kept apart from the endpoint URI on purpose. Credentials embedded in a URL
/// leak into logs, error messages and `Display` output; passing them as a
/// separate value makes that leak a deliberate act rather than an accident.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: Option<String>,
    /// Protocol-specific authentication realm. For SMB this is the domain or
    /// workgroup. Backends that have no such concept ignore it.
    pub domain: Option<String>,
}

impl Credentials {
    /// Credentials for an anonymous / guest connection.
    pub fn anonymous() -> Self {
        Credentials::default()
    }

    pub fn user_password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Credentials {
            username: Some(username.into()),
            password: Some(password.into()),
            domain: None,
        }
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }
}

/// Never print secrets, not even by accident in a log line or a panic message.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("domain", &self.domain)
            .finish()
    }
}

/// A parsed endpoint URI.
///
/// The registry only understands enough of a URI to route it: the scheme, and
/// the remainder for the driver to interpret. What comes after `://` is
/// entirely the backend's business — that is how SMB's share name stays out of
/// the shared layer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Endpoint {
    scheme: String,
    authority_and_path: String,
    raw: String,
}

impl Endpoint {
    /// Parse `scheme://rest`. The scheme is matched case-insensitively and
    /// normalized to lowercase.
    pub fn parse(uri: &str) -> Result<Self> {
        let Some((scheme, rest)) = uri.split_once("://") else {
            return Err(Error::InvalidPath {
                path: uri.to_string(),
                reason: "endpoint is missing a `scheme://` prefix".to_string(),
            });
        };

        if scheme.is_empty() {
            return Err(Error::InvalidPath {
                path: uri.to_string(),
                reason: "endpoint has an empty scheme".to_string(),
            });
        }
        if !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return Err(Error::InvalidPath {
                path: uri.to_string(),
                reason: format!("`{scheme}` is not a valid URI scheme"),
            });
        }

        Ok(Endpoint {
            scheme: scheme.to_ascii_lowercase(),
            authority_and_path: rest.to_string(),
            raw: uri.to_string(),
        })
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Everything after `://`, for the driver to parse however it likes.
    pub fn authority_and_path(&self) -> &str {
        &self.authority_and_path
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Opens sessions for one URI scheme.
///
/// A driver is the only thing a new protocol needs to add: implement this, hand
/// it to [`BackendRegistry::register`], and the scheme becomes reachable
/// everywhere — CLI, FFI facade, and anything built on them.
#[async_trait]
pub trait BackendDriver: Send + Sync {
    /// The URI scheme this driver handles, lowercase and without `://`.
    fn scheme(&self) -> &'static str;

    /// A short human-readable description, for `--help` output and error
    /// messages that list what is available.
    fn description(&self) -> &'static str {
        ""
    }

    /// Establish a session.
    ///
    /// Implementations should authenticate here rather than lazily, so that a
    /// bad password surfaces at connect time with a clear error instead of
    /// midway through a transfer.
    async fn connect(
        &self,
        endpoint: &Endpoint,
        credentials: &Credentials,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn StorageBackend>>;
}

/// Maps URI schemes to drivers.
///
/// Deliberately a plain registry rather than anything cleverer: the set of
/// protocols is tiny, lookup happens once per connection, and a `HashMap`
/// keeps the failure mode obvious ("no driver for scheme `sftp`").
#[derive(Default)]
pub struct BackendRegistry {
    drivers: HashMap<String, Box<dyn BackendDriver>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        BackendRegistry::default()
    }

    /// Register a driver, returning any previously registered driver for the
    /// same scheme. A driver registered later wins; the displaced one is
    /// returned rather than dropped silently, so a caller can notice a
    /// conflict.
    pub fn register(&mut self, driver: Box<dyn BackendDriver>) -> Option<Box<dyn BackendDriver>> {
        self.drivers.insert(driver.scheme().to_ascii_lowercase(), driver)
    }

    pub fn driver_for(&self, scheme: &str) -> Option<&dyn BackendDriver> {
        self.drivers.get(&scheme.to_ascii_lowercase()).map(|d| d.as_ref())
    }

    /// Registered schemes, sorted, for diagnostics.
    pub fn schemes(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.drivers.keys().map(|s| s.as_str()).collect();
        v.sort_unstable();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.drivers.len()
    }

    /// Connect to `uri`, dispatching on its scheme.
    pub async fn connect(
        &self,
        uri: &str,
        credentials: &Credentials,
    ) -> Result<Box<dyn StorageBackend>> {
        self.connect_with(uri, credentials, &ConnectionOptions::new())
            .await
    }

    /// Connect with protocol-specific settings.
    pub async fn connect_with(
        &self,
        uri: &str,
        credentials: &Credentials,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn StorageBackend>> {
        let endpoint = Endpoint::parse(uri)?;
        let Some(driver) = self.drivers.get(endpoint.scheme()) else {
            return Err(Error::Unsupported {
                operation: "connect: no driver registered for this scheme",
            });
        };
        driver.connect(&endpoint, credentials, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scheme_and_remainder() {
        let e = Endpoint::parse("smb://nas.local/media/movies").unwrap();
        assert_eq!(e.scheme(), "smb");
        assert_eq!(e.authority_and_path(), "nas.local/media/movies");
        assert_eq!(e.as_str(), "smb://nas.local/media/movies");
    }

    #[test]
    fn scheme_matching_is_case_insensitive() {
        let e = Endpoint::parse("SMB://host/share").unwrap();
        assert_eq!(e.scheme(), "smb");
    }

    #[test]
    fn base64_style_scheme_characters_are_allowed() {
        assert!(Endpoint::parse("web+dav://h/p").is_ok());
        assert!(Endpoint::parse("a-b.c://h/p").is_ok());
    }

    #[test]
    fn rejects_missing_or_malformed_scheme() {
        for bad in ["nas.local/share", "://host/share", "sm b://host"] {
            assert!(
                Endpoint::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn credentials_debug_never_reveals_the_password() {
        let c = Credentials::user_password("alice", "hunter2");
        let rendered = format!("{c:?}");
        assert!(
            !rendered.contains("hunter2"),
            "password leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        assert!(rendered.contains("alice"), "username should still be visible");
    }

    #[test]
    fn registry_reports_registered_schemes_sorted() {
        struct D(&'static str);
        #[async_trait]
        impl BackendDriver for D {
            fn scheme(&self) -> &'static str {
                self.0
            }
            async fn connect(
                &self,
                _e: &Endpoint,
                _c: &Credentials,
                _o: &ConnectionOptions,
            ) -> Result<Box<dyn StorageBackend>> {
                Err(Error::Unsupported { operation: "test" })
            }
        }

        let mut reg = BackendRegistry::new();
        assert!(reg.is_empty());
        reg.register(Box::new(D("smb")));
        reg.register(Box::new(D("file")));
        assert_eq!(reg.schemes(), ["file", "smb"]);
        assert_eq!(reg.len(), 2);
        assert!(reg.driver_for("SMB").is_some(), "lookup is case-insensitive");
        assert!(reg.driver_for("sftp").is_none());
    }

    #[test]
    fn registering_the_same_scheme_returns_the_displaced_driver() {
        struct D(&'static str);
        #[async_trait]
        impl BackendDriver for D {
            fn scheme(&self) -> &'static str {
                self.0
            }
            async fn connect(
                &self,
                _e: &Endpoint,
                _c: &Credentials,
                _o: &ConnectionOptions,
            ) -> Result<Box<dyn StorageBackend>> {
                Err(Error::Unsupported { operation: "test" })
            }
        }

        let mut reg = BackendRegistry::new();
        assert!(reg.register(Box::new(D("smb"))).is_none());
        assert!(
            reg.register(Box::new(D("smb"))).is_some(),
            "second registration should hand back the first driver"
        );
        assert_eq!(reg.len(), 1);
    }

    #[tokio::test]
    async fn connect_on_an_unregistered_scheme_names_the_failure() {
        let reg = BackendRegistry::new();
        let err = reg.connect("sftp://h/p", &Credentials::anonymous()).await;
        assert!(matches!(err, Err(Error::Unsupported { .. })));
    }
}

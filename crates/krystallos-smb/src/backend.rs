use crate::actor::{ConnectConfig, Session};
use crate::DEFAULT_TIMEOUT_SECS;
use async_trait::async_trait;
use krystallos_core::{
    BackendDriver, Capabilities, Credentials, Endpoint, Entry, Error, FileHandle, Metadata,
    OpenMode, Result, StorageBackend, VfsPath,
};

/// An SMB endpoint split into its parts.
///
/// The format follows libsmb2's own URL grammar — `lib/init.c:246-289` — which
/// is `[domain;][user@]server/share`. Parsing it here rather than handing the
/// whole string to the library keeps the share concept on this side of the
/// abstraction boundary: `krystallos-core` never learns that SMB has shares.
#[derive(Debug, PartialEq, Eq)]
struct ParsedEndpoint {
    server: String,
    share: String,
    /// User named in the URI. Credentials passed separately take precedence.
    user: Option<String>,
    domain: Option<String>,
}

impl ParsedEndpoint {
    fn parse(authority_and_path: &str) -> Result<Self> {
        let (authority, rest) = authority_and_path.split_once('/').ok_or_else(|| {
            Error::InvalidPath {
                path: authority_and_path.to_string(),
                reason: "an SMB endpoint must name a share, e.g. `smb://host/share`".to_string(),
            }
        })?;

        // A share is the first component after the host; anything beyond it is
        // a path within the share, which the backend addresses separately.
        //
        // The emptiness check has to come *after* taking that component, not
        // before: for `smb://host//x` the remainder is `/x`, which is not empty
        // but whose first component is. Checking earlier lets an empty share
        // through and turns it into a confusing failure inside libsmb2.
        let share = rest.split('/').next().unwrap_or("");
        if share.is_empty() {
            return Err(Error::InvalidPath {
                path: authority_and_path.to_string(),
                reason: "the share name is empty".to_string(),
            });
        }

        // `domain;` comes first and is optional.
        let (domain, rest) = match authority.split_once(';') {
            Some((d, rest)) => (Some(d.to_string()), rest),
            None => (None, authority),
        };

        // Then `user@`, also optional.
        let (user, server) = match rest.split_once('@') {
            Some((u, s)) => (Some(u.to_string()), s),
            None => (None, rest),
        };

        if server.is_empty() {
            return Err(Error::InvalidPath {
                path: authority_and_path.to_string(),
                reason: "the server name is empty".to_string(),
            });
        }

        Ok(ParsedEndpoint {
            server: server.to_string(),
            share: share.to_string(),
            user: user.filter(|u| !u.is_empty()),
            domain: domain.filter(|d| !d.is_empty()),
        })
    }

    /// Canonical endpoint string, without a trailing slash, for
    /// [`VfsPath::to_uri`](krystallos_core::VfsPath::to_uri).
    fn label(&self) -> String {
        format!("smb://{}/{}", self.server, self.share)
    }
}

/// A connected SMB share.
///
/// # Scope
///
/// Path-based operations only. `open` and the file handle it would return are
/// not implemented yet — a handle has to outlive a single command, which needs
/// its own lifetime design on the session thread. Until that lands, `open`
/// reports [`Error::Unsupported`] rather than pretending.
pub struct SmbBackend {
    session: Session,
    endpoint: String,
}

#[async_trait]
impl StorageBackend for SmbBackend {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> Capabilities {
        // What SMB itself supports. Note that `random_read` and `random_write`
        // describe the protocol: SMB2 has positioned reads and writes, and
        // libsmb2 exposes them as `smb2_pread`/`smb2_pwrite`.
        Capabilities::FULL
    }

    async fn list(&self, path: &VfsPath) -> Result<Vec<Entry>> {
        self.session.list(path).await
    }

    async fn stat(&self, path: &VfsPath) -> Result<Metadata> {
        self.session.stat(path).await
    }

    async fn open(&self, _path: &VfsPath, _mode: OpenMode) -> Result<Box<dyn FileHandle>> {
        Err(Error::Unsupported {
            operation: "open — SMB file I/O is not implemented yet",
        })
    }

    async fn remove_file(&self, path: &VfsPath) -> Result<()> {
        self.session.remove_file(path).await
    }

    async fn remove_dir(&self, path: &VfsPath) -> Result<()> {
        self.session.remove_dir(path).await
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.session.rename(from, to).await
    }

    async fn mkdir(&self, path: &VfsPath) -> Result<()> {
        self.session.mkdir(path).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.session.shutdown().await
    }
}

/// Opens [`SmbBackend`] sessions for `smb://` endpoints.
#[derive(Debug, Default, Clone, Copy)]
pub struct SmbDriver;

impl SmbDriver {
    pub fn new() -> Self {
        SmbDriver
    }
}

#[async_trait]
impl BackendDriver for SmbDriver {
    fn scheme(&self) -> &'static str {
        "smb"
    }

    fn description(&self) -> &'static str {
        "SMB2/3 share"
    }

    async fn connect(
        &self,
        endpoint: &Endpoint,
        credentials: &Credentials,
    ) -> Result<Box<dyn StorageBackend>> {
        let parsed = ParsedEndpoint::parse(endpoint.authority_and_path())?;

        // Credentials passed explicitly win over anything embedded in the URI.
        // The URI form exists for convenience; the struct form is what callers
        // use when they care about not leaking a password into logs.
        let config = ConnectConfig {
            server: parsed.server.clone(),
            share: parsed.share.clone(),
            user: credentials
                .username
                .clone()
                .or_else(|| parsed.user.clone()),
            password: credentials.password.clone(),
            domain: credentials.domain.clone().or_else(|| parsed.domain.clone()),
            // SMB3 encryption is requested when the caller supplied a password.
            // Asking for it with no credentials would fail the handshake on
            // servers that then require it.
            seal: credentials.password.is_some(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            label: parsed.label(),
        };

        let session = Session::connect(config).await?;
        Ok(Box::new(SmbBackend {
            session,
            endpoint: parsed.label(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<ParsedEndpoint> {
        ParsedEndpoint::parse(s)
    }

    #[test]
    fn parses_a_bare_server_and_share() {
        let p = parse("nas.local/media").unwrap();
        assert_eq!(p.server, "nas.local");
        assert_eq!(p.share, "media");
        assert_eq!(p.user, None);
        assert_eq!(p.domain, None);
    }

    #[test]
    fn parses_user_and_domain() {
        let p = parse("WORKGROUP;alice@nas.local/media").unwrap();
        assert_eq!(p.server, "nas.local");
        assert_eq!(p.share, "media");
        assert_eq!(p.user.as_deref(), Some("alice"));
        assert_eq!(p.domain.as_deref(), Some("WORKGROUP"));
    }

    #[test]
    fn parses_user_without_domain() {
        let p = parse("b@127.0.0.1/krystallos-test").unwrap();
        assert_eq!(p.server, "127.0.0.1");
        assert_eq!(p.share, "krystallos-test");
        assert_eq!(p.user.as_deref(), Some("b"));
    }

    #[test]
    fn an_ipv4_literal_is_not_mistaken_for_a_domain_separator() {
        // The `;` is the only domain separator; dots and colons in a host must
        // survive untouched.
        let p = parse("192.168.1.10/share").unwrap();
        assert_eq!(p.server, "192.168.1.10");
        assert_eq!(p.domain, None);
    }

    #[test]
    fn a_path_within_the_share_still_yields_the_share_name() {
        // The endpoint names the share; deeper paths are addressed as VfsPaths.
        let p = parse("nas.local/media/movies/2024").unwrap();
        assert_eq!(p.share, "media");
    }

    #[test]
    fn rejects_endpoints_with_no_share() {
        for bad in ["nas.local", "nas.local/", "nas.local//x"] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_an_empty_server() {
        // `smb:///share` — authority is empty.
        assert!(parse("/share").is_err());
    }

    #[test]
    fn empty_user_and_domain_components_are_dropped() {
        let p = parse(";@nas.local/media").unwrap();
        assert_eq!(p.user, None);
        assert_eq!(p.domain, None);
    }

    #[test]
    fn label_is_canonical_and_has_no_trailing_slash() {
        assert_eq!(parse("nas.local/media").unwrap().label(), "smb://nas.local/media");
        // Credentials are deliberately not echoed back into the label: it ends
        // up in log lines and error messages.
        assert_eq!(
            parse("b@127.0.0.1/share").unwrap().label(),
            "smb://127.0.0.1/share"
        );
    }

    #[test]
    fn the_driver_declares_its_scheme() {
        assert_eq!(SmbDriver::new().scheme(), "smb");
        assert!(!SmbDriver::new().description().is_empty());
    }
}

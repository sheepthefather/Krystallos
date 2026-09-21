use crate::{Error, Result};
use std::fmt;

/// A normalized, absolute path inside a single backend.
///
/// `VfsPath` is intentionally *not* a general filesystem path. It has no notion
/// of a volume, a drive, or a share: those are backend-specific concepts that
/// the backend consumes when it parses its own endpoint URI. Everything above
/// the backend sees only paths of this shape.
///
/// # Invariants
///
/// Enforced by [`VfsPath::new`], the only way to build a non-root path:
///
/// - always begins with `/`
/// - no empty segments (`//` is collapsed)
/// - no `.` segments
/// - no `..` segments — they are resolved during normalization, and a `..`
///   that would escape the root is rejected rather than clamped
/// - no trailing `/`, except for the root itself
///
/// Rejecting rather than clamping is deliberate: a path that tries to escape
/// the root is a caller bug or an injection attempt, and silently rewriting it
/// to `/` would hide that.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VfsPath(String);

impl VfsPath {
    /// The root of the backend's namespace.
    pub fn root() -> Self {
        VfsPath("/".to_string())
    }

    /// Normalize `raw` into a `VfsPath`.
    ///
    /// `raw` may be relative or absolute; a leading `/` is optional. Both `\`
    /// and `/` are accepted as separators on input, because Windows-flavoured
    /// paths show up in user input constantly. The output always uses `/`.
    pub fn new(raw: &str) -> Result<Self> {
        let mut segments: Vec<&str> = Vec::new();

        for segment in raw.split(['/', '\\']) {
            match segment {
                // Empty runs of separators, and `.`, contribute nothing.
                "" | "." => continue,
                ".." => {
                    if segments.pop().is_none() {
                        return Err(Error::InvalidPath {
                            path: raw.to_string(),
                            reason: "path escapes the backend root".to_string(),
                        });
                    }
                }
                s => segments.push(s),
            }
        }

        if segments.is_empty() {
            return Ok(VfsPath::root());
        }

        let mut out = String::with_capacity(raw.len() + 1);
        for s in &segments {
            out.push('/');
            out.push_str(s);
        }
        Ok(VfsPath(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.len() == 1
    }

    /// The path's segments, excluding the leading empty root segment.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    /// The final segment, or `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            return None;
        }
        self.0.rsplit('/').next()
    }

    /// The containing path. The root's parent is the root.
    pub fn parent(&self) -> VfsPath {
        if self.is_root() {
            return VfsPath::root();
        }
        match self.0.rfind('/') {
            Some(0) | None => VfsPath::root(),
            Some(i) => VfsPath(self.0[..i].to_string()),
        }
    }

    /// Append a name, or a relative subpath, to this path.
    ///
    /// `name` may contain separators, so `join("a/b")` works, and it is
    /// normalized like any other input.
    ///
    /// `..` is **rejected rather than resolved**, which is the one place this
    /// differs from [`VfsPath::new`]. `join` is the call to reach for when
    /// building a path out of a name that came from the server (a directory
    /// entry) or from the user (a search box). A name must not be able to
    /// redirect the operation upward — `parent().join("..")` silently becoming
    /// the grandparent is exactly the kind of surprise that turns into a
    /// data-loss bug during a delete or a rename. When upward traversal is
    /// genuinely intended, say so by constructing the whole path with
    /// [`VfsPath::new`], where the `..` is visible at the call site.
    ///
    /// A `name` that is empty, `.`, or otherwise leaves the path unchanged is
    /// also rejected: a caller asking to join nothing is almost certainly a bug.
    pub fn join(&self, name: &str) -> Result<VfsPath> {
        if name.split(['/', '\\']).any(|s| s == "..") {
            return Err(Error::InvalidPath {
                path: name.to_string(),
                reason: "`..` is not allowed in a joined name; build the full path with \
                         VfsPath::new if you mean to traverse upward"
                    .to_string(),
            });
        }

        let mut out = self.0.clone();
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(name);
        let joined = VfsPath::new(&out)?;
        if joined == *self {
            return Err(Error::InvalidPath {
                path: name.to_string(),
                reason: "joining this name does not change the path".to_string(),
            });
        }
        Ok(joined)
    }

    /// Attach the backend this path belongs to, producing a URI.
    ///
    /// Used for display and for round-tripping through the registry. The result
    /// is `<endpoint><path>`, where `endpoint` is expected to have no trailing
    /// slash, e.g. `smb://host/share` + `/movies/a.mkv`.
    pub fn to_uri(&self, endpoint: &str) -> String {
        format!("{}{}", endpoint.trim_end_matches('/'), self.0)
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lets a path be used anywhere a `&str` is wanted, which is most places —
/// every backend needs a string form to hand to its protocol library.
impl AsRef<str> for VfsPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VfsPath({:?})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VfsPath {
        VfsPath::new(s).expect("should normalize")
    }

    #[test]
    fn root_forms_all_collapse_to_root() {
        for raw in ["", "/", "//", ".", "/.", "///./", "\\"] {
            assert!(p(raw).is_root(), "{raw:?} should be root");
            assert_eq!(p(raw).as_str(), "/");
        }
    }

    #[test]
    fn duplicate_and_trailing_separators_collapse() {
        assert_eq!(p("/a//b///c/").as_str(), "/a/b/c");
        assert_eq!(p("a/b").as_str(), "/a/b");
    }

    #[test]
    fn backslashes_are_treated_as_separators() {
        assert_eq!(p(r"\a\b\c").as_str(), "/a/b/c");
        assert_eq!(p(r"/a\b/c").as_str(), "/a/b/c");
    }

    #[test]
    fn dot_segments_are_dropped() {
        assert_eq!(p("/a/./b/.").as_str(), "/a/b");
        assert_eq!(p("./a").as_str(), "/a");
    }

    #[test]
    fn dotdot_is_resolved_within_the_root() {
        assert_eq!(p("/a/b/../c").as_str(), "/a/c");
        assert_eq!(p("/a/b/../../c").as_str(), "/c");
        assert_eq!(p("/a/..").as_str(), "/");
    }

    #[test]
    fn dotdot_escaping_the_root_is_rejected() {
        for raw in ["/..", "/../a", "/a/../../b"] {
            let err = VfsPath::new(raw).unwrap_err();
            match err {
                Error::InvalidPath { reason, .. } => assert!(reason.contains("escapes")),
                other => panic!("expected InvalidPath for {raw:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn file_name_and_parent() {
        assert_eq!(p("/a/b/c").file_name(), Some("c"));
        assert_eq!(p("/a").file_name(), Some("a"));
        assert_eq!(VfsPath::root().file_name(), None);

        assert_eq!(p("/a/b/c").parent().as_str(), "/a/b");
        assert_eq!(p("/a").parent().as_str(), "/");
        assert_eq!(VfsPath::root().parent().as_str(), "/");
    }

    #[test]
    fn segments_skips_the_root_marker() {
        assert_eq!(p("/a/b/c").segments().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(VfsPath::root().segments().count(), 0);
    }

    #[test]
    fn join_appends_and_normalizes() {
        let base = p("/a/b");
        assert_eq!(base.join("c").unwrap().as_str(), "/a/b/c");
        assert_eq!(base.join("c/d").unwrap().as_str(), "/a/b/c/d");
        assert_eq!(base.join("./c//d").unwrap().as_str(), "/a/b/c/d");
        assert_eq!(base.join(r"c\d").unwrap().as_str(), "/a/b/c/d");
        assert_eq!(VfsPath::root().join("a").unwrap().as_str(), "/a");
    }

    #[test]
    fn join_rejects_names_that_leave_the_path_unchanged() {
        let base = p("/a/b");
        for name in ["", ".", "/", "./"] {
            assert!(
                base.join(name).is_err(),
                "joining {name:?} should be rejected as a no-op"
            );
        }
    }

    #[test]
    fn join_refuses_dotdot_rather_than_traversing_upward() {
        // The regression this guards: `/a/b`.join("..") used to resolve to
        // `/a`, letting a server-supplied name walk the path upward during a
        // delete or rename.
        let base = p("/a/b");
        for name in ["..", "../c", "c/..", "c/../..", r"..\c"] {
            let err = base.join(name).unwrap_err();
            match err {
                Error::InvalidPath { reason, .. } => {
                    assert!(
                        reason.contains(".."),
                        "reason should name the offending segment: {reason}"
                    );
                }
                other => panic!("expected InvalidPath for {name:?}, got {other:?}"),
            }
        }
        assert_eq!(base.as_str(), "/a/b", "base must be untouched");
    }

    #[test]
    fn join_cannot_escape_the_root() {
        assert!(p("/a").join("../..").is_err());
        assert!(VfsPath::root().join("..").is_err());
    }

    #[test]
    fn upward_traversal_is_still_available_through_new() {
        // Rejecting `..` in `join` must not make traversal impossible — it just
        // makes it explicit at the call site.
        assert_eq!(p("/a/b/..").as_str(), "/a");
        assert_eq!(p("/a/b/../c").as_str(), "/a/c");
    }

    #[test]
    fn to_uri_attaches_the_endpoint() {
        assert_eq!(p("/a/b").to_uri("smb://host/share"), "smb://host/share/a/b");
        assert_eq!(p("/a/b").to_uri("smb://host/share/"), "smb://host/share/a/b");
        assert_eq!(VfsPath::root().to_uri("file:///d:"), "file:///d:/");
    }

    #[test]
    fn unicode_names_survive_normalization() {
        // Names are carried through byte-for-byte. Normalizing Unicode form is
        // the backend's business, not ours — see ARCHITECTURE.md.
        let path = p("/影片/第 1 集.mkv");
        assert_eq!(path.file_name(), Some("第 1 集.mkv"));
        assert_eq!(path.parent().as_str(), "/影片");
    }
}

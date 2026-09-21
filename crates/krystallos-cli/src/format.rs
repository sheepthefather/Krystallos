use krystallos_core::{Entry, EntryKind, Metadata};

/// Render one directory entry as a fixed-width line.
///
/// Fixed columns on purpose: the output is meant to be eyeballed side by side
/// with a Windows Explorer window during verification, and a stable layout
/// makes a mismatch obvious.
pub fn entry_line(entry: &Entry) -> String {
    format!(
        "{} {:>12} {}  {}",
        kind_char(entry.metadata.kind),
        entry.metadata.len,
        time_cell(entry.metadata.modified),
        entry.name
    )
}

pub fn metadata_block(path: &str, meta: &Metadata) -> String {
    let mut out = String::new();
    out.push_str(&format!("path:     {path}\n"));
    out.push_str(&format!("kind:     {:?}\n", meta.kind));
    out.push_str(&format!("size:     {}\n", meta.len));
    out.push_str(&format!("modified: {}\n", time_cell(meta.modified)));
    out.push_str(&format!("created:  {}\n", time_cell(meta.created)));
    out.push_str(&format!("accessed: {}\n", time_cell(meta.accessed)));
    out.push_str(&format!("readonly: {}\n", meta.read_only));
    out
}

pub fn kind_char(kind: EntryKind) -> char {
    match kind {
        EntryKind::Directory => 'd',
        EntryKind::File => 'f',
        EntryKind::Symlink => 'l',
        EntryKind::Other => '?',
    }
}

/// Times are RFC 3339 in UTC. Backends routinely cannot report a timestamp —
/// a missing value is rendered as `-` rather than as the Unix epoch, so that
/// "no information" stays distinguishable from "1970".
fn time_cell(t: Option<std::time::SystemTime>) -> String {
    match t {
        Some(t) => humantime::format_rfc3339_seconds(t).to_string(),
        None => "-".to_string(),
    }
}

/// Human-readable byte count for progress output.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn entry_lines_align_on_stable_columns() {
        let e = Entry::new("movie.mkv", {
            let mut m = Metadata::file(1_073_741_824);
            m.modified = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
            m
        });
        let line = entry_line(&e);
        assert!(line.starts_with("f   1073741824 "), "got: {line}");
        assert!(line.ends_with("  movie.mkv"), "got: {line}");
    }

    #[test]
    fn a_missing_timestamp_renders_as_a_dash_not_as_the_epoch() {
        let e = Entry::new("x", Metadata::file(0));
        assert!(entry_line(&e).contains(" -  "), "got: {}", entry_line(&e));
    }

    #[test]
    fn kind_char_covers_every_kind() {
        assert_eq!(kind_char(EntryKind::Directory), 'd');
        assert_eq!(kind_char(EntryKind::File), 'f');
        assert_eq!(kind_char(EntryKind::Symlink), 'l');
        assert_eq!(kind_char(EntryKind::Other), '?');
    }

    #[test]
    fn human_bytes_switches_units_at_the_right_points() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn metadata_block_labels_every_field() {
        let block = metadata_block("/a/b", &Metadata::file(42));
        for label in [
            "path:", "kind:", "size:", "modified:", "created:", "accessed:", "readonly:",
        ] {
            assert!(block.contains(label), "missing {label} in:\n{block}");
        }
        assert!(block.contains("size:     42"));
    }
}

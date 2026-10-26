//! Number formatting for the UI.

/// Bytes in Explorer style: 1024-based, one decimal from KB up ("812 B", "1.4 MB", "2.0 TB").
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// 1234567 → "1,234,567".
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Percentage with no decimals, except below 1% and above 99% where it matters.
pub fn percent(p: f64) -> String {
    if p <= 0.0 {
        "0%".into()
    } else if p < 1.0 || (p > 99.0 && p < 100.0) {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

/// A source name for display: the export scripts write "PC C: ()" for a drive without a volume
/// label; the empty brackets are dropped. (The stored name is unchanged.)
pub fn host(h: &str) -> &str {
    h.strip_suffix(" ()").unwrap_or(h)
}

/// "3 files" / "1 file".
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", count(n), if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KB");
        assert_eq!(bytes(1_536_000), "1.5 MB");
        assert_eq!(bytes(250 * 1024 * 1024 * 1024), "250 GB");
        assert_eq!(bytes(3 * 1024u64.pow(4)), "3.0 TB");
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.42), "0.4%");
        assert_eq!(percent(42.4), "42%");
        assert_eq!(percent(99.6), "99.6%");
        assert_eq!(percent(100.0), "100%");
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(plural(2000, "file", "files"), "2,000 files");
        assert_eq!(host("PC C: ()"), "PC C:");
        assert_eq!(host("PC D: (Media)"), "PC D: (Media)");
        assert_eq!(host("gdrive:"), "gdrive:");
    }
}

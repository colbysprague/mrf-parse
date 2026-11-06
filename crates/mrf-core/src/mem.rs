//! Process memory reporting.

/// Peak resident set size of this process so far, in bytes, if the
/// platform reports it. Uses `getrusage(RUSAGE_SELF)` via `nix` (a safe
/// wrapper; this workspace forbids `unsafe`). Note `ru_maxrss` is in bytes
/// on macOS but in kilobytes on Linux and the BSDs.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        use nix::sys::resource::{getrusage, UsageWho};
        let max = getrusage(UsageWho::RUSAGE_SELF).ok()?.max_rss();
        let max = u64::try_from(max).ok()?;
        if cfg!(target_os = "macos") {
            Some(max)
        } else {
            Some(max * 1024)
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Human-readable byte count (`12.3MB`).
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_is_plausible() {
        if let Some(b) = peak_rss_bytes() {
            assert!(b > 1024 * 1024, "{b}");
            assert!(b < 64 * 1024 * 1024 * 1024, "{b}");
        }
    }

    #[test]
    fn human() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0GB");
    }
}

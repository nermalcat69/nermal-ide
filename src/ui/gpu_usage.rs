//! Best-effort system GPU utilization, for the Usage tab.
//!
//! There is no cross-platform equivalent of `sysinfo` for this: none of the
//! three platforms expose per-process GPU time to an unprivileged reader, and
//! even whole-system utilization comes from a different place on each one.
//! This tries whatever local signal exists and returns `None` rather than a
//! guess when nothing does — the Usage tab hides the row entirely in that
//! case instead of showing a number that isn't real.

/// Percent utilization of the busiest GPU on this machine, `0.0..=100.0`.
pub(crate) fn sample() -> Option<f32> {
    if let Some(v) = nvidia_smi() {
        return Some(v);
    }
    #[cfg(target_os = "macos")]
    {
        return macos_ioreg();
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Covers a discrete NVIDIA card on any of the three platforms — the one
/// vendor with a CLI that ships alongside its driver rather than needing a
/// separate install.
fn nvidia_smi() -> Option<f32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse::<f32>()
        .ok()
}

/// Apple Silicon and Intel/AMD Macs each report GPU load under a differently
/// named key of the same `IOAccelerator` node — `ioreg` is the one command
/// that reads it without `sudo`.
#[cfg(target_os = "macos")]
fn macos_ioreg() -> Option<f32> {
    let out = std::process::Command::new("ioreg")
        .args(["-r", "-d", "1", "-c", "IOAccelerator"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for key in ["\"Device Utilization %\"", "\"GPU Activity(%)\""] {
        if let Some(v) = number_after(&text, key) {
            return Some(v);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn number_after(text: &str, key: &str) -> Option<f32> {
    let after = &text[text.find(key)? + key.len()..];
    let rest = after[after.find('=')? + 1..].trim_start();
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn number_after_reads_the_value_following_the_key() {
        let text = r#"| "Device Utilization %" = 42"#;
        assert_eq!(number_after(text, "\"Device Utilization %\""), Some(42.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn number_after_is_none_when_the_key_is_absent() {
        assert_eq!(number_after("nothing here", "\"Device Utilization %\""), None);
    }
}

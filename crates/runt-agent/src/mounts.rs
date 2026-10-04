//! Shared folders: virtio-fs filesystems from the host, mounted at boot.
//!
//! The supervisor passes `runt.fs=<tag>:<hex(path)>[:ro],...` on the kernel
//! command line. Paths are hex-encoded so any byte sequence survives.

use std::io;

#[derive(Debug, Clone, PartialEq)]
pub struct GuestMount {
    pub tag: String,
    pub dst: String,
    pub read_only: bool,
}

/// Parse the value of `runt.fs`. Malformed entries are skipped (and logged
/// by the caller via the returned errors).
pub fn parse(value: &str) -> (Vec<GuestMount>, Vec<String>) {
    let mut mounts = Vec::new();
    let mut errors = Vec::new();
    for entry in value.split(',').filter(|e| !e.is_empty()) {
        match parse_entry(entry) {
            Some(m) => mounts.push(m),
            None => errors.push(format!("bad runt.fs entry {entry:?}")),
        }
    }
    (mounts, errors)
}

fn parse_entry(entry: &str) -> Option<GuestMount> {
    let mut parts = entry.split(':');
    let tag = parts.next()?;
    let dst = String::from_utf8(hex_decode(parts.next()?)?).ok()?;
    let read_only = match parts.next() {
        None => false,
        Some("ro") => true,
        Some(_) => return None,
    };
    if parts.next().is_some()
        || tag.is_empty()
        || !tag.bytes().all(|b| b.is_ascii_alphanumeric())
        || !dst.starts_with('/')
        || dst.split('/').any(|c| c == "..")
    {
        return None;
    }
    Some(GuestMount {
        tag: tag.into(),
        dst,
        read_only,
    })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Mount every share. Failures are reported but don't stop the others.
pub fn mount_all(mounts: &[GuestMount]) {
    for m in mounts {
        if let Err(e) = mount_one(m) {
            eprintln!(
                "runt-agent: warning: cannot mount {} at {}: {e}",
                m.tag, m.dst
            );
        }
    }
}

fn mount_one(m: &GuestMount) -> io::Result<()> {
    crate::boot::mkdir_p(&m.dst)?;
    let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
    if m.read_only {
        flags |= libc::MS_RDONLY;
    }
    crate::boot::mount(&m.tag, &m.dst, "virtiofs", flags, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn parses_entries() {
        let v = format!(
            "runt0:{},runt1:{}:ro",
            hex("/home/joe/src/app"),
            hex("/data dir")
        );
        let (m, errs) = parse(&v);
        assert!(errs.is_empty());
        assert_eq!(
            m,
            vec![
                GuestMount {
                    tag: "runt0".into(),
                    dst: "/home/joe/src/app".into(),
                    read_only: false
                },
                GuestMount {
                    tag: "runt1".into(),
                    dst: "/data dir".into(),
                    read_only: true
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_entries() {
        for bad in [
            format!("runt0:{}:rw", hex("/x")),
            format!("runt0:{}", hex("relative")),
            format!("runt0:{}", hex("/a/../b")),
            format!("bad tag:{}", hex("/x")),
            "runt0:zz".to_string(),
            "runt0:abc".to_string(),
        ] {
            let (m, errs) = parse(&bad);
            assert!(m.is_empty() && errs.len() == 1, "{bad} should be rejected");
        }
    }
}

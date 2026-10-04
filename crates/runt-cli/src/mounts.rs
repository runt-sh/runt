//! Shared folders: `--mount SRC[:DST][:ro]` parsing and validation, the
//! kernel cmdline encoding the guest agent reads, and mapping the host's
//! working directory into the VM.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, Result};

pub const MAX_MOUNTS: usize = 16;

/// x86 kernel command lines are limited to 2048 bytes; leave headroom for
/// what libkrun appends (virtio-mmio device descriptions).
const MAX_CMDLINE: usize = 1536;

/// Guest paths a share must not cover: the guest needs its own.
const RESERVED: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/proc", "/run",
    "/sbin", "/sys", "/usr", "/var",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mount {
    /// Canonical host directory.
    pub src: PathBuf,
    /// Absolute path inside the VM.
    pub dst: PathBuf,
    #[serde(default)]
    pub read_only: bool,
}

/// Parse one `--mount` value. Relative SRCs resolve against `cwd`.
pub fn parse(spec: &str, cwd: &Path) -> Result<Mount> {
    let bad = |why: &str| {
        CliError::new("invalid_mount", format!("invalid --mount {spec:?}: {why}"))
            .hint("use --mount SRC[:DST][:ro], e.g. --mount . or --mount ~/data:/data:ro")
    };
    let (rest, read_only) = match spec.strip_suffix(":ro") {
        Some(r) => (r, true),
        None => (spec.strip_suffix(":rw").unwrap_or(spec), false),
    };
    let (src, dst) = match rest.split_once(':') {
        Some((s, d)) => (s, Some(d)),
        None => (rest, None),
    };
    if src.is_empty() {
        return Err(bad("missing source directory"));
    }
    let src = expand_home(src);
    let src = cwd.join(src);
    let src = src
        .canonicalize()
        .map_err(|e| bad(&format!("{}: {e}", src.display())))?;
    if !src.is_dir() {
        return Err(bad(&format!("{} is not a directory", src.display())));
    }
    if src == Path::new("/") {
        return Err(bad("sharing the host's root directory is not allowed"));
    }
    let dst = match dst {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => src.clone(),
    };
    validate_dst(&dst).map_err(|why| bad(&why))?;
    Ok(Mount {
        src,
        dst,
        read_only,
    })
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if p == "~" => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| p.into()),
        _ => PathBuf::from(p),
    }
}

fn validate_dst(dst: &Path) -> std::result::Result<(), String> {
    if !dst.is_absolute() {
        return Err(format!("{} must be an absolute path", dst.display()));
    }
    if dst
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("{} must not contain . or ..", dst.display()));
    }
    if dst == Path::new("/") {
        return Err("cannot mount over the VM's root directory".into());
    }
    for r in RESERVED {
        if dst.starts_with(r) {
            return Err(format!("{} is reserved for the VM's own system files", r));
        }
    }
    Ok(())
}

/// Check a set of mounts for conflicts.
pub fn validate_set(mounts: &[Mount]) -> Result<()> {
    if mounts.len() > MAX_MOUNTS {
        return Err(CliError::new(
            "too_many_mounts",
            format!("at most {MAX_MOUNTS} mounts are supported"),
        ));
    }
    for (i, a) in mounts.iter().enumerate() {
        for b in &mounts[i + 1..] {
            if a.dst.starts_with(&b.dst) || b.dst.starts_with(&a.dst) {
                return Err(CliError::new(
                    "invalid_mount",
                    format!(
                        "mounts overlap inside the VM: {} and {}",
                        a.dst.display(),
                        b.dst.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// virtio-fs tag for the n-th mount.
pub fn tag(i: usize) -> String {
    format!("runt{i}")
}

/// `runt.fs=...` for the guest kernel command line, or None without mounts.
pub fn cmdline(mounts: &[Mount]) -> Option<String> {
    if mounts.is_empty() {
        return None;
    }
    let entries: Vec<String> = mounts
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let hex: String = m
                .dst
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{}:{hex}{}", tag(i), if m.read_only { ":ro" } else { "" })
        })
        .collect();
    Some(format!("runt.fs={}", entries.join(",")))
}

/// Fail early if the guest command line would be too long.
pub fn check_cmdline_len(cmdline: &str) -> Result<()> {
    if cmdline.len() > MAX_CMDLINE {
        return Err(CliError::new(
            "mounts_too_long",
            "the mount paths are too long to pass to the VM",
        )
        .hint("use fewer or shorter mount paths (set DST explicitly, e.g. --mount ~/very/long/path:/src)"));
    }
    Ok(())
}

/// Where `runt exec` should start inside the VM when the user didn't say:
/// the same place as on the host, if the host's cwd is inside a mount.
pub fn default_workdir(cwd: &Path, mounts: &[Mount]) -> Option<PathBuf> {
    mounts.iter().find_map(|m| {
        let rel = cwd.strip_prefix(&m.src).ok()?;
        Some(if rel.as_os_str().is_empty() {
            m.dst.clone()
        } else {
            m.dst.join(rel)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("runt-mounts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn parses_specs() {
        let d = tmpdir("parse");
        let m = parse(".", &d).unwrap();
        assert_eq!(
            (m.src.clone(), m.dst.clone(), m.read_only),
            (d.clone(), d.clone(), false)
        );

        let m = parse(&format!("{}:/data:ro", d.display()), Path::new("/")).unwrap();
        assert_eq!((m.dst, m.read_only), (PathBuf::from("/data"), true));

        let m = parse(".:/src:rw", &d).unwrap();
        assert_eq!((m.dst, m.read_only), (PathBuf::from("/src"), false));

        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(parse("sub", &d).unwrap().src, sub);
    }

    #[test]
    fn rejects_bad_specs() {
        let d = tmpdir("bad");
        let file = d.join("f");
        std::fs::write(&file, "").unwrap();
        for spec in [
            "",
            ":/x",
            "does-not-exist",
            "f",
            "/",
            ".:relative",
            ".:/",
            ".:/etc",
            ".:/usr/local/x",
            ".:/a/../b",
        ] {
            assert!(parse(spec, &d).is_err(), "{spec:?} should be rejected");
        }
    }

    #[test]
    fn rejects_overlaps() {
        let m = |dst: &str| Mount {
            src: "/tmp".into(),
            dst: dst.into(),
            read_only: false,
        };
        assert!(validate_set(&[m("/a"), m("/b")]).is_ok());
        assert!(validate_set(&[m("/a"), m("/a/b")]).is_err());
        assert!(validate_set(&[m("/ab"), m("/a")]).is_ok());
    }

    #[test]
    fn encodes_cmdline() {
        let mounts = vec![
            Mount {
                src: "/h".into(),
                dst: "/a b".into(),
                read_only: false,
            },
            Mount {
                src: "/h2".into(),
                dst: "/d".into(),
                read_only: true,
            },
        ];
        assert_eq!(
            cmdline(&mounts).unwrap(),
            "runt.fs=runt0:2f612062,runt1:2f64:ro"
        );
        assert!(cmdline(&[]).is_none());
        assert!(check_cmdline_len(&"x".repeat(100)).is_ok());
        assert!(check_cmdline_len(&"x".repeat(5000)).is_err());
    }

    #[test]
    fn maps_workdir() {
        let mounts = vec![Mount {
            src: "/home/joe/src/app".into(),
            dst: "/work".into(),
            read_only: false,
        }];
        assert_eq!(
            default_workdir(Path::new("/home/joe/src/app"), &mounts),
            Some("/work".into())
        );
        assert_eq!(
            default_workdir(Path::new("/home/joe/src/app/web/src"), &mounts),
            Some("/work/web/src".into())
        );
        assert_eq!(
            default_workdir(Path::new("/home/joe/src/other"), &mounts),
            None
        );
        assert_eq!(
            default_workdir(Path::new("/home/joe/src/app2"), &mounts),
            None
        );
    }
}

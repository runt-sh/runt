//! Deterministic tar archives of build inputs. Entries are sorted, owned by
//! root and stamped with a fixed time, so the same files always produce the
//! same bytes, and so the same cache key, wherever and whenever they were
//! checked out.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 1980-01-01: the earliest time zip files can store, so tools that zip
/// copied files (Python wheels, for one) don't choke on them.
pub const MTIME: u64 = 315_532_800;

const BLOCK: usize = 512;

/// What [`write`] put in the archive.
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub files: u64,
    pub bytes: u64,
    /// Sockets, FIFOs and devices, which are left out.
    pub skipped: Vec<String>,
}

/// Archive `paths` (relative to `root`, each a file or a directory taken
/// recursively) under their relative names, leaving out anything matching
/// `exclude`. Symlinks are stored as links, never followed.
pub fn write(
    root: &Path,
    paths: &[String],
    exclude: &[String],
    out: &mut dyn Write,
) -> io::Result<Summary> {
    let mut sum = Summary::default();
    let mut names: Vec<String> = paths.iter().map(|p| normalize(p)).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        if name.is_empty() {
            walk_dir(root, "", exclude, out, &mut sum)?;
        } else {
            let meta = fs::symlink_metadata(root.join(&name))
                .map_err(|e| io::Error::new(e.kind(), format!("{name}: {e}")))?;
            entry(root, &name, &meta, exclude, out, &mut sum)?;
        }
    }
    out.write_all(&[0u8; 2 * BLOCK])?;
    Ok(sum)
}

/// `./a//b/` -> `a/b`; `.` -> ``.
fn normalize(p: &str) -> String {
    p.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn walk_dir(
    root: &Path,
    rel: &str,
    exclude: &[String],
    out: &mut dyn Write,
    sum: &mut Summary,
) -> io::Result<()> {
    let mut children: Vec<_> = fs::read_dir(root.join(rel))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    children.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    for c in children {
        let Some(c) = c.to_str() else {
            sum.skipped.push(format!("{rel}/{}", c.to_string_lossy()));
            continue;
        };
        let name = if rel.is_empty() {
            c.to_string()
        } else {
            format!("{rel}/{c}")
        };
        let meta = fs::symlink_metadata(root.join(&name))?;
        entry(root, &name, &meta, exclude, out, sum)?;
    }
    Ok(())
}

fn entry(
    root: &Path,
    name: &str,
    meta: &fs::Metadata,
    exclude: &[String],
    out: &mut dyn Write,
    sum: &mut Summary,
) -> io::Result<()> {
    if excluded(name, exclude) {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o7777;
    let ft = meta.file_type();
    if ft.is_dir() {
        out.write_all(&header(&format!("{name}/"), mode, 0, b'5', "")?)?;
        walk_dir(root, name, exclude, out, sum)
    } else if ft.is_symlink() {
        let target = fs::read_link(root.join(name))?;
        let target = target.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name}: link target isn't UTF-8"),
            )
        })?;
        out.write_all(&header(name, 0o777, 0, b'2', target)?)?;
        sum.files += 1;
        Ok(())
    } else if ft.is_file() {
        let size = meta.len();
        out.write_all(&header(name, mode, size, b'0', "")?)?;
        // Write exactly `size` bytes even if the file changes under us, so
        // the archive stays well-formed.
        let mut f = File::open(root.join(name))?.take(size);
        let copied = io::copy(&mut f, out)?;
        let mut pad = (size - copied) as usize;
        pad += (BLOCK - (size as usize % BLOCK)) % BLOCK;
        out.write_all(&vec![0u8; pad])?;
        sum.files += 1;
        sum.bytes += size;
        Ok(())
    } else {
        sum.skipped.push(name.to_string());
        Ok(())
    }
}

/// gitignore-style matching, simplified: a pattern without a slash matches
/// any file or directory with that name; one with a slash matches the path
/// from the project directory. `*` and `?` stay within a path component,
/// `**` crosses them. Excluding a directory excludes everything in it.
pub fn excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            return false;
        }
        if p.contains('/') {
            // The path itself, or a directory it is in.
            let p = p.trim_start_matches('/').as_bytes();
            path.match_indices('/')
                .map(|(i, _)| &path[..i])
                .chain([path])
                .any(|prefix| glob(p, prefix.as_bytes()))
        } else {
            path.split('/').any(|c| glob(p.as_bytes(), c.as_bytes()))
        }
    })
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        // `**/` matches zero or more whole directories.
        [b'*', b'*', b'/', rest @ ..] => {
            glob(rest, s) || (1..=s.len()).any(|i| s[i - 1] == b'/' && glob(rest, &s[i..]))
        }
        [b'*', b'*', rest @ ..] => (0..=s.len()).any(|i| glob(rest, &s[i..])),
        [b'*', rest @ ..] => (0..=s.len())
            .take_while(|&i| i == 0 || s[i - 1] != b'/')
            .any(|i| glob(rest, &s[i..])),
        [b'?', rest @ ..] => matches!(s, [c, tail @ ..] if *c != b'/' && glob(rest, tail)),
        [c, rest @ ..] => matches!(s, [d, tail @ ..] if c == d && glob(rest, tail)),
    }
}

/// A ustar header, preceded by a PAX header when the name, link target or
/// size doesn't fit.
fn header(name: &str, mode: u32, size: u64, kind: u8, link: &str) -> io::Result<Vec<u8>> {
    let mut pax = String::new();
    if name.len() > 100 {
        pax_record(&mut pax, "path", name);
    }
    if link.len() > 100 {
        pax_record(&mut pax, "linkpath", link);
    }
    if size >= 1 << 33 {
        pax_record(&mut pax, "size", &size.to_string());
    }
    let mut out = Vec::with_capacity(BLOCK * 3);
    if !pax.is_empty() {
        out.extend(ustar("././@PaxHeader", 0o644, pax.len() as u64, b'x', ""));
        out.extend(pax.as_bytes());
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    out.extend(ustar(name, mode, size, kind, link));
    Ok(out)
}

/// `"<len> key=value\n"`, where len counts the whole record.
fn pax_record(out: &mut String, key: &str, value: &str) {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while len.to_string().len() + body.len() != len {
        len += 1;
    }
    out.push_str(&format!("{len}{body}"));
}

fn ustar(name: &str, mode: u32, size: u64, kind: u8, link: &str) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    let put = |h: &mut [u8; BLOCK], at: usize, len: usize, v: &[u8]| {
        let n = v.len().min(len);
        h[at..at + n].copy_from_slice(&v[..n]);
    };
    put(&mut h, 0, 100, name.as_bytes());
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0); // uid
    octal(&mut h[116..124], 0); // gid
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], MTIME);
    h[156] = kind;
    put(&mut h, 157, 100, link.as_bytes());
    put(&mut h, 257, 8, b"ustar\x0000");
    put(&mut h, 265, 32, b"root");
    put(&mut h, 297, 32, b"root");
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// A NUL-terminated octal field. Values too big for it are carried by a
/// PAX record instead, and the field holds 0.
fn octal(field: &mut [u8], v: u64) {
    let digits = field.len() - 1;
    let v = if v < 1 << (3 * digits) { v } else { 0 };
    field[..digits].copy_from_slice(format!("{v:0digits$o}").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("runt-tar-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn matches_excludes() {
        let ex = |p: &str, pats: &[&str]| {
            excluded(p, &pats.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert!(ex("node_modules", &["node_modules"]));
        assert!(ex("a/node_modules", &["node_modules/"]));
        assert!(ex("a/b.log", &["*.log"]));
        assert!(!ex("a/b.logx", &["*.log"]));
        assert!(ex("build/out", &["/build"]));
        assert!(!ex("src/build", &["/build"]));
        assert!(ex("src/build", &["build"]));
        assert!(ex("docs/a/b.md", &["docs/**/*.md"]));
        assert!(ex("docs/b.md", &["docs/**/*.md"]));
        assert!(!ex("docs/a/b.md", &["docs/*.md"]));
        assert!(ex("a.txt", &["?.txt"]));
        assert!(!ex("ab.txt", &["?.txt"]));
        assert!(!ex("x", &[""]));
    }

    #[test]
    fn archives_deterministically() {
        let d = tmp("det");
        fs::create_dir_all(d.join("src/deep")).unwrap();
        fs::write(d.join("src/a.txt"), "hello").unwrap();
        fs::write(d.join("src/deep/run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(d.join("src/deep/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", d.join("src/link")).unwrap();
        fs::create_dir_all(d.join("src/node_modules/x")).unwrap();
        fs::write(d.join("src/node_modules/x/i.js"), "x").unwrap();
        let long = "n".repeat(120);
        fs::write(d.join("src").join(&long), "long").unwrap();
        fs::write(d.join("top.json"), "{}").unwrap();

        let paths = vec!["src".to_string(), "./top.json".to_string()];
        let ex = vec!["node_modules".to_string()];
        let mut a = Vec::new();
        let sum = write(&d, &paths, &ex, &mut a).unwrap();
        assert_eq!(sum.files, 5);
        // Touching files changes nothing; editing one does.
        Command::new("touch")
            .arg(d.join("src/a.txt"))
            .status()
            .unwrap();
        let mut b = Vec::new();
        write(&d, &paths, &ex, &mut b).unwrap();
        assert_eq!(a, b);
        fs::write(d.join("src/a.txt"), "hellO").unwrap();
        let mut c = Vec::new();
        write(&d, &paths, &ex, &mut c).unwrap();
        assert_ne!(a, c);

        // GNU tar reads it back the way we meant.
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        fs::write(d.join("a.tar"), &a).unwrap();
        let st = Command::new("tar")
            .arg("-xf")
            .arg(d.join("a.tar"))
            .arg("-C")
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        assert_eq!(fs::read_to_string(out.join("src/a.txt")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(out.join("src").join(&long)).unwrap(),
            "long"
        );
        assert_eq!(
            fs::read_link(out.join("src/link")).unwrap(),
            Path::new("a.txt")
        );
        assert!(!out.join("src/node_modules").exists());
        let m = fs::metadata(out.join("src/deep/run.sh")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o755);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(m.mtime() as u64, MTIME);
        let listing = Command::new("tar")
            .arg("-tvf")
            .arg(d.join("a.tar"))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&listing.stdout).contains("root/root"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn copies_a_whole_directory() {
        let d = tmp("dot");
        fs::write(d.join("x"), "1").unwrap();
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), "ref").unwrap();
        let mut a = Vec::new();
        let sum = write(&d, &[".".into()], &[".git".into()], &mut a).unwrap();
        assert_eq!(sum.files, 1);
        let listing = Command::new("tar")
            .arg("-tf")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                c.stdin.take().unwrap().write_all(&a)?;
                c.wait_with_output()
            })
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&listing.stdout), "x\n");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pax_records_count_themselves() {
        let mut s = String::new();
        pax_record(&mut s, "path", "abc");
        assert_eq!(s, "12 path=abc\n");
        let mut s = String::new();
        pax_record(&mut s, "path", &"x".repeat(95));
        assert_eq!(
            s.len(),
            s.split(' ').next().unwrap().parse::<usize>().unwrap()
        );
    }
}

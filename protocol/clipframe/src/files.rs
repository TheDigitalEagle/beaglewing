//! File-batch handling shared by both endpoints: pack a clipboard's file
//! list into an in-memory tar, unpack a received tar into a staging
//! directory, and compute cheap change signatures.
//!
//! Entries are named by their basename (directories recurse under their
//! basename), so a received batch stages flat and readable. Unpacking
//! relies on the tar crate's built-in refusal of path-traversal entries.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn walk(path: &Path, out: &mut Vec<(String, u64)>, prefix: &str) -> std::io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rel = if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    };
    if meta.is_file() {
        out.push((rel, meta.len()));
        Ok(meta.len())
    } else if meta.is_dir() {
        let mut total = 0;
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for child in entries {
            total += walk(&child, out, &rel)?;
        }
        Ok(total)
    } else {
        Ok(0) // sockets, symlink oddities: skipped
    }
}

/// Change signature and total byte size for a batch of paths.
/// Returns None if any path is unreadable.
pub fn batch_signature(paths: &[PathBuf]) -> Option<(u64, u64)> {
    let mut items = Vec::new();
    let mut total = 0;
    let mut sorted: Vec<_> = paths.to_vec();
    sorted.sort();
    for p in &sorted {
        total += walk(p, &mut items, "").ok()?;
    }
    Some((crate::list_signature(items.into_iter()), total))
}

/// Pack the batch into an in-memory tar. Fails loudly past the cap.
pub fn pack(paths: &[PathBuf], cap: u64) -> Result<Vec<u8>, String> {
    let (_, total) = batch_signature(paths).ok_or("a path in the batch is unreadable")?;
    if total > cap {
        return Err(format!(
            "batch is {}MB, over the {}MB channel cap",
            total / (1024 * 1024),
            cap / (1024 * 1024)
        ));
    }
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(true);
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("no basename for {}", p.display()))?;
        let meta = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if meta.is_dir() {
            builder
                .append_dir_all(&name, p)
                .map_err(|e| format!("{}: {e}", p.display()))?;
        } else {
            builder
                .append_path_with_name(p, &name)
                .map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    builder.into_inner().map_err(|e| e.to_string())
}

/// Unpack a received tar into a fresh batch directory under `stage_root`.
/// Returns the top-level staged paths (what goes on the clipboard).
pub fn unpack_to_stage(tar_bytes: &[u8], stage_root: &Path) -> Result<Vec<PathBuf>, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let batch = stage_root.join(format!("bw-{ts}"));
    std::fs::create_dir_all(&batch).map_err(|e| e.to_string())?;
    tar::Archive::new(tar_bytes)
        .unpack(&batch)
        .map_err(|e| format!("unpack: {e}"))?;
    let mut tops: Vec<_> = std::fs::read_dir(&batch)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    tops.sort();
    if tops.is_empty() {
        return Err("received batch was empty".into());
    }
    Ok(tops)
}

/// Delete staged batches older than `max_age`. Best-effort.
pub fn cleanup_stage(stage_root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(stage_root) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let path = e.path();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|age| age > max_age)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipframe-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pack_unpack_roundtrip_files_and_dirs() {
        let d = scratch("roundtrip");
        std::fs::write(d.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir_all(d.join("sub/deep")).unwrap();
        std::fs::write(d.join("sub/deep/b.bin"), vec![7u8; 5000]).unwrap();

        let batch = vec![d.join("a.txt"), d.join("sub")];
        let tarball = pack(&batch, 1024 * 1024).unwrap();

        let stage = d.join("stage");
        let tops = unpack_to_stage(&tarball, &stage).unwrap();
        assert_eq!(tops.len(), 2);
        let staged_a = tops.iter().find(|p| p.ends_with("a.txt")).unwrap();
        assert_eq!(std::fs::read(staged_a).unwrap(), b"hello");
        let staged_b = tops
            .iter()
            .find(|p| p.ends_with("sub"))
            .unwrap()
            .join("deep/b.bin");
        assert_eq!(std::fs::read(staged_b).unwrap().len(), 5000);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cap_enforced() {
        let d = scratch("cap");
        std::fs::write(d.join("big.bin"), vec![0u8; 4096]).unwrap();
        let err = pack(&[d.join("big.bin")], 1000).unwrap_err();
        assert!(err.contains("over the"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn signature_tracks_content_shape() {
        let d = scratch("sig");
        std::fs::write(d.join("x"), b"1234").unwrap();
        let (s1, t1) = batch_signature(&[d.join("x")]).unwrap();
        assert_eq!(t1, 4);
        std::fs::write(d.join("x"), b"12345").unwrap();
        let (s2, _) = batch_signature(&[d.join("x")]).unwrap();
        assert_ne!(s1, s2);
        assert!(batch_signature(&[d.join("missing")]).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}

//! Local file handling shared by both directions: hashing, partial files,
//! resume points, conflict-free names.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// BLAKE3 of a whole file.
pub fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

pub fn part_paths(target: &Path) -> (PathBuf, PathBuf) {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir = target.parent().unwrap_or(Path::new("."));
    (
        dir.join(format!("{name}.relaypart")),
        dir.join(format!("{name}.relaypart.meta")),
    )
}

fn meta_line(size: u64, hash: &[u8; 32], chunk: u32) -> String {
    format!("relay-part v1 {size} {chunk} {}\n", relay_net::hex(hash))
}

/// An in-progress incoming file.
#[derive(Debug)]
pub struct Partial {
    pub target: PathBuf,
    part: PathBuf,
    meta: PathBuf,
    file: File,
    pub next_chunk: u64,
    pub size: u64,
    pub chunk: u32,
    pub hash: [u8; 32],
}

impl Partial {
    /// Open (or create) the partial file for `target`. If an earlier
    /// attempt left one for the *same* source (size and hash match), keep
    /// its complete chunks and resume after them.
    pub fn open(target: &Path, size: u64, chunk: u32, hash: [u8; 32]) -> std::io::Result<Self> {
        let (part, meta) = part_paths(target);
        let want = meta_line(size, &hash, chunk);
        let same_source = fs::read_to_string(&meta).is_ok_and(|m| m == want);
        let mut next_chunk = 0;
        if same_source && let Ok(md) = fs::metadata(&part) {
            next_chunk = md.len() / u64::from(chunk);
        } else {
            let _ = fs::remove_file(&part);
            fs::write(&meta, &want)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)?;
        // Drop any incomplete trailing chunk.
        file.set_len(next_chunk * u64::from(chunk))?;
        let mut p = Self {
            target: target.to_path_buf(),
            part,
            meta,
            file,
            next_chunk,
            size,
            chunk,
            hash,
        };
        p.seek_end()?;
        Ok(p)
    }

    fn seek_end(&mut self) -> std::io::Result<()> {
        use std::io::Seek;
        self.file.seek(std::io::SeekFrom::End(0)).map(|_| ())
    }

    pub fn total_chunks(&self) -> u64 {
        self.size.div_ceil(u64::from(self.chunk))
    }

    /// Append chunk `index` after checking its hash.
    pub fn write_chunk(
        &mut self,
        index: u64,
        data: &[u8],
        chunk_hash: &[u8],
    ) -> Result<(), String> {
        if index != self.next_chunk {
            return Err(format!(
                "chunk {index} out of order (expected {})",
                self.next_chunk
            ));
        }
        if blake3::hash(data).as_bytes() != chunk_hash {
            return Err(format!("chunk {index} failed its BLAKE3 check"));
        }
        let expected_len = (self.size - index * u64::from(self.chunk)).min(u64::from(self.chunk));
        if data.len() as u64 != expected_len {
            return Err(format!("chunk {index} has the wrong length"));
        }
        self.file.write_all(data).map_err(|e| e.to_string())?;
        self.next_chunk += 1;
        Ok(())
    }

    /// Verify the whole file and move it into place under a name that does
    /// not clobber an existing file (FR-29). Returns the final path.
    pub fn finish(self) -> Result<PathBuf, String> {
        self.file.sync_all().map_err(|e| e.to_string())?;
        drop(self.file);
        let got = hash_file(&self.part).map_err(|e| e.to_string())?;
        if got != self.hash {
            // Corrupt: discard so a retry starts clean.
            let _ = fs::remove_file(&self.part);
            let _ = fs::remove_file(&self.meta);
            return Err("file failed its BLAKE3 check; discarded".into());
        }
        let dest = unique_name(&self.target);
        fs::rename(&self.part, &dest).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&self.meta);
        Ok(dest)
    }
}

/// `target`, or `name (1).ext`, `name (2).ext`, ... if it exists.
pub fn unique_name(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let dir = target.parent().unwrap_or(Path::new("."));
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = target
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some free name")
}

/// Reject names that would escape the destination folder.
pub fn safe_name(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.ends_with(".relaypart")
        && !name.ends_with(".relaypart.meta");
    ok.then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("relay-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resume_only_for_the_same_source() {
        let d = tmp("resume");
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let hash = *blake3::hash(&data).as_bytes();
        let target = d.join("f.bin");
        let mut p = Partial::open(&target, data.len() as u64, 4096, hash).unwrap();
        p.write_chunk(0, &data[..4096], blake3::hash(&data[..4096]).as_bytes())
            .unwrap();
        // Half of chunk 1 lands, then the connection drops.
        p.file.write_all(&data[4096..6000]).unwrap();
        drop(p);
        let p = Partial::open(&target, data.len() as u64, 4096, hash).unwrap();
        assert_eq!(p.next_chunk, 1, "resumes after the last complete chunk");
        drop(p);
        let other = [9u8; 32];
        let p = Partial::open(&target, data.len() as u64, 4096, other).unwrap();
        assert_eq!(p.next_chunk, 0, "different source starts over");
    }

    #[test]
    fn rejects_bad_chunks_and_corrupt_files() {
        let d = tmp("bad");
        let data = vec![7u8; 5000];
        let hash = *blake3::hash(&data).as_bytes();
        let mut p = Partial::open(&d.join("x"), 5000, 4096, hash).unwrap();
        assert!(
            p.write_chunk(0, &data[..4096], &[0; 32]).is_err(),
            "bad chunk hash"
        );
        assert!(
            p.write_chunk(1, &data[4096..], blake3::hash(&data[4096..]).as_bytes())
                .is_err(),
            "out of order"
        );
        p.write_chunk(0, &data[..4096], blake3::hash(&data[..4096]).as_bytes())
            .unwrap();
        p.write_chunk(1, &data[4096..], blake3::hash(&data[4096..]).as_bytes())
            .unwrap();
        let out = p.finish().unwrap();
        assert_eq!(fs::read(&out).unwrap(), data);

        let mut p = Partial::open(&d.join("y"), 5000, 4096, [1; 32]).unwrap();
        p.write_chunk(0, &data[..4096], blake3::hash(&data[..4096]).as_bytes())
            .unwrap();
        p.write_chunk(1, &data[4096..], blake3::hash(&data[4096..]).as_bytes())
            .unwrap();
        assert!(p.finish().is_err(), "whole-file hash mismatch");
        assert!(
            !part_paths(&d.join("y")).0.exists(),
            "corrupt partial discarded"
        );
    }

    #[test]
    fn names() {
        let d = tmp("names");
        fs::write(d.join("a.txt"), "x").unwrap();
        fs::write(d.join("a (1).txt"), "x").unwrap();
        assert_eq!(unique_name(&d.join("a.txt")), d.join("a (2).txt"));
        assert_eq!(unique_name(&d.join("b.txt")), d.join("b.txt"));
        assert!(safe_name("../etc/passwd").is_none());
        assert!(safe_name("..").is_none());
        assert!(safe_name("ok.txt").is_some());
    }
}

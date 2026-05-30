//! tar + zstd packing of `(path, bytes)` entries into a single blob and back.

use std::io::Read;

use nevoflux_brain::BrainError;

/// One file in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// tar the entries, then zstd-compress (level 3).
pub fn pack(entries: &[Entry]) -> Result<Vec<u8>, BrainError> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        for e in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(e.bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, &e.path, e.bytes.as_slice())
                .map_err(|err| BrainError::MalformedArchive(format!("tar append: {err}")))?;
        }
        builder
            .finish()
            .map_err(|err| BrainError::MalformedArchive(format!("tar finish: {err}")))?;
    }
    zstd::encode_all(tar_buf.as_slice(), 3)
        .map_err(|err| BrainError::MalformedArchive(format!("zstd encode: {err}")))
}

/// zstd-decompress, then untar into `(path, bytes)` entries.
pub fn unpack(blob: &[u8]) -> Result<Vec<Entry>, BrainError> {
    let tar_buf = zstd::decode_all(blob)
        .map_err(|err| BrainError::MalformedArchive(format!("zstd decode: {err}")))?;
    let mut archive = tar::Archive::new(tar_buf.as_slice());
    let mut out = Vec::new();
    for entry in archive
        .entries()
        .map_err(|err| BrainError::MalformedArchive(format!("tar entries: {err}")))?
    {
        let mut entry =
            entry.map_err(|err| BrainError::MalformedArchive(format!("tar entry: {err}")))?;
        let path = entry
            .path()
            .map_err(|err| BrainError::MalformedArchive(format!("tar path: {err}")))?
            .to_string_lossy()
            .replace('\\', "/");
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|err| BrainError::MalformedArchive(format!("tar read: {err}")))?;
        out.push(Entry { path, bytes });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let entries = vec![
            Entry {
                path: "manifest.json".into(),
                bytes: b"{}".to_vec(),
            },
            Entry {
                path: "files/concepts/yc.md".into(),
                bytes: b"# YC\nbody".to_vec(),
            },
        ];
        let blob = pack(&entries).unwrap();
        let back = unpack(&blob).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].path, "manifest.json");
        assert_eq!(back[0].bytes, b"{}");
        assert_eq!(back[1].path, "files/concepts/yc.md");
        assert_eq!(back[1].bytes, b"# YC\nbody");
    }

    #[test]
    fn corrupt_blob_rejected() {
        assert!(matches!(
            unpack(b"not-zstd").unwrap_err(),
            BrainError::MalformedArchive(_)
        ));
    }
}

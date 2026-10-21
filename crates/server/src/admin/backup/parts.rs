//! Fixed-size envelopes around a compressed archive. No user filenames are used.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

use super::archive::{Export, Limits, Scratch};

const MAGIC: &[u8] = b"PHONOLITE-METADATA\x03\n";
const HEADER: u64 = MAGIC.len() as u64 + 16 + 4 + 4 + 8 + 32 + 32;
const MAX_PARTS: u32 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Header {
    pub set: uuid::Uuid,
    pub index: u32,
    pub count: u32,
    pub total_bytes: u64,
    pub archive_hash: [u8; 32],
    pub hash: [u8; 32],
    pub payload_bytes: u64,
}

impl Header {
    pub fn same_set(&self, other: &Self) -> bool {
        self.set == other.set
            && self.count == other.count
            && self.total_bytes == other.total_bytes
            && self.archive_hash == other.archive_hash
    }
}

pub(super) fn whole_limits(mut limits: Limits) -> Limits {
    // Gzip framing can make incompressible data slightly larger than its input.
    // This is a total-set bound; `archive` remains the per-download/upload bound.
    limits.archive = limits
        .expanded
        .saturating_add(limits.expanded / 16)
        .saturating_add(1024 * 1024);
    limits
}

pub(super) struct Prepared {
    pub scratch: Arc<Scratch>,
    pub files: Vec<(String, u64)>,
}

fn hash(file: &mut File) -> Result<[u8; 32], String> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

pub(super) fn split(mut export: Export, max_file: u64) -> Result<Prepared, String> {
    let length = export.file.metadata().map_err(|e| e.to_string())?.len();
    if length <= max_file {
        drop(export.file);
        return Ok(Prepared {
            scratch: Arc::new(export.scratch),
            files: vec![("export.phonolite".into(), length)],
        });
    }
    let capacity = max_file
        .checked_sub(HEADER)
        .filter(|n| *n > 0)
        .ok_or("Backup file limit is too small.")?;
    let count = length.div_ceil(capacity);
    if count > MAX_PARTS as u64 {
        return Err(
            "Backup would require more than 4096 files. Increase the per-file limit.".into(),
        );
    }
    export.file.rewind().map_err(|e| e.to_string())?;
    let archive_hash = hash(&mut export.file)?;
    export.file.rewind().map_err(|e| e.to_string())?;
    let set = uuid::Uuid::new_v4();
    let mut files = Vec::new();
    let mut remaining = length;
    let mut buffer = [0; 64 * 1024];
    for index in 0..count as u32 {
        let name = format!(
            "phonolite-{}-part-{:04}-of-{:04}.phonolite",
            &set.to_string()[..8],
            index + 1,
            count
        );
        let mut file = File::create(export.scratch.path(&name)).map_err(|e| e.to_string())?;
        let payload_bytes = remaining.min(capacity);
        file.write_all(MAGIC)
            .and_then(|_| file.write_all(set.as_bytes()))
            .and_then(|_| file.write_all(&index.to_le_bytes()))
            .and_then(|_| file.write_all(&(count as u32).to_le_bytes()))
            .and_then(|_| file.write_all(&length.to_le_bytes()))
            .and_then(|_| file.write_all(&archive_hash))
            .and_then(|_| file.write_all(&[0; 32]))
            .map_err(|e| e.to_string())?;
        let mut left = payload_bytes;
        let mut hasher = blake3::Hasher::new();
        while left > 0 {
            let n = left.min(buffer.len() as u64) as usize;
            export
                .file
                .read_exact(&mut buffer[..n])
                .map_err(|e| e.to_string())?;
            file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
            hasher.update(&buffer[..n]);
            left -= n as u64;
        }
        file.seek(SeekFrom::Start(HEADER - 32))
            .and_then(|_| file.write_all(hasher.finalize().as_bytes()))
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        files.push((name, HEADER + payload_bytes));
        remaining -= payload_bytes;
    }
    drop(export.file);
    std::fs::remove_file(export.scratch.path("export.phonolite")).map_err(|e| e.to_string())?;
    Ok(Prepared {
        scratch: Arc::new(export.scratch),
        files,
    })
}

pub(super) fn inspect(file: &mut File, limits: Limits) -> Result<Option<Header>, String> {
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    if length > limits.archive {
        return Err("Backup file exceeds its configured size limit.".into());
    }
    file.rewind().map_err(|e| e.to_string())?;
    let mut magic = vec![0; MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|_| "Truncated backup header.")?;
    if magic != MAGIC {
        if magic == super::MAGIC || magic == b"PHONOLITE-METADATA\x02\n" {
            file.rewind().map_err(|e| e.to_string())?;
            return Ok(None);
        }
        return Err("Choose a supported Phonolite metadata backup.".into());
    }
    let mut bytes = [0; 96];
    file.read_exact(&mut bytes)
        .map_err(|_| "Truncated backup part header.")?;
    let header = Header {
        set: uuid::Uuid::from_slice(&bytes[..16]).map_err(|_| "Invalid backup set.")?,
        index: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        count: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        total_bytes: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        archive_hash: bytes[32..64].try_into().unwrap(),
        hash: bytes[64..96].try_into().unwrap(),
        payload_bytes: length.checked_sub(HEADER).ok_or("Truncated backup part.")?,
    };
    if header.count < 2
        || header.count > MAX_PARTS
        || header.index >= header.count
        || header.total_bytes > whole_limits(limits).archive
        || header.payload_bytes == 0
        || header.total_bytes < header.payload_bytes
        || header.total_bytes < header.count as u64
    {
        return Err("Invalid backup part count or total size exceeds its limit.".into());
    }
    if hash(file)? != header.hash {
        return Err("Backup part integrity check failed; upload this file again.".into());
    }
    Ok(Some(header))
}

pub(super) fn assemble(
    scratch: &Scratch,
    parts: &std::collections::BTreeMap<u32, Header>,
    output: &Scratch,
) -> Result<File, String> {
    let first = parts.values().next().ok_or("No backup parts uploaded.")?;
    if parts.len() != first.count as usize {
        return Err("Upload all backup parts before restoring.".into());
    }
    let mut destination = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(output.path("assembled.phonolite"))
        .map_err(|e| e.to_string())?;
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    for index in 0..first.count {
        let part = parts.get(&index).ok_or("Missing backup part.")?;
        if !first.same_set(part) {
            return Err("These files belong to different backup sets.".into());
        }
        let mut file = File::open(scratch.path(&format!("part-{index}.phonolite")))
            .map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() != HEADER + part.payload_bytes {
            return Err("Backup part size changed on disk.".into());
        }
        file.seek(SeekFrom::Start(HEADER))
            .map_err(|e| e.to_string())?;
        let mut part_hash = blake3::Hasher::new();
        let mut left = part.payload_bytes;
        while left > 0 {
            let n = left.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..n])
                .map_err(|e| e.to_string())?;
            destination
                .write_all(&buffer[..n])
                .map_err(|e| e.to_string())?;
            hasher.update(&buffer[..n]);
            part_hash.update(&buffer[..n]);
            left -= n as u64;
        }
        if part_hash.finalize().as_bytes() != &part.hash {
            return Err("Backup part changed on disk; upload it again.".into());
        }
        total = total
            .checked_add(part.payload_bytes)
            .ok_or("Backup size overflow.")?;
        if total > first.total_bytes {
            return Err("Backup parts exceed their declared size.".into());
        }
    }
    if total != first.total_bytes || hasher.finalize().as_bytes() != &first.archive_hash {
        return Err("Backup set integrity check failed.".into());
    }
    destination.rewind().map_err(|e| e.to_string())?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::super::{archive, jobs, register_upload};
    use super::*;
    use library::backup::CatalogBackup;

    fn fixture() -> (Prepared, Limits) {
        let source = Scratch::new().unwrap();
        let mut bytes = vec![0; 32 * 1024];
        blake3::Hasher::new().finalize_xof().fill(&mut bytes);
        std::fs::write(source.path("artist.png"), bytes).unwrap();
        let mut catalog = CatalogBackup::default();
        catalog.artists.push(common::Artist {
            id: "old-id".into(),
            name: "Artist".into(),
            genres: vec!["Rock".into()],
            summary: Some("Biography".into()),
            logo_ref: Some("artist.png".into()),
            banner_ref: None,
        });
        let limits = Limits {
            archive: 4096,
            expanded: 128 * 1024,
            manifest: 16 * 1024,
        };
        let export =
            archive::export(vec![catalog], &source.path(""), whole_limits(limits)).unwrap();
        (split(export, limits.archive).unwrap(), limits)
    }

    fn upload(
        prepared: &Prepared,
        index: usize,
        limits: Limits,
        store: &jobs::Store,
    ) -> Result<(String, bool), String> {
        let incoming = Arc::new(Scratch::new()?);
        std::fs::copy(
            prepared.scratch.path(&prepared.files[index].0),
            incoming.path("upload.phonolite"),
        )
        .unwrap();
        let mut file = File::open(incoming.path("upload.phonolite")).unwrap();
        let header = inspect(&mut file, limits)?;
        drop(file);
        register_upload(store, incoming, header)
    }

    #[test]
    fn split_files_obey_the_limit_and_restore_from_separate_out_of_order_uploads() {
        let (prepared, limits) = fixture();
        assert!(prepared.files.len() > 2);
        assert!(!prepared.scratch.path("export.phonolite").exists());
        for (name, length) in &prepared.files {
            assert!(*length <= limits.archive);
            assert_eq!(
                *length,
                std::fs::metadata(prepared.scratch.path(name))
                    .unwrap()
                    .len()
            );
        }
        let store = jobs::Store::default();
        let (id, complete) = upload(&prepared, prepared.files.len() - 1, limits, &store).unwrap();
        assert!(!complete);
        let (scratch, received) = store
            .with(&id, |job| (job.scratch.clone(), job.received.clone()))
            .unwrap();
        assert!(assemble(&scratch, &received, &Scratch::new().unwrap())
            .unwrap_err()
            .contains("all backup parts"));
        // Identical retries do not count as an additional part.
        assert_eq!(
            upload(&prepared, prepared.files.len() - 1, limits, &store).unwrap(),
            (id.clone(), false)
        );
        assert_eq!(store.get(&id).unwrap().received_parts, 1);
        for index in (0..prepared.files.len() - 1).rev() {
            let (other_id, complete) = upload(&prepared, index, limits, &store).unwrap();
            assert_eq!(id, other_id);
            assert_eq!(complete, index == 0);
        }
        let (scratch, received) = store
            .with(&id, |job| (job.scratch.clone(), job.received.clone()))
            .unwrap();
        let output = Scratch::new().unwrap();
        let mut file = assemble(&scratch, &received, &output).unwrap();
        let imported = archive::import(&mut file, &output, whole_limits(limits)).unwrap();
        assert_eq!(
            imported.catalogs[0].artists[0].summary.as_deref(),
            Some("Biography")
        );
        assert_eq!(imported.catalogs[0].artists[0].genres, ["Rock"]);
        assert!(imported.catalogs[0].artists[0]
            .logo_ref
            .as_ref()
            .unwrap()
            .starts_with("restored/"));
    }

    #[test]
    fn conflicting_parts_corrupt_payloads_and_invalid_headers_are_rejected() {
        let (prepared, limits) = fixture();
        let store = jobs::Store::default();
        let (id, _) = upload(&prepared, 0, limits, &store).unwrap();
        let incoming = Arc::new(Scratch::new().unwrap());
        std::fs::copy(
            prepared.scratch.path(&prepared.files[1].0),
            incoming.path("upload.phonolite"),
        )
        .unwrap();
        let mut file = File::open(incoming.path("upload.phonolite")).unwrap();
        let mut header = inspect(&mut file, limits).unwrap().unwrap();
        drop(file);
        header.archive_hash[0] ^= 1;
        assert!(register_upload(&store, incoming, Some(header))
            .unwrap_err()
            .contains("conflicting"));
        assert_eq!(store.get(&id).unwrap().received_parts, 1);
        for scenario in 0..4 {
            let scratch = Scratch::new().unwrap();
            let mut bytes = std::fs::read(prepared.scratch.path(&prepared.files[0].0)).unwrap();
            match scenario {
                0 => *bytes.last_mut().unwrap() ^= 1,
                1 => bytes[MAGIC.len() + 20..MAGIC.len() + 24]
                    .copy_from_slice(&(MAX_PARTS + 1).to_le_bytes()),
                2 => bytes.truncate(HEADER as usize - 1),
                _ => bytes[MAGIC.len() + 24..MAGIC.len() + 32]
                    .copy_from_slice(&u64::MAX.to_le_bytes()),
            }
            std::fs::write(scratch.path("bad"), bytes).unwrap();
            assert!(
                inspect(&mut File::open(scratch.path("bad")).unwrap(), limits).is_err(),
                "scenario {scenario}"
            );
        }
    }

    #[test]
    fn assembly_rechecks_disk_contents_and_total_integrity() {
        let (prepared, limits) = fixture();
        let store = jobs::Store::default();
        let mut id = String::new();
        for index in 0..prepared.files.len() {
            id = upload(&prepared, index, limits, &store).unwrap().0;
        }
        let (scratch, mut received) = store
            .with(&id, |job| (job.scratch.clone(), job.received.clone()))
            .unwrap();
        for header in received.values_mut() {
            header.archive_hash[0] ^= 1;
        }
        assert!(assemble(&scratch, &received, &Scratch::new().unwrap())
            .unwrap_err()
            .contains("set integrity"));
        let (_, received) = store
            .with(&id, |job| (job.scratch.clone(), job.received.clone()))
            .unwrap();
        let path = scratch.path("part-0.phonolite");
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(assemble(&scratch, &received, &Scratch::new().unwrap())
            .unwrap_err()
            .contains("changed on disk"));
    }
}

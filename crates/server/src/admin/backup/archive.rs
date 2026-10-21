//! Disk-backed archives: a bounded JSON manifest followed by raw image blobs,
//! all gzip-compressed. Image memory is bounded by one image, not library size.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::{bufread::GzDecoder, write::GzEncoder, Compression};
use library::backup::CatalogBackup;
use serde::{Deserialize, Serialize};

use super::{asset_refs, image_extension, MAX_IMAGE};

const MAGIC: &[u8] = b"PHONOLITE-METADATA\x02\n";
const HEADER_SIZE: u64 = MAGIC.len() as u64 + 32;

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub archive: u64,
    pub expanded: u64,
    pub manifest: u64,
}

impl Limits {
    pub fn from_config(config: &crate::config::ServerConfig) -> Result<Self, String> {
        crate::config::validate_backup_limits(config).map_err(str::to_owned)?;
        Ok(Self {
            archive: config.metadata_backup_max_archive_mib * 1024 * 1024,
            expanded: config.metadata_backup_max_expanded_mib * 1024 * 1024,
            manifest: config.metadata_backup_max_manifest_mib * 1024 * 1024,
        })
    }
}

pub(super) struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Result<Self, String> {
        let path =
            std::env::temp_dir().join(format!("phonolite-metadata-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)
            .map_err(|err| format!("Cannot create backup temporary folder: {err}"))?;
        Ok(Self(path))
    }

    pub fn path(&self, filename: &str) -> PathBuf {
        self.0.join(filename)
    }

    fn stage(&self, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.path(&format!("{}.blob", blake3::hash(bytes).to_hex()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => file.write_all(bytes).map_err(|err| err.to_string())?,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if std::fs::read(&path).map_err(|err| err.to_string())? != bytes {
                    return Err("Staged artwork integrity check failed.".into());
                }
            }
            Err(err) => return Err(err.to_string()),
        }
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) struct Export {
    // Close files before removing their temporary folder (required on Windows).
    pub file: File,
    pub scratch: Scratch,
}

pub(super) struct Import {
    pub catalogs: Vec<CatalogBackup>,
    images: BTreeMap<String, PathBuf>,
}

impl Import {
    pub fn write_assets(&self, root: &Path) -> Result<(), String> {
        for (reference, path) in &self.images {
            let bytes = read_image(path)?;
            super::write_assets(root, &BTreeMap::from([(reference.clone(), bytes)]))?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    created_at: u64,
    server_version: String,
    catalogs: Vec<CatalogBackup>,
    /// Source reference -> content hash. Multiple references can share one blob.
    assets: BTreeMap<String, String>,
    /// Content hash -> raw length; payload blobs follow this sorted order.
    blobs: BTreeMap<String, u64>,
}

struct LimitedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::other(
                "Backup exceeds its configured size limit.",
            ));
        }
        let count = self.inner.write(bytes)?;
        self.remaining -= count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn read_image(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|err| err.to_string())?
        .take(MAX_IMAGE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() > MAX_IMAGE {
        return Err("An artwork file exceeds the 16 MiB backup limit.".into());
    }
    Ok(bytes)
}

pub(super) fn export(
    catalogs: Vec<CatalogBackup>,
    root: &Path,
    limits: Limits,
) -> Result<Export, String> {
    let mut assets = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let mut blobs = BTreeMap::new();
    let canonical_root = root.canonicalize().ok();
    let mut artwork_bytes = 0u64;
    for reference in asset_refs(&catalogs) {
        if image_extension(&reference).is_none() {
            return Err("Invalid stored artwork reference.".into());
        }
        let path = match root.join(&reference).canonicalize() {
            Ok(path) => path,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("Cannot read artwork: {err}")),
        };
        if !canonical_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
        {
            return Err("Artwork points outside the metadata folder.".into());
        }
        let bytes = read_image(&path)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if !blobs.contains_key(&hash) {
            artwork_bytes = artwork_bytes
                .checked_add(bytes.len() as u64)
                .ok_or("Backup size overflow.")?;
            if artwork_bytes > limits.expanded {
                return Err("Artwork exceeds the configured expanded backup limit.".into());
            }
            blobs.insert(hash.clone(), bytes.len() as u64);
            sources.insert(hash.clone(), path);
        }
        assets.insert(reference, hash);
    }
    let manifest = serde_json::to_vec(&Manifest {
        format_version: 2,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        server_version: env!("CARGO_PKG_VERSION").into(),
        catalogs,
        assets,
        blobs,
    })
    .map_err(|err| err.to_string())?;
    if manifest.len() as u64 > limits.manifest {
        return Err("Backup catalog exceeds the configured manifest size limit.".into());
    }
    if artwork_bytes
        .checked_add(manifest.len() as u64 + 8)
        .is_none_or(|size| size > limits.expanded)
    {
        return Err("Backup exceeds the configured expanded size limit.".into());
    }
    let scratch = Scratch::new()?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(scratch.path("export.phonolite"))
        .map_err(|err| err.to_string())?;
    file.write_all(MAGIC)
        .and_then(|_| file.write_all(&[0; 32]))
        .map_err(|err| err.to_string())?;
    let sink = LimitedWriter {
        inner: file,
        remaining: limits
            .archive
            .checked_sub(HEADER_SIZE)
            .ok_or("Archive limit is too small.")?,
    };
    let mut writer = GzEncoder::new(sink, Compression::fast());
    writer
        .write_all(&(manifest.len() as u64).to_le_bytes())
        .and_then(|_| writer.write_all(&manifest))
        .map_err(|err| err.to_string())?;
    drop(manifest);
    // References are immutable content versions; verify them again during copy.
    for (hash, path) in sources {
        let bytes = read_image(&path)?;
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err("Artwork changed while creating the backup; retry the download.".into());
        }
        writer.write_all(&bytes).map_err(|err| err.to_string())?;
    }
    let mut file = writer.finish().map_err(|err| err.to_string())?.inner;
    file.flush().map_err(|err| err.to_string())?;
    let digest = payload_hash(&mut file, limits.archive)?;
    file.seek(SeekFrom::Start(MAGIC.len() as u64))
        .and_then(|_| file.write_all(digest.as_bytes()))
        .and_then(|_| file.seek(SeekFrom::Start(0)))
        .map_err(|err| err.to_string())?;
    Ok(Export { file, scratch })
}

fn payload_hash(file: &mut File, max_archive: u64) -> Result<blake3::Hash, String> {
    if file.metadata().map_err(|err| err.to_string())?.len() > max_archive {
        return Err("Backup exceeds the configured archive size limit.".into());
    }
    file.seek(SeekFrom::Start(HEADER_SIZE))
        .map_err(|err| err.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|err| err.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize())
}

pub(super) fn import(file: &mut File, scratch: &Scratch, limits: Limits) -> Result<Import, String> {
    if file.metadata().map_err(|err| err.to_string())?.len() > limits.archive {
        return Err("Backup exceeds the configured archive size limit.".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|err| err.to_string())?;
    let mut magic = vec![0; MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|_| "Choose a supported Phonolite metadata backup.")?;
    if magic == super::MAGIC {
        // Existing v1 exports were bounded to 64/256 MiB. Keep those bounds
        // while reading their legacy all-in-one JSON/base64 representation.
        if file.metadata().map_err(|err| err.to_string())?.len() > super::MAX_COMPRESSED as u64 {
            return Err(
                "Legacy v1 backups cannot exceed 64 MiB; export a v2 backup for larger libraries."
                    .into(),
            );
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|err| err.to_string())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|err| err.to_string())?;
        let mut legacy = super::decode_with_limit(
            &bytes,
            limits.expanded.min(super::MAX_EXPANDED as u64) as usize,
        )?;
        let images = super::prepare_assets(&mut legacy)?;
        let mut staged = BTreeMap::new();
        for (reference, bytes) in images {
            staged.insert(reference, scratch.stage(&bytes)?);
        }
        return Ok(Import {
            catalogs: legacy.catalogs,
            images: staged,
        });
    }
    if magic != MAGIC {
        return Err("Choose a supported Phonolite metadata backup.".into());
    }
    let mut expected = [0; 32];
    file.read_exact(&mut expected)
        .map_err(|_| "Truncated backup header.")?;
    if payload_hash(file, limits.archive)?.as_bytes() != &expected {
        return Err("Backup integrity check failed; the file is damaged.".into());
    }
    file.seek(SeekFrom::Start(HEADER_SIZE))
        .map_err(|err| err.to_string())?;
    let mut decoder = GzDecoder::new(BufReader::new(file));
    let mut length = [0; 8];
    decoder
        .read_exact(&mut length)
        .map_err(|_| "Invalid compressed backup manifest.")?;
    let length = u64::from_le_bytes(length);
    if length > limits.manifest
        || length
            .checked_add(8)
            .is_none_or(|size| size > limits.expanded)
    {
        return Err("Backup manifest exceeds its size limit.".into());
    }
    let mut bytes = vec![0; length as usize];
    decoder
        .read_exact(&mut bytes)
        .map_err(|_| "Truncated backup manifest.")?;
    let mut manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid backup manifest.")?;
    drop(bytes);
    if manifest.format_version != 2 {
        return Err("Unsupported backup version; upgrade the server first.".into());
    }
    if manifest.catalogs.is_empty() {
        return Err("The backup has no metadata catalog.".into());
    }
    let mut expanded = length + 8;
    for (hash, &size) in &manifest.blobs {
        if hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || size > MAX_IMAGE as u64
        {
            return Err("Invalid artwork descriptor or artwork exceeds 16 MiB.".into());
        }
        expanded = expanded.checked_add(size).ok_or("Backup size overflow.")?;
        if expanded > limits.expanded {
            return Err("Backup exceeds the configured expanded size limit.".into());
        }
    }
    for (reference, hash) in &manifest.assets {
        if image_extension(reference).is_none() || !manifest.blobs.contains_key(hash) {
            return Err("Invalid artwork reference in backup.".into());
        }
    }
    let mut staged = BTreeMap::new();
    for (hash, &size) in &manifest.blobs {
        let mut bytes = vec![0; size as usize];
        decoder
            .read_exact(&mut bytes)
            .map_err(|_| "Truncated backup artwork.")?;
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err("Artwork integrity check failed.".into());
        }
        staged.insert(hash.clone(), scratch.stage(&bytes)?);
    }
    let mut extra = [0];
    if decoder
        .read(&mut extra)
        .map_err(|_| "Invalid compressed backup.")?
        != 0
    {
        return Err("Unexpected data after the backup artwork.".into());
    }
    if !decoder
        .into_inner()
        .fill_buf()
        .map_err(|err| err.to_string())?
        .is_empty()
    {
        return Err("Unexpected data after the backup.".into());
    }
    let mut mapping = BTreeMap::new();
    let mut images = BTreeMap::new();
    for (reference, hash) in manifest.assets {
        let ext = image_extension(&reference).ok_or("Invalid artwork reference.")?;
        let target = format!("restored/{hash}.{ext}");
        images.insert(target.clone(), staged[&hash].clone());
        mapping.insert(reference, target);
    }
    for catalog in &mut manifest.catalogs {
        for artist in &mut catalog.artists {
            for reference in [&mut artist.logo_ref, &mut artist.banner_ref] {
                *reference = reference
                    .as_ref()
                    .and_then(|value| mapping.get(value))
                    .cloned();
            }
        }
    }
    Ok(Import {
        catalogs: manifest.catalogs,
        images,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::from_config(&crate::config::ServerConfig::default()).unwrap()
    }

    fn artist(index: usize, logo: &str) -> common::Artist {
        common::Artist {
            id: index.to_string(),
            name: format!("Artist {index}"),
            genres: vec![],
            summary: Some("Saved artist biography".into()),
            logo_ref: Some(logo.into()),
            banner_ref: None,
        }
    }

    #[test]
    fn disk_archive_roundtrips_ten_thousand_artists_and_deduplicates_artwork_content() {
        let source = Scratch::new().unwrap();
        std::fs::create_dir(source.path("logos")).unwrap();
        std::fs::create_dir(source.path("banners")).unwrap();
        std::fs::write(source.path("logos/shared.png"), b"shared image").unwrap();
        std::fs::write(source.path("banners/copy.png"), b"shared image").unwrap();
        let mut catalog = CatalogBackup::default();
        catalog.artists = (0..10_000).map(|i| artist(i, "logos/shared.png")).collect();
        catalog.artists[0].banner_ref = Some("banners/copy.png".into());
        let mut export = export(vec![catalog], &source.0, limits()).unwrap();
        let scratch = Scratch::new().unwrap();
        let imported = import(&mut export.file, &scratch, limits()).unwrap();
        assert_eq!(imported.catalogs[0].artists.len(), 10_000);
        assert_eq!(imported.images.len(), 1);
        assert_eq!(
            imported.catalogs[0].artists[0].logo_ref,
            imported.catalogs[0].artists[0].banner_ref
        );
        let destination = Scratch::new().unwrap();
        imported.write_assets(&destination.0).unwrap();
        let reference = imported.catalogs[0].artists[0].logo_ref.as_ref().unwrap();
        assert_eq!(
            std::fs::read(destination.path(reference)).unwrap(),
            b"shared image"
        );
    }

    #[test]
    fn disk_archive_restores_above_the_old_64_mib_compressed_limit() {
        let source = Scratch::new().unwrap();
        let mut catalog = CatalogBackup::default();
        // Five independent incompressible 14 MiB images cross the former cap.
        // The test, like production, holds only one artwork buffer at a time.
        for i in 0..5 {
            let mut bytes = vec![0; 14 * 1024 * 1024];
            let mut hash = blake3::Hasher::new();
            hash.update(&[i]);
            hash.finalize_xof().fill(&mut bytes);
            let filename = format!("image-{i}.png");
            std::fs::write(source.path(&filename), bytes).unwrap();
            catalog.artists.push(artist(i as usize, &filename));
        }
        let mut export = export(vec![catalog], &source.0, limits()).unwrap();
        assert!(export.file.metadata().unwrap().len() > 64 * 1024 * 1024);
        let scratch = Scratch::new().unwrap();
        let imported = import(&mut export.file, &scratch, limits()).unwrap();
        assert_eq!(imported.images.len(), 5);
        for path in imported.images.values() {
            assert_eq!(std::fs::metadata(path).unwrap().len(), 14 * 1024 * 1024);
        }
    }

    fn encoded_fixture(
        scratch: &Scratch,
        manifest: &Manifest,
        image_bytes: &[u8],
        declared_length: Option<u64>,
    ) -> File {
        let json = serde_json::to_vec(manifest).unwrap();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        gzip.write_all(&declared_length.unwrap_or(json.len() as u64).to_le_bytes())
            .unwrap();
        gzip.write_all(&json).unwrap();
        gzip.write_all(image_bytes).unwrap();
        let payload = gzip.finish().unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(scratch.path("fixture.phonolite"))
            .unwrap();
        file.write_all(MAGIC).unwrap();
        file.write_all(blake3::hash(&payload).as_bytes()).unwrap();
        file.write_all(&payload).unwrap();
        file
    }

    fn manifest() -> Manifest {
        Manifest {
            format_version: 2,
            created_at: 0,
            server_version: "test".into(),
            catalogs: vec![CatalogBackup::default()],
            assets: BTreeMap::new(),
            blobs: BTreeMap::new(),
        }
    }

    #[test]
    fn disk_import_checks_manifest_and_expansion_limits_before_allocating_artwork() {
        let scratch = Scratch::new().unwrap();
        let mut file = encoded_fixture(&scratch, &manifest(), &[], Some(limits().manifest + 1));
        assert!(import(&mut file, &scratch, limits())
            .err()
            .unwrap()
            .contains("manifest"));
        drop(file);
        let scratch = Scratch::new().unwrap();
        let mut file = encoded_fixture(&scratch, &manifest(), &[], None);
        let mut bounded = limits();
        bounded.expanded = 12;
        assert!(import(&mut file, &scratch, bounded)
            .err()
            .unwrap()
            .contains("manifest"));
        bounded = limits();
        bounded.archive = file.metadata().unwrap().len() - 1;
        assert!(import(&mut file, &scratch, bounded)
            .err()
            .unwrap()
            .contains("archive size"));
    }

    #[test]
    fn disk_import_rejects_bad_artwork_descriptors_paths_hashes_and_trailing_data() {
        for scenario in 0..5 {
            let scratch = Scratch::new().unwrap();
            let mut manifest = manifest();
            let hash = blake3::hash(b"image").to_hex().to_string();
            manifest.assets.insert(
                if scenario == 0 {
                    "../escape.png".into()
                } else {
                    "image.png".into()
                },
                hash.clone(),
            );
            manifest.blobs.insert(
                hash,
                if scenario == 1 {
                    MAX_IMAGE as u64 + 1
                } else {
                    5
                },
            );
            let data = match scenario {
                2 => b"wrong".as_slice(),
                3 => b"short".as_slice().get(..2).unwrap(),
                4 => b"imageextra".as_slice(),
                _ => b"image".as_slice(),
            };
            let mut file = encoded_fixture(&scratch, &manifest, data, None);
            assert!(
                import(&mut file, &scratch, limits()).is_err(),
                "scenario {scenario}"
            );
        }
    }

    #[test]
    fn disk_import_accepts_existing_v1_backups() {
        let scratch = Scratch::new().unwrap();
        let mut catalog = CatalogBackup::default();
        catalog.artists.push(artist(0, "logos/legacy.png"));
        let legacy = super::super::BackupFile {
            format_version: 1,
            created_at: 1,
            server_version: "old".into(),
            catalogs: vec![catalog],
            assets: BTreeMap::from([(
                "logos/legacy.png".into(),
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"old artwork"),
            )]),
        };
        let bytes = super::super::encode(&legacy).unwrap();
        std::fs::write(scratch.path("legacy.phonolite"), bytes).unwrap();
        let mut file = File::open(scratch.path("legacy.phonolite")).unwrap();
        let imported = import(&mut file, &scratch, limits()).unwrap();
        assert_eq!(imported.catalogs[0].artists.len(), 1);
        assert_eq!(
            std::fs::read(imported.images.values().next().unwrap()).unwrap(),
            b"old artwork"
        );
    }
}

//! Recover references to downloaded artist artwork without contacting a provider.
//!
//! The index can be rebuilt independently of the metadata directory. In that
//! case, a missing database reference must not hide an existing verified asset.

use std::path::Path;

use common::Artist;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ArtistAssetSource {
    pub url: String,
    pub filename: String,
}

#[derive(Clone, Copy)]
pub(crate) enum ArtistArtworkKind {
    Logo,
    Banner,
}

impl ArtistArtworkKind {
    fn directory(self) -> &'static str {
        match self {
            Self::Logo => "logos",
            Self::Banner => "banners",
        }
    }
}

// These helpers perform filesystem I/O. Request handlers call them on a
// blocking worker, and only for the artists in the requested page or image.
pub(crate) fn recover_artist_artwork(metadata_root: &Path, artist: &mut Artist) {
    if artist
        .logo_ref
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        artist.logo_ref =
            stored_artist_artwork_ref(metadata_root, &artist.id, ArtistArtworkKind::Logo);
    }
    if artist
        .banner_ref
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        artist.banner_ref =
            stored_artist_artwork_ref(metadata_root, &artist.id, ArtistArtworkKind::Banner);
    }
}

pub(crate) fn stored_artist_artwork_ref(
    metadata_root: &Path,
    artist_id: &str,
    kind: ArtistArtworkKind,
) -> Option<String> {
    if !is_filename(artist_id) {
        return None;
    }
    let directory = kind.directory();
    let base = metadata_root.join(directory);
    let record = std::fs::read(base.join(format!("{artist_id}.source"))).ok()?;
    let source: ArtistAssetSource = serde_json::from_slice(&record).ok()?;
    if source.url.trim().is_empty() || !is_filename(&source.filename) {
        return None;
    }
    let versioned_name = source.filename.strip_prefix(&format!("{artist_id}."))?;
    let (version, extension) = versioned_name.split_once('.')?;
    // Only trust the content-versioned format written by the downloader.
    // Legacy files without a source record may belong to an old identity.
    if version.len() != 64
        || !version.bytes().all(|value| value.is_ascii_hexdigit())
        || !matches!(extension, "jpg" | "jpeg" | "png" | "gif" | "webp")
    {
        return None;
    }
    let metadata = std::fs::metadata(base.join(&source.filename)).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    Some(format!("{directory}/{}", source.filename))
}

fn is_filename(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', ':', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "phonolite-artist-recovery-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn artist() -> Artist {
        Artist {
            id: "artist".into(),
            name: "Artist with stored artwork".into(),
            genres: vec!["Rock".into()],
            summary: Some("Existing metadata".into()),
            logo_ref: None,
            banner_ref: None,
        }
    }

    fn write_asset(root: &Path, kind: ArtistArtworkKind, bytes: &[u8]) -> String {
        let directory = kind.directory();
        let base = root.join(directory);
        std::fs::create_dir_all(&base).unwrap();
        let filename = format!("artist.{}.png", blake3::hash(bytes).to_hex());
        std::fs::write(base.join(&filename), bytes).unwrap();
        std::fs::write(
            base.join("artist.source"),
            serde_json::to_vec(&ArtistAssetSource {
                url: "https://provider.example/art.png".into(),
                filename: filename.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        format!("{directory}/{filename}")
    }

    #[test]
    fn missing_references_are_recovered_without_a_provider_lookup() {
        let dir = TestDirectory::new();
        let logo = write_asset(&dir.0, ArtistArtworkKind::Logo, b"stored logo");
        let banner = write_asset(&dir.0, ArtistArtworkKind::Banner, b"stored banner");
        let mut artist = artist();
        artist.logo_ref = Some(" ".into());

        recover_artist_artwork(&dir.0, &mut artist);

        assert_eq!(artist.logo_ref.as_deref(), Some(logo.as_str()));
        assert_eq!(artist.banner_ref.as_deref(), Some(banner.as_str()));
        assert_eq!(artist.summary.as_deref(), Some("Existing metadata"));
        assert_eq!(std::fs::read(dir.0.join(logo)).unwrap(), b"stored logo");
    }

    #[test]
    fn explicit_references_take_precedence_over_provider_files() {
        let dir = TestDirectory::new();
        write_asset(&dir.0, ArtistArtworkKind::Logo, b"provider logo");
        let mut artist = artist();
        artist.logo_ref = Some("restored/custom-logo.png".into());

        recover_artist_artwork(&dir.0, &mut artist);

        assert_eq!(artist.logo_ref.as_deref(), Some("restored/custom-logo.png"));
        assert!(artist.banner_ref.is_none());
    }

    #[test]
    fn unverified_missing_and_empty_assets_are_not_advertised() {
        let dir = TestDirectory::new();
        let reference = write_asset(&dir.0, ArtistArtworkKind::Logo, b"logo");
        std::fs::remove_file(dir.0.join("logos/artist.source")).unwrap();
        let mut artist = artist();
        recover_artist_artwork(&dir.0, &mut artist);
        assert!(artist.logo_ref.is_none());

        write_asset(&dir.0, ArtistArtworkKind::Logo, b"logo");
        std::fs::remove_file(dir.0.join(&reference)).unwrap();
        recover_artist_artwork(&dir.0, &mut artist);
        assert!(artist.logo_ref.is_none());

        std::fs::write(dir.0.join(reference), b"").unwrap();
        recover_artist_artwork(&dir.0, &mut artist);
        assert!(artist.logo_ref.is_none());
    }

    #[test]
    fn recovery_rejects_foreign_legacy_and_unsafe_source_records() {
        let dir = TestDirectory::new();
        write_asset(&dir.0, ArtistArtworkKind::Logo, b"logo");
        for filename in [
            format!("other.{}.png", "a".repeat(64)),
            "artist.png".into(),
            "artist.old.png".into(),
            "../artist.png".into(),
            "artist.\\..\\other.png".into(),
            "C:\\artist.png".into(),
        ] {
            std::fs::write(
                dir.0.join("logos/artist.source"),
                serde_json::to_vec(&ArtistAssetSource {
                    url: "https://provider.example/art.png".into(),
                    filename,
                })
                .unwrap(),
            )
            .unwrap();
            assert!(stored_artist_artwork_ref(&dir.0, "artist", ArtistArtworkKind::Logo).is_none());
        }
        std::fs::write(dir.0.join("logos/artist.source"), b"invalid JSON").unwrap();
        assert!(stored_artist_artwork_ref(&dir.0, "artist", ArtistArtworkKind::Logo).is_none());
        assert!(stored_artist_artwork_ref(&dir.0, "../artist", ArtistArtworkKind::Logo).is_none());
    }
}

mod support;

use support::{assert_contains_all, read_repo_file};

#[test]
fn shared_models_define_catalog_cover_and_seek_shapes() {
    let common_rs = read_repo_file("crates/common/src/lib.rs");

    assert_contains_all(
        &common_rs,
        &[
            "pub struct Artist {",
            "pub name: String,",
            "pub genres: Vec<String>,",
            "pub summary: Option<String>,",
            "pub logo_ref: Option<String>,",
            "pub banner_ref: Option<String>,",
            "pub struct Album {",
            "pub artist_ids: Vec<String>,",
            "pub artist_names: Vec<String>,",
            "pub cover_ref: Option<CoverRef>,",
            "pub struct Track {",
            "pub duration_ms: u32,",
            "pub codec: Codec,",
            "pub file_relpath: String,",
            "pub file_size: u64,",
            "pub enum CoverRef {",
            "Embedded { track_id: String }",
            "File { relpath: String }",
            "pub struct SeekIndex {",
            "pub points: Vec<SeekPoint>,",
            "blake3::hash(input.as_bytes()).to_hex().to_string()",
            "parts.join(\"/\")",
        ],
    );
}

#[test]
fn library_persistence_declares_tables_codecs_and_search_entrypoints() {
    let library_rs = read_repo_file("crates/library/src/lib.rs");

    assert_contains_all(
        &library_rs,
        &[
            "const ROOT_SEP: &str = \"::\";",
            "const SEEK_STEP_MS: u32 = 5000;",
            "TableDefinition::new(\"artists\")",
            "TableDefinition::new(\"albums\")",
            "TableDefinition::new(\"tracks\")",
            "TableDefinition::new(\"artist_albums\")",
            "TableDefinition::new(\"album_tracks\")",
            "TableDefinition::new(\"track_embedded_cover\")",
            "TableDefinition::new(\"seek\")",
            "TableDefinition::new(\"external_attempts\")",
            "TableDefinition::new(\"tag_errors\")",
            "TableDefinition::new(\"tag_error_files\")",
            "pub fn list_artists(",
            "pub fn list_albums(",
            "pub fn list_tracks(",
            "pub fn get_seek(",
            "pub fn track_has_embedded_cover(",
            "\"mp3\" => Some(Codec::Mp3)",
            "\"flac\" => Some(Codec::Flac)",
        ],
    );
}

#[test]
fn library_scanning_sidecars_covers_and_rooted_relpaths_are_encoded() {
    let library_rs = read_repo_file("crates/library/src/lib.rs");

    assert_contains_all(
        &library_rs,
        &[
            ".min_depth(1)",
            ".max_depth(2)",
            "let album_sidecar = read_sidecar_info(&album_dir.join(\"album.json\"));",
            "load_sidecar_info(&mut artist_sidecar_cache, parent.join(\"artist.json\"))",
            "if tag.has_embedded_cover && album_cover.is_none()",
            "find_folder_cover(&root.path, &root.id, &album_dir)",
            "format!(\"{}{}{}\", id, ROOT_SEP, relpath)",
            "let Some(idx) = relpath.find(ROOT_SEP) else",
            "const COVERS: &[&str] = &[",
            "\"cover.jpg\"",
            "\"folder.jpg\"",
            "\"front.jpg\"",
            "\"album.jpg\"",
            "const INDEX_VERSION: u32 = 10;",
            "canonical_artist_key",
            "canonical_album_group_key",
            "find_compatible_album_identity",
            "find_duplicate_track_id",
            "first_configured_root_wins_duplicate_track_source",
            "\"cd\", \"disc\"",
            "\"disk\"",
            "\"dvd\"",
            "\"volume\"",
            "\"part\"",
            "\"side\"",
            "\"lp\"",
            "token.chars().all(is_roman_char)",
        ],
    );
}

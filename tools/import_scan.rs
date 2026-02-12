use std::env;
use std::path::{Path, PathBuf};

use library::Library;
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let mut args = env::args().skip(1);
    let music_root = args
        .next()
        .or_else(|| env::var("MUSIC_ROOT").ok())
        .ok_or("MUSIC_ROOT not set and no path argument")?;
    let index_path = args
        .next()
        .or_else(|| env::var("INDEX_PATH").ok())
        .unwrap_or_else(|| "data/library.redb".to_string());

    let index_exists = Path::new(&index_path).exists();
    let (library, _) =
        Library::load_or_scan(PathBuf::from(&music_root), PathBuf::from(&index_path))?;
    let stats = if index_exists {
        library.rescan()?
    } else {
        library.stats()?
    };

    println!(
        "Indexed: {} artists, {} albums, {} tracks",
        stats.artists, stats.albums, stats.tracks
    );

    Ok(())
}

use std::{fs, io, path::Path};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{config::resolve_path, http::json_error_response, state::AppState};

use super::super::{admin_user_from_headers, is_admin};

#[derive(Default, Deserialize)]
pub struct FolderQuery {
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct Folder {
    name: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct FolderListing {
    // A null path is the Windows drive list, which cannot itself be added.
    path: Option<String>,
    parent: Option<String>,
    folders: Vec<Folder>,
}

pub async fn admin_browse_folders(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FolderQuery>,
) -> Response {
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }

    // Only inspect one directory at a time, away from the async runtime threads.
    let config_path = state.config_path.clone();
    match tokio::task::spawn_blocking(move || browse_folders(&config_path, query.path.as_deref()))
        .await
    {
        Ok(Ok(listing)) => Json(listing).into_response(),
        Ok(Err(err)) => {
            let (status, message) = match err.kind() {
                io::ErrorKind::PermissionDenied => (
                    StatusCode::FORBIDDEN,
                    "This folder cannot be read. Check the server's folder permissions.",
                ),
                io::ErrorKind::NotFound => {
                    (StatusCode::NOT_FOUND, "This folder could not be found.")
                }
                io::ErrorKind::InvalidInput | io::ErrorKind::NotADirectory => {
                    (StatusCode::BAD_REQUEST, "Choose a valid folder path.")
                }
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "This folder could not be read.",
                ),
            };
            json_error_response(status, message)
        }
        Err(_) => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "This folder could not be read.",
        ),
    }
}

fn browse_folders(config_path: &Path, requested: Option<&str>) -> io::Result<FolderListing> {
    let requested = requested.unwrap_or_default().trim();
    if requested.is_empty() {
        #[cfg(windows)]
        {
            let folders = ('A'..='Z')
                .map(|letter| format!("{}:\\", letter))
                .filter(|drive| Path::new(drive).is_dir())
                .map(|drive| Folder {
                    name: drive.clone(),
                    path: drive,
                })
                .collect();
            return Ok(FolderListing {
                path: None,
                parent: None,
                folders,
            });
        }
        #[cfg(not(windows))]
        return list_folder(Path::new("/"));
    }
    list_folder(&resolve_path(config_path, requested))
}

fn path_text(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Non-UTF-8 path"))?;
    // Windows canonicalization uses extended paths; show ordinary drive/UNC paths.
    #[cfg(windows)]
    {
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{}", unc));
        }
        if let Some(drive) = path.strip_prefix(r"\\?\") {
            return Ok(drive.to_string());
        }
    }
    Ok(path.to_string())
}

fn list_folder(path: &Path) -> io::Result<FolderListing> {
    let path = path.canonicalize()?;
    let mut folders = Vec::new();
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if !kind.is_dir() && !(kind.is_symlink() && entry.path().is_dir()) {
            continue;
        }
        // Unrepresentable names cannot safely round-trip through JSON paths.
        if let (Some(name), Ok(path)) = (entry.file_name().to_str(), path_text(&entry.path())) {
            folders.push(Folder {
                name: name.to_string(),
                path,
            });
        }
    }
    folders.sort_by_cached_key(|folder| (folder.name.to_lowercase(), folder.name.clone()));
    let parent = path.parent().map(path_text).transpose()?;
    #[cfg(windows)]
    let parent = parent.or_else(|| Some(String::new()));
    Ok(FolderListing {
        path: Some(path_text(&path)?),
        parent,
        folders,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFolder(std::path::PathBuf);
    impl TestFolder {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("phonolite-folder-browser-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_only_immediate_directories_sorted_and_preserves_paths() {
        let temp = TestFolder::new();
        for name in ["zeta", "Alpha & music", "beta"] {
            fs::create_dir(temp.0.join(name)).unwrap();
        }
        fs::create_dir(temp.0.join("beta/nested")).unwrap();
        fs::write(temp.0.join("song.flac"), b"file").unwrap();
        let listing = list_folder(&temp.0).unwrap();
        assert_eq!(
            listing
                .folders
                .iter()
                .map(|folder| folder.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha & music", "beta", "zeta"]
        );
        assert_eq!(
            listing.path,
            Some(path_text(&temp.0.canonicalize().unwrap()).unwrap())
        );
        let child = browse_folders(&temp.0.join("server.yml"), Some("beta")).unwrap();
        assert_eq!(child.parent, listing.path);
        assert_eq!(child.folders[0].name, "nested");
        assert_eq!(listing.folders[1].path, child.path.unwrap());
    }

    #[test]
    fn empty_missing_and_file_paths() {
        let temp = TestFolder::new();
        assert!(list_folder(&temp.0).unwrap().folders.is_empty());
        assert_eq!(
            list_folder(&temp.0.join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::write(temp.0.join("file"), b"file").unwrap();
        assert!(list_folder(&temp.0.join("file")).is_err());
    }

    #[test]
    fn starts_at_the_server_os_root() {
        let listing = browse_folders(Path::new("server.yml"), None).unwrap();
        assert!(listing.parent.is_none());
        #[cfg(windows)]
        {
            assert!(listing.path.is_none());
            assert!(listing
                .folders
                .iter()
                .all(|folder| folder.path.ends_with(":\\")));
            if let Some(drive) = listing.folders.first() {
                assert_eq!(
                    list_folder(Path::new(&drive.path)).unwrap().parent,
                    Some(String::new())
                );
            }
        }
        #[cfg(not(windows))]
        assert_eq!(listing.path.as_deref(), Some("/"));
    }

    #[cfg(windows)]
    #[test]
    fn displays_canonical_drive_and_unc_paths_without_namespace_prefixes() {
        assert_eq!(path_text(Path::new(r"\\?\C:\Music")).unwrap(), r"C:\Music");
        assert_eq!(
            path_text(Path::new(r"\\?\UNC\nas\Music")).unwrap(),
            r"\\nas\Music"
        );
    }
}

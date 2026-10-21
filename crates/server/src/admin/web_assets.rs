//! The console ships in the executable; disk assets are an explicit development override.
use std::{env, fs, io, path::PathBuf};

use bytes::Bytes;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

#[derive(Default)]
pub(super) struct WebAssets {
    root: Option<PathBuf>,
}

impl WebAssets {
    pub(super) fn configured() -> Self {
        Self {
            root: env::var("PHONOLITE_WEB_ROOT")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    pub(super) fn get(&self, name: &str) -> Option<WebAsset> {
        // Exact catalog lookup also limits disk overrides to known console files.
        let index = EMBEDDED_WEB_ASSETS
            .binary_search_by_key(&name, |(name, _)| *name)
            .ok()?;
        Some(match &self.root {
            Some(root) => WebAsset::File(root.join(name)),
            None => WebAsset::Embedded(EMBEDDED_WEB_ASSETS[index].1),
        })
    }
}

pub(super) enum WebAsset {
    Embedded(&'static [u8]),
    File(PathBuf),
}

impl WebAsset {
    pub(super) fn text(self) -> Result<String, String> {
        match self {
            Self::Embedded(data) => std::str::from_utf8(data)
                .map(str::to_owned)
                .map_err(|err| format!("invalid UTF-8 in embedded template: {err}")),
            Self::File(path) => fs::read_to_string(&path)
                .map_err(|err| format!("failed to read {}: {err}", path.display())),
        }
    }

    pub(super) async fn bytes(self) -> io::Result<Bytes> {
        match self {
            Self::Embedded(data) => Ok(Bytes::from_static(data)),
            Self::File(path) => tokio::fs::read(path).await.map(Bytes::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WebAsset, WebAssets, EMBEDDED_WEB_ASSETS};

    #[test]
    fn bundled_templates_and_binary_assets_need_no_disk_directory() {
        let assets = WebAssets::default();
        for (name, marker) in [
            ("templates/layout.html", "<!DOCTYPE html>"),
            ("templates/login.html", "<form"),
            ("templates/settings.html", "<form"),
            ("templates/partials/nav.html", "/library"),
        ] {
            let WebAsset::Embedded(_) = assets.get(name).unwrap() else {
                panic!("{name} depends on a disk directory");
            };
            assert!(assets.get(name).unwrap().text().unwrap().contains(marker));
        }
        for name in [
            "static/css/admin.css",
            "static/js/app.js",
            "static/fonts/Poppins-Regular.ttf",
            "static/img/favicon.ico",
            "static/img/phonolite-logo-nobackground.png",
            "static/icons/lucide/x.svg",
            "static/icons/lucide/LICENSE",
        ] {
            assert!(
                matches!(assets.get(name), Some(WebAsset::Embedded(data)) if !data.is_empty()),
                "{name}"
            );
        }
        assert!(EMBEDDED_WEB_ASSETS
            .iter()
            .all(|(name, _)| { name.starts_with("templates/") || name.starts_with("static/") }));
        for name in [
            "README.md",
            "tests/console.test.mjs",
            "../config.yaml",
            "static/../templates/login.html",
            "static\\js\\app.js",
            "/static/js/app.js",
        ] {
            assert!(assets.get(name).is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn explicit_disk_override_is_scoped_and_does_not_mix_asset_versions() {
        let root = std::env::temp_dir().join(format!("phonolite-web-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::create_dir_all(root.join("static/js")).unwrap();
        std::fs::write(root.join("templates/layout.html"), "custom template").unwrap();
        std::fs::write(root.join("static/js/app.js"), "custom script").unwrap();
        std::fs::write(root.join("static/secret.txt"), "not an asset").unwrap();
        let assets = WebAssets {
            root: Some(root.clone()),
        };
        assert_eq!(
            assets.get("templates/layout.html").unwrap().text().unwrap(),
            "custom template"
        );
        assert_eq!(
            assets
                .get("static/js/app.js")
                .unwrap()
                .bytes()
                .await
                .unwrap(),
            "custom script"
        );
        assert!(assets.get("static/secret.txt").is_none());
        assert!(assets.get("templates/login.html").unwrap().text().is_err());
        assert!(assets
            .get("static/css/admin.css")
            .unwrap()
            .bytes()
            .await
            .is_err());
        let resolved = root.canonicalize().unwrap();
        assert!(resolved.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        std::fs::remove_dir_all(&resolved).unwrap();
        assert!(assets.get("templates/layout.html").unwrap().text().is_err());
    }
}

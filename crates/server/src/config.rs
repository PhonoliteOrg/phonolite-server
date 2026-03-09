use std::collections::HashSet;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::external::Provider;

pub const CONFIG_VERSION: u32 = 8;

#[derive(Clone, Debug, Serialize)]
#[serde(default)]
pub struct MusicRootConfig {
    pub id: String,
    pub path: String,
}

impl Default for MusicRootConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            path: String::new(),
        }
    }
}

impl<'de> Deserialize<'de> for MusicRootConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Path(String),
            Full { id: Option<String>, path: String },
        }

        let repr = Repr::deserialize(deserializer)?;
        let (id, path) = match repr {
            Repr::Path(path) => (String::new(), path),
            Repr::Full { id, path } => (id.unwrap_or_default(), path),
        };
        Ok(Self { id, path })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataSourceConfig {
    pub id: String,
    pub provider: Provider,
    pub enabled: bool,
    pub api_key: String,
    pub user_agent: String,
}

impl Default for MetadataSourceConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: Provider::TheAudioDb,
            enabled: true,
            api_key: String::new(),
            user_agent: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub version: u32,
    pub music_root: String,
    #[serde(default)]
    pub music_roots: Vec<MusicRootConfig>,
    pub index_path: String,
    pub metadata_path: String,
    #[serde(alias = "log_path")]
    pub log_dir: String,
    pub log_debug_enabled: bool,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    pub quic_enabled: bool,
    pub quic_port: u16,
    pub quic_cert_path: String,
    pub quic_key_path: String,
    pub quic_self_signed: bool,
    pub watch_music: bool,
    pub watch_debounce_secs: u64,
    pub session_ttl_secs: u64,
    pub stats_collection_enabled: bool,
    pub external_metadata_enabled: bool,
    pub external_metadata_sources: Vec<MetadataSourceConfig>,
    #[serde(default, skip_serializing)]
    pub external_metadata_provider: String,
    #[serde(default, skip_serializing)]
    pub external_metadata_api_key: String,
    pub external_metadata_min_interval_secs: u64,
    pub external_metadata_timeout_secs: u64,
    pub external_metadata_enrich_on_scan: bool,
    pub external_metadata_scan_limit: usize,
    pub external_metadata_on_tag_error: bool,
    pub stream_cache_enabled: bool,
    pub stream_cache_dir: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            music_root: "".to_string(),
            music_roots: Vec::new(),
            index_path: "library.redb".to_string(),
            metadata_path: "metadata".to_string(),
            log_dir: "logs".to_string(),
            log_debug_enabled: false,
            port: 3000,
            bind_addr: None,
            quic_enabled: true,
            quic_port: 3001,
            quic_cert_path: "quic_cert.pem".to_string(),
            quic_key_path: "quic_key.pem".to_string(),
            quic_self_signed: true,
            watch_music: true,
            watch_debounce_secs: 2,
            session_ttl_secs: 60 * 60 * 24 * 7,
            stats_collection_enabled: false,
            external_metadata_enabled: false,
            external_metadata_sources: Vec::new(),
            external_metadata_provider: "theaudiodb".to_string(),
            external_metadata_api_key: "".to_string(),
            external_metadata_min_interval_secs: 60 * 60 * 24,
            external_metadata_timeout_secs: 8,
            external_metadata_enrich_on_scan: false,
            external_metadata_scan_limit: 50,
            external_metadata_on_tag_error: true,
            stream_cache_enabled: true,
            stream_cache_dir: "stream_cache".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Yaml(serde_yaml::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "io error: {}", err),
            ConfigError::Yaml(err) => write!(f, "yaml error: {}", err),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        ConfigError::Io(err)
    }
}

impl From<serde_yaml::Error> for ConfigError {
    fn from(err: serde_yaml::Error) -> Self {
        ConfigError::Yaml(err)
    }
}

pub fn config_path_from_env() -> PathBuf {
    match env::var("PHONOLITE_CONFIG") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => default_config_path(),
    }
}

fn default_config_path() -> PathBuf {
    match env::current_exe() {
        Ok(exe) => exe
            .parent()
            .map(|dir| dir.join("config.yaml"))
            .unwrap_or_else(|| PathBuf::from("config.yaml")),
        Err(_) => PathBuf::from("config.yaml"),
    }
}

pub fn load_or_create_config(path: &Path) -> Result<(ServerConfig, bool), ConfigError> {
    if path.exists() {
        let contents = fs::read_to_string(path)?;
        let mut config: ServerConfig = serde_yaml::from_str(&contents)?;
        let legacy_bind_addr = config.bind_addr.clone();
        if config.external_metadata_sources.is_empty() {
            let provider = config.external_metadata_provider.trim();
            let api_key = config.external_metadata_api_key.trim();
            if !api_key.is_empty() && provider.eq_ignore_ascii_case("theaudiodb") {
                config.external_metadata_sources.push(MetadataSourceConfig {
                    id: format!("src-{}", source_id_suffix()),
                    provider: Provider::TheAudioDb,
                    enabled: config.external_metadata_enabled,
                    api_key: api_key.to_string(),
                    user_agent: String::new(),
                });
            }
        }
        let prev_version = config.version;
        if config.version < CONFIG_VERSION {
            config.version = CONFIG_VERSION;
        }
        if config.metadata_path.trim().is_empty() {
            config.metadata_path = "metadata".to_string();
        }
        if config.log_dir.trim().is_empty() {
            config.log_dir = "logs".to_string();
        }
        if config.stream_cache_dir.trim().is_empty() {
            config.stream_cache_dir = "stream_cache".to_string();
        }
        if prev_version < 7 {
            config.log_debug_enabled = false;
        }
        if prev_version < 8 && config.log_dir.trim().is_empty() {
            config.log_dir = "logs".to_string();
        }
        if config.port == 0 {
            if let Some(bind_addr) = legacy_bind_addr.as_deref() {
                if let Some(port) = parse_port(bind_addr) {
                    config.port = port;
                }
            }
            if config.port == 0 {
                config.port = 3000;
            }
        }
        if prev_version < 5 {
            config.quic_enabled = true;
            config.quic_self_signed = true;
        }
        if config.quic_port == 0 {
            config.quic_port = config.port.saturating_add(1);
        }
        if config.quic_port == config.port {
            config.quic_port = if config.port < u16::MAX {
                config.port + 1
            } else if config.port > 1 {
                config.port - 1
            } else {
                3001
            };
        }
        if config.quic_cert_path.trim().is_empty() {
            config.quic_cert_path = "quic_cert.pem".to_string();
        }
        if config.quic_key_path.trim().is_empty() {
            config.quic_key_path = "quic_key.pem".to_string();
        }
        normalize_music_roots(&mut config);
        config.bind_addr = normalize_bind_addr(legacy_bind_addr);
        return Ok((config, false));
    }

    let config = ServerConfig::default();
    save_config(path, &config)?;
    Ok((config, true))
}

pub fn save_config(path: &Path, config: &ServerConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let contents = serde_yaml::to_string(config)?;
    fs::write(path, contents)?;
    Ok(())
}

pub(crate) fn normalize_music_roots(config: &mut ServerConfig) {
    let legacy = config.music_root.trim().to_string();
    let mut roots = Vec::new();
    for root in std::mem::take(&mut config.music_roots) {
        let path = root.path.trim();
        if path.is_empty() {
            continue;
        }
        roots.push(MusicRootConfig {
            id: root.id.trim().to_string(),
            path: path.to_string(),
        });
    }

    if roots.is_empty() && !legacy.is_empty() {
        roots.push(MusicRootConfig {
            id: String::new(),
            path: legacy.clone(),
        });
    }

    let mut seen = HashSet::new();
    let mut default_used = false;
    for root in roots.iter_mut() {
        let id = root.id.trim().to_string();
        if id.is_empty() {
            if default_used {
                root.id = new_music_root_id();
            } else {
                root.id.clear();
                default_used = true;
            }
        } else if seen.contains(&id) {
            root.id = new_music_root_id();
        } else {
            root.id = id;
        }
        if !root.id.is_empty() {
            seen.insert(root.id.clone());
        }
    }

    config.music_roots = roots;
    config.music_root = config
        .music_roots
        .iter()
        .find(|root| root.id.is_empty())
        .or_else(|| config.music_roots.first())
        .map(|root| root.path.clone())
        .unwrap_or_default();
}

pub(crate) fn new_music_root_id() -> String {
    format!("root-{}", source_id_suffix())
}

pub fn resolve_path(config_path: &Path, value: &str) -> PathBuf {
    let raw = PathBuf::from(value);
    if raw.is_absolute() {
        return raw;
    }
    let base = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    base.join(raw)
}

pub fn resolve_music_roots(
    config_path: &Path,
    roots: &[MusicRootConfig],
) -> Vec<library::LibraryRoot> {
    let mut out = Vec::new();
    for root in roots {
        let trimmed = root.path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resolved = resolve_path(config_path, trimmed);
        out.push(library::LibraryRoot::new(root.id.clone(), resolved));
    }
    out
}

pub fn resolve_music_root(config_path: &Path, value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(resolve_path(config_path, trimmed))
    }
}

fn parse_port(value: &str) -> Option<u16> {
    split_bind_addr(value).1
}

fn source_id_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

pub fn bind_target(bind_addr: Option<&str>, port: u16) -> String {
    let host = bind_addr
        .and_then(normalize_bind_addr_str)
        .unwrap_or_else(|| "0.0.0.0".to_string());
    format_bind_target(&host, port)
}

fn normalize_bind_addr(value: Option<String>) -> Option<String> {
    value.and_then(|value| normalize_bind_addr_str(&value))
}

fn normalize_bind_addr_str(value: &str) -> Option<String> {
    split_bind_addr(value).0
}

fn split_bind_addr(value: &str) -> (Option<String>, Option<u16>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return (Some(addr.ip().to_string()), Some(addr.port()));
    }
    if let Some(host) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .map(str::trim)
    {
        if host.is_empty() {
            return (None, None);
        }
        return (Some(host.to_string()), None);
    }
    if trimmed.matches(':').count() > 1 {
        return (Some(trimmed.to_string()), None);
    }
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if let Ok(port) = port.trim().parse::<u16>() {
            let host = host.trim();
            if host.is_empty() {
                return (None, Some(port));
            }
            return (Some(host.to_string()), Some(port));
        }
    }
    (Some(trimmed.to_string()), None)
}

fn format_bind_target(host: &str, port: u16) -> String {
    if host.starts_with('[') && host.ends_with(']') {
        format!("{}:{}", host, port)
    } else if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

#[cfg(test)]
mod tests {
    use super::{bind_target, parse_port, split_bind_addr};

    #[test]
    fn parse_legacy_ipv4_bind_addr() {
        assert_eq!(parse_port("127.0.0.1:9000"), Some(9000));
        assert_eq!(
            split_bind_addr("127.0.0.1:9000").0.as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn parse_legacy_ipv6_bind_addr() {
        assert_eq!(parse_port("[::1]:9000"), Some(9000));
        assert_eq!(split_bind_addr("[::1]:9000").0.as_deref(), Some("::1"));
    }

    #[test]
    fn format_ipv6_bind_target() {
        assert_eq!(bind_target(Some("::"), 3000), "[::]:3000");
    }
}

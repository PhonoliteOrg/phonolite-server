//! Translate persisted metadata-source settings into validated provider inputs.
//! This module has no scan lifecycle, AppState, or HTTP handler dependencies.

use std::time::{Duration, SystemTime};

use crate::config::ServerConfig;
use crate::external::{self, ExternalConfig, ExternalSource, Provider};

pub(crate) fn external_config_from_settings(config: &ServerConfig) -> Option<ExternalConfig> {
    let timeout = config.external_metadata_timeout_secs.max(1);
    let mut sources = Vec::new();
    for source in &config.external_metadata_sources {
        if !source.enabled {
            continue;
        }
        match source.provider {
            Provider::TheAudioDb => {
                let api_key = source.api_key.trim();
                if api_key.is_empty() {
                    continue;
                }
                sources.push(ExternalSource {
                    provider: Provider::TheAudioDb,
                    api_key: Some(api_key.to_string()),
                    user_agent: None,
                    timeout: Duration::from_secs(timeout),
                });
            }
            Provider::MusicBrainz => {
                let user_agent = source.user_agent.trim();
                if user_agent.is_empty() {
                    continue;
                }
                sources.push(ExternalSource {
                    provider: Provider::MusicBrainz,
                    api_key: None,
                    user_agent: Some(user_agent.to_string()),
                    timeout: Duration::from_secs(timeout),
                });
            }
        }
    }
    if sources.is_empty() {
        return None;
    }
    Some(ExternalConfig { sources })
}

pub(crate) fn parse_provider(value: &str) -> Result<Provider, String> {
    let provider =
        external::provider_from_str(value).ok_or_else(|| "unsupported provider".to_string())?;
    Ok(provider)
}

pub(crate) fn source_fields_from_parts(
    provider: Provider,
    api_key: &str,
    user_agent: &str,
) -> Result<(String, String), String> {
    let api_key = api_key.trim();
    let user_agent = user_agent.trim();
    match provider {
        Provider::TheAudioDb => {
            if api_key.is_empty() {
                return Err("api key is required".to_string());
            }
            Ok((api_key.to_string(), String::new()))
        }
        Provider::MusicBrainz => {
            if user_agent.is_empty() {
                return Err("user agent is required".to_string());
            }
            Ok((String::new(), user_agent.to_string()))
        }
    }
}

pub(crate) fn new_source_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("src-{}", nanos)
}

#[cfg(test)]
mod tests {
    use super::{external_config_from_settings, parse_provider, source_fields_from_parts};
    use crate::config::{MetadataSourceConfig, ServerConfig};
    use crate::external::Provider;
    use std::time::Duration;

    #[test]
    fn metadata_source_aliases_and_validation_preserve_errors_and_provider_fields() {
        assert!(matches!(
            parse_provider(" MB ").unwrap(),
            Provider::MusicBrainz
        ));
        assert!(matches!(
            parse_provider(" audio_db ").unwrap(),
            Provider::TheAudioDb
        ));
        assert_eq!(
            parse_provider("unknown").unwrap_err(),
            "unsupported provider"
        );
        assert_eq!(
            source_fields_from_parts(Provider::TheAudioDb, " key ", "ignored").unwrap(),
            ("key".to_string(), String::new())
        );
        assert_eq!(
            source_fields_from_parts(Provider::MusicBrainz, "ignored", " client/1 ").unwrap(),
            (String::new(), "client/1".to_string())
        );
        assert_eq!(
            source_fields_from_parts(Provider::TheAudioDb, " ", "client/1").unwrap_err(),
            "api key is required"
        );
        assert_eq!(
            source_fields_from_parts(Provider::MusicBrainz, "key", " ").unwrap_err(),
            "user agent is required"
        );
    }

    #[test]
    fn metadata_source_mapping_preserves_order_trims_fields_and_clamps_timeout() {
        let config = ServerConfig {
            external_metadata_timeout_secs: 0,
            external_metadata_sources: vec![
                MetadataSourceConfig {
                    provider: Provider::MusicBrainz,
                    user_agent: " first/1 ".to_string(),
                    api_key: "ignored".to_string(),
                    ..MetadataSourceConfig::default()
                },
                MetadataSourceConfig {
                    enabled: false,
                    api_key: "disabled".to_string(),
                    ..MetadataSourceConfig::default()
                },
                MetadataSourceConfig::default(),
                MetadataSourceConfig {
                    api_key: " second ".to_string(),
                    user_agent: "ignored".to_string(),
                    ..MetadataSourceConfig::default()
                },
                MetadataSourceConfig {
                    provider: Provider::MusicBrainz,
                    user_agent: " ".to_string(),
                    ..MetadataSourceConfig::default()
                },
            ],
            ..ServerConfig::default()
        };
        let mapped = external_config_from_settings(&config).unwrap();
        assert_eq!(mapped.sources.len(), 2);
        assert!(matches!(mapped.sources[0].provider, Provider::MusicBrainz));
        assert_eq!(mapped.sources[0].user_agent.as_deref(), Some("first/1"));
        assert_eq!(mapped.sources[0].api_key, None);
        assert!(matches!(mapped.sources[1].provider, Provider::TheAudioDb));
        assert_eq!(mapped.sources[1].api_key.as_deref(), Some("second"));
        assert_eq!(mapped.sources[1].user_agent, None);
        assert!(mapped
            .sources
            .iter()
            .all(|source| source.timeout == Duration::from_secs(1)));
    }

    #[test]
    fn metadata_source_mapping_returns_none_without_enabled_valid_sources() {
        assert!(external_config_from_settings(&ServerConfig::default()).is_none());
        let config = ServerConfig {
            external_metadata_sources: vec![MetadataSourceConfig {
                enabled: false,
                api_key: "key".to_string(),
                ..MetadataSourceConfig::default()
            }],
            ..ServerConfig::default()
        };
        assert!(external_config_from_settings(&config).is_none());
    }
}

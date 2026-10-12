//! Pure validation of the main settings form; persistence belongs to the handler.
use crate::config::{parse_admin_bind_addr, validate_ports, ServerConfig};
use crate::state::SettingsForm;

pub(super) fn validate_settings(
    form: &SettingsForm,
    current: &ServerConfig,
) -> Result<ServerConfig, String> {
    let index_path = form.index_path.trim();
    if index_path.is_empty() {
        return Err("index_path is required".to_string());
    }
    let metadata_path = form.metadata_path.trim();
    if metadata_path.is_empty() {
        return Err("metadata_path is required".to_string());
    }
    let log_dir = form.log_dir.trim();
    if log_dir.is_empty() {
        return Err("log_dir is required".to_string());
    }
    let port = match form.port.trim().parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => return Err("port must be a valid number".to_string()),
    };
    let admin_port = match form.admin_port.trim().parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => return Err("admin_port must be a valid number between 1 and 65535".to_string()),
    };
    let quic_port = match form.quic_port.trim().parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => return Err("quic_port must be a valid number".to_string()),
    };
    validate_ports(port, admin_port, quic_port).map_err(str::to_owned)?;
    let admin_bind_addr = parse_admin_bind_addr(&form.admin_bind_addr).map_err(str::to_owned)?;
    let watch_debounce_secs = match form.watch_debounce_secs.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => return Err("watch_debounce_secs must be a positive number".to_string()),
    };
    let session_ttl_secs = match form.session_ttl_secs.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => return Err("session_ttl_secs must be a positive number".to_string()),
    };
    let external_min_interval_secs = match form
        .external_metadata_min_interval_secs
        .trim()
        .parse::<u64>()
    {
        Ok(value) if value > 0 => value,
        _ => {
            return Err("external_metadata_min_interval_secs must be a positive number".to_string())
        }
    };
    let external_timeout_secs = match form.external_metadata_timeout_secs.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => return Err("external_metadata_timeout_secs must be a positive number".to_string()),
    };
    let external_scan_limit = match form.external_metadata_scan_limit.trim().parse::<usize>() {
        Ok(value) => value,
        _ => return Err("external_metadata_scan_limit must be a number".to_string()),
    };

    let mut config = current.clone();
    config.index_path = index_path.to_string();
    config.metadata_path = metadata_path.to_string();
    config.log_dir = log_dir.to_string();
    config.log_debug_enabled = form.log_debug_enabled.is_some();
    config.port = port;
    config.admin_port = admin_port;
    config.admin_bind_addr = admin_bind_addr.to_string();
    config.quic_port = quic_port;
    config.watch_music = form.watch_music.is_some();
    config.watch_debounce_secs = watch_debounce_secs;
    config.session_ttl_secs = session_ttl_secs;
    config.stats_collection_enabled = form.stats_collection_enabled.is_some();
    config.external_metadata_min_interval_secs = external_min_interval_secs;
    config.external_metadata_timeout_secs = external_timeout_secs;
    config.external_metadata_scan_limit = external_scan_limit;
    config.external_metadata_on_tag_error = form.external_metadata_on_tag_error.is_some();
    let sources_enabled = config
        .external_metadata_sources
        .iter()
        .any(|source| source.enabled);
    config.external_metadata_enabled = sources_enabled;
    config.external_metadata_enrich_on_scan = sources_enabled;

    Ok(config)
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::config::MetadataSourceConfig;

    fn form() -> SettingsForm {
        SettingsForm {
            index_path: "  catalog.redb  ".into(),
            metadata_path: " metadata ".into(),
            log_dir: " logs ".into(),
            port: "3000".into(),
            admin_port: "3002".into(),
            admin_bind_addr: "127.0.0.1".into(),
            quic_port: "3001".into(),
            watch_music: None,
            watch_debounce_secs: "2".into(),
            session_ttl_secs: "60".into(),
            stats_collection_enabled: None,
            external_metadata_enabled: None,
            external_metadata_min_interval_secs: "60".into(),
            external_metadata_timeout_secs: "8".into(),
            external_metadata_enrich_on_scan: None,
            external_metadata_scan_limit: "0".into(),
            external_metadata_on_tag_error: None,
            log_debug_enabled: None,
        }
    }

    #[test]
    fn validation_preserves_error_order_and_numeric_limits() {
        let current = ServerConfig::default();
        let mut form = form();
        form.index_path.clear();
        form.port = "0".into();
        assert_eq!(
            validate_settings(&form, &current).unwrap_err(),
            "index_path is required"
        );
        form.index_path = "index".into();
        assert_eq!(
            validate_settings(&form, &current).unwrap_err(),
            "port must be a valid number"
        );
        form.port = "3001".into();
        assert_eq!(
            validate_settings(&form, &current).unwrap_err(),
            "quic_port must be different from port"
        );
        form.port = "65535".into();
        assert!(validate_settings(&form, &current).is_ok());
        form.external_metadata_timeout_secs = "0".into();
        assert_eq!(
            validate_settings(&form, &current).unwrap_err(),
            "external_metadata_timeout_secs must be a positive number"
        );
    }

    #[test]
    fn settings_only_update_form_fields_and_derive_metadata_flags_from_sources() {
        let mut current = ServerConfig {
            bind_addr: Some("127.0.0.1".into()),
            stream_cache_enabled: false,
            ..ServerConfig::default()
        };
        current
            .external_metadata_sources
            .push(MetadataSourceConfig {
                enabled: true,
                ..MetadataSourceConfig::default()
            });
        let config = validate_settings(&form(), &current).unwrap();
        assert_eq!(config.index_path, "catalog.redb");
        assert_eq!(config.bind_addr, current.bind_addr);
        assert!(!config.stream_cache_enabled);
        assert!(!config.watch_music);
        assert!(!config.stats_collection_enabled);
        assert!(config.external_metadata_enabled && config.external_metadata_enrich_on_scan);
        assert_eq!(config.external_metadata_scan_limit, 0);
        assert!(
            current.watch_music,
            "validation must not mutate current settings"
        );
        current.external_metadata_sources[0].enabled = false;
        let mut form = form();
        form.external_metadata_enabled = Some("on".into());
        form.external_metadata_enrich_on_scan = Some("on".into());
        let config = validate_settings(&form, &current).unwrap();
        assert!(!config.external_metadata_enabled && !config.external_metadata_enrich_on_scan);
    }

    #[test]
    fn admin_port_rejects_invalid_values_and_api_port_conflicts() {
        let current = ServerConfig::default();
        let mut form = form();
        for value in ["", "0", "-1", "65536", "not a port"] {
            form.admin_port = value.into();
            assert_eq!(
                validate_settings(&form, &current).unwrap_err(),
                "admin_port must be a valid number between 1 and 65535",
                "{value}"
            );
        }
        form.admin_port = form.port.clone();
        assert_eq!(
            validate_settings(&form, &current).unwrap_err(),
            "port must be different from admin_port"
        );
    }

    #[test]
    fn admin_port_is_saved_and_validated_against_the_submitted_api_port() {
        let current = ServerConfig::default();
        let mut form = form();
        // Moving both ports together can reuse the old admin port for the API.
        form.port = current.admin_port.to_string();
        for value in ["1", " 4002 ", "65535"] {
            form.admin_port = value.into();
            let updated = validate_settings(&form, &current).unwrap();
            assert_eq!(updated.admin_port, value.trim().parse::<u16>().unwrap());
            assert_eq!(updated.port, current.admin_port);
            let encoded = serde_yaml::to_string(&updated).unwrap();
            let restored: ServerConfig = serde_yaml::from_str(&encoded).unwrap();
            assert_eq!(restored.admin_port, updated.admin_port);
        }
        assert_eq!(
            current.admin_port, 3002,
            "saving must not mutate active settings"
        );
    }

    #[test]
    fn settings_reject_every_duplicate_port_pair_even_when_quic_is_disabled() {
        for quic_enabled in [true, false] {
            let current = ServerConfig {
                quic_enabled,
                ..ServerConfig::default()
            };
            for (port, admin_port, quic_port, message) in [
                (
                    "3000",
                    "3000",
                    "3001",
                    "port must be different from admin_port",
                ),
                (
                    "3000",
                    "3002",
                    "3000",
                    "quic_port must be different from port",
                ),
                (
                    "3000",
                    "3001",
                    "3001",
                    "quic_port must be different from admin_port",
                ),
                (
                    "3000",
                    "3000",
                    "3000",
                    "quic_port must be different from port",
                ),
            ] {
                let mut form = form();
                form.port = port.into();
                form.admin_port = admin_port.into();
                form.quic_port = quic_port.into();
                assert_eq!(validate_settings(&form, &current).unwrap_err(), message);
            }
        }
    }

    #[test]
    fn admin_bind_setting_accepts_interface_and_wildcard_ips_without_changing_active_config() {
        let current = ServerConfig::default();
        let mut form = form();
        for address in [
            "127.0.0.1",
            "192.168.1.10",
            " 0.0.0.0 ",
            "::1",
            "::",
            "fd00::10",
        ] {
            form.admin_bind_addr = address.into();
            let updated = validate_settings(&form, &current).unwrap();
            assert_eq!(updated.admin_bind_addr, address.trim());
            assert_eq!(current.admin_bind_addr, "127.0.0.1");
        }
        form.admin_bind_addr = "192.168.1.10:3002".into();
        assert_eq!(
            validate_settings(&form, &current).unwrap_err(),
            "admin_bind_addr must be a valid IPv4 or IPv6 address"
        );
    }
}

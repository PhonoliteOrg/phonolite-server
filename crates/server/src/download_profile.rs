//! Stable offline download profiles, independent of streaming quality.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DownloadProfile {
    #[default]
    #[serde(rename = "original")]
    Original,
    #[serde(rename = "opus_64")]
    Opus64,
    #[serde(rename = "opus_96")]
    Opus96,
    #[serde(rename = "opus_128")]
    Opus128,
    #[serde(rename = "opus_192")]
    Opus192,
}

impl DownloadProfile {
    pub const ALL: [&'static str; 5] = ["original", "opus_64", "opus_96", "opus_128", "opus_192"];

    pub fn id(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Opus64 => "opus_64",
            Self::Opus96 => "opus_96",
            Self::Opus128 => "opus_128",
            Self::Opus192 => "opus_192",
        }
    }

    pub fn bitrate(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::Opus64 => Some(64_000),
            Self::Opus96 => Some(96_000),
            Self::Opus128 => Some(128_000),
            Self::Opus192 => Some(192_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ids_are_explicit_and_unknown_profiles_are_rejected() {
        for id in DownloadProfile::ALL {
            let profile: DownloadProfile = serde_json::from_value(serde_json::json!(id)).unwrap();
            assert_eq!(profile.id(), id);
        }
        assert!(serde_json::from_value::<DownloadProfile>(serde_json::json!("opus_999")).is_err());
        assert_eq!(DownloadProfile::default(), DownloadProfile::Original);
    }
}

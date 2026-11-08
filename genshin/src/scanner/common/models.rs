use serde::{Deserialize, Serialize};

/// GOOD v3 character export
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoodCharacter {
    pub key: String,
    pub level: i32,
    pub constellation: i32,
    pub ascension: i32,
    pub talent: GoodTalent,
    /// Element for multi-element characters (Traveler, Manekin, Manekina).
    /// Only serialized when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoodTalent {
    pub auto: i32,
    pub skill: i32,
    pub burst: i32,
}

/// GOOD v3 weapon export
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoodWeapon {
    pub key: String,
    pub level: i32,
    pub ascension: i32,
    pub refinement: i32,
    #[serde(default, skip_serializing)]
    pub rarity: i32,
    pub location: String,
    pub lock: bool,
}

/// GOOD v3 artifact export
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoodArtifact {
    #[serde(rename = "setKey")]
    pub set_key: String,
    #[serde(rename = "slotKey")]
    pub slot_key: String,
    pub level: i32,
    pub rarity: i32,
    #[serde(rename = "mainStatKey")]
    pub main_stat_key: String,
    pub substats: Vec<GoodSubStat>,
    pub location: String,
    pub lock: bool,
    #[serde(default, rename = "astralMark")]
    pub astral_mark: bool,
    #[serde(default, alias = "elixerCrafted", rename = "elixirCrafted")]
    pub elixir_crafted: bool,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "unactivatedSubstats"
    )]
    pub unactivated_substats: Vec<GoodSubStat>,
    /// Total number of substat rolls (initial + upgrades). Set by the roll solver.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "totalRolls"
    )]
    pub total_rolls: Option<i32>,
}

/// Substat in GOOD v3 format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoodSubStat {
    pub key: String,
    pub value: f64,
    /// Value of the initial roll (before upgrades). Set by the roll solver
    /// when the substat has exactly 1 roll (i.e., the value IS the initial roll).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "initialValue"
    )]
    pub initial_value: Option<f64>,
    /// Per-roll raw values from game packet data (capture only, not set by OCR scanner).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rolls: Vec<f64>,
}

/// Optional GGScanner account identity extension to GOOD exports.
///
/// Only populate these fields from a validated source for the same account as
/// the exported inventory. Neither current Genshin OCR nor packet capture
/// establishes this identity; both leave export metadata absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoodMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
}

/// GOOD v3 full export
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoodExport {
    pub format: String,
    pub version: u32,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<GoodMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub characters: Option<Vec<GoodCharacter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weapons: Option<Vec<GoodWeapon>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<GoodArtifact>>,
    /// GGScanner extension: completed in-game achievement IDs.
    ///
    /// Presence is authoritative, including an empty array. Absence preserves
    /// achievement state in consumers that understand this extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievements: Option<Vec<u32>>,
}

impl GoodExport {
    pub fn new(
        characters: Option<Vec<GoodCharacter>>,
        weapons: Option<Vec<GoodWeapon>>,
        artifacts: Option<Vec<GoodArtifact>>,
    ) -> Self {
        Self {
            format: "GOOD".to_string(),
            version: 3,
            source: "yas-GOODScanner".to_string(),
            metadata: None,
            characters,
            weapons,
            artifacts,
            achievements: None,
        }
    }

    pub fn with_achievements(mut self, achievements: Option<Vec<u32>>) -> Self {
        self.achievements = achievements;
        self
    }
}

#[cfg(test)]
mod export_metadata_tests {
    use super::*;
    use crate::continuous::ContinuousCapture;
    use serde_json::json;

    #[test]
    fn normal_exports_and_legacy_roundtrips_omit_unavailable_identity() {
        let export = GoodExport::new(Some(Vec::new()), Some(Vec::new()), Some(Vec::new()));
        assert!(export.metadata.is_none());
        let legacy = json!({
            "format": "GOOD", "version": 3, "source": "yas-GOODScanner",
            "characters": [], "weapons": [], "artifacts": []
        });
        assert_eq!(serde_json::to_value(&export).unwrap(), legacy);
        let decoded: GoodExport = serde_json::from_value(legacy.clone()).unwrap();
        assert!(decoded.metadata.is_none());
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);
    }

    #[test]
    fn validated_identity_metadata_roundtrips_without_inventing_optional_fields() {
        let mut export = GoodExport::new(None, None, None);
        export.metadata = Some(GoodMetadata {
            uid: Some("123456789".into()),
            nickname: Some("Test Traveler".into()),
        });
        let serialized = serde_json::to_value(&export).unwrap();
        assert_eq!(
            serialized["metadata"],
            json!({
                "uid": "123456789", "nickname": "Test Traveler"
            })
        );
        let decoded: GoodExport = serde_json::from_value(serialized).unwrap();
        assert_eq!(decoded.metadata, export.metadata);

        export.metadata = Some(GoodMetadata {
            uid: Some("123456789".into()),
            nickname: None,
        });
        assert_eq!(
            serde_json::to_value(export).unwrap()["metadata"],
            json!({"uid": "123456789"})
        );
    }

    #[test]
    fn continuous_snapshots_preserve_export_identity_without_adding_it() {
        let capture = ContinuousCapture::default();
        let mut export = GoodExport::new(None, None, Some(Vec::new()));
        capture.publish(serde_json::to_value(&export).unwrap());
        let (status, response) = ContinuousCapture::response(Some(&capture));
        assert_eq!(status, 200);
        assert!(response["data"].get("metadata").is_none());

        export.metadata = Some(GoodMetadata {
            uid: Some("123456789".into()),
            nickname: Some("Test Traveler".into()),
        });
        capture.publish(serde_json::to_value(&export).unwrap());
        let (status, response) = ContinuousCapture::response(Some(&capture));
        assert_eq!(status, 200);
        let decoded: GoodExport = serde_json::from_value(response["data"].clone()).unwrap();
        assert_eq!(decoded.metadata, export.metadata);
    }
}

/// Debug info for a single OCR field.
#[derive(Debug)]
pub struct DebugOcrField {
    /// Field name (e.g., "weapon_name", "level")
    pub field_name: String,
    /// Raw OCR output text
    pub raw_text: String,
    /// Parsed/matched value
    pub parsed_value: String,
    /// OCR region used (x, y, w, h) at 1920x1080 base
    pub region: (f64, f64, f64, f64),
    /// Time taken for this OCR call in milliseconds
    pub duration_ms: u64,
}

/// Debug result of scanning a single item.
#[derive(Debug)]
pub struct DebugScanResult {
    /// Per-field OCR results
    pub fields: Vec<DebugOcrField>,
    /// Total scan time in milliseconds
    pub total_duration_ms: u64,
    /// The parsed item data as a serializable string
    pub parsed_json: String,
}

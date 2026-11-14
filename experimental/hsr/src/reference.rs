use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    error::{hints, HsrError, HsrResult},
    localization::BilingualName,
    model::{
        CharacterReference, GearReference, GearSlot, LightConeReference, ReferenceSnapshot,
        RelicMainAffixReference, StatReference, StatValueKind, TrailblazerGender,
        TrailblazerIdentity, REFERENCE_SCHEMA_VERSION,
    },
    privacy::reject_sensitive_fields,
};

const TRAILBLAZER_NAME_PLACEHOLDER: &str = "{NICKNAME}";

/// Boundary for normalized HSR reference data. A future GIlore adapter should
/// produce this snapshot rather than leaking datamine-specific structures into
/// the scanner pipeline.
pub trait ReferenceProvider {
    fn load(&self) -> HsrResult<ReferenceSnapshot>;
}

/// Conservative lower bounds for an account-facing HSR reference bundle.
/// The audited HSR 4.5 bundle contains 93 characters, 169 Light Cones, 742
/// relic definitions, 54 named relic properties, and 117 main affixes. These
/// floors tolerate small upstream filtering corrections while rejecting the
/// deliberately tiny committed fixture before any live account interaction.
pub const LIVE_REFERENCE_MIN_CHARACTERS: usize = 90;
pub const LIVE_REFERENCE_MIN_LIGHT_CONES: usize = 160;
pub const LIVE_REFERENCE_MIN_GEAR_PIECES: usize = 700;
pub const LIVE_REFERENCE_MIN_STATS: usize = 50;
pub const LIVE_REFERENCE_MIN_MAIN_AFFIXES: usize = 110;

const LIVE_REQUIRED_STAT_KEYS: &[&str] = &[
    "HPDelta",
    "AttackDelta",
    "DefenceDelta",
    "SpeedDelta",
    "HPAddedRatio",
    "AttackAddedRatio",
    "DefenceAddedRatio",
    "CriticalChanceBase",
    "CriticalDamageBase",
    "HealRatioBase",
    "StatusProbabilityBase",
    "StatusResistanceBase",
    "BreakDamageAddedRatioBase",
    "SPRatioBase",
    "PhysicalAddedRatio",
    "FireAddedRatio",
    "IceAddedRatio",
    "ThunderAddedRatio",
    "WindAddedRatio",
    "QuantumAddedRatio",
    "ImaginaryAddedRatio",
];

const LIVE_REQUIRED_MAIN_STAT_KEYS: &[&str] = &[
    "HPDelta",
    "AttackDelta",
    "SpeedDelta",
    "HPAddedRatio",
    "AttackAddedRatio",
    "DefenceAddedRatio",
    "CriticalChanceBase",
    "CriticalDamageBase",
    "HealRatioBase",
    "StatusProbabilityBase",
    "BreakDamageAddedRatioBase",
    "SPRatioBase",
    "PhysicalAddedRatio",
    "FireAddedRatio",
    "IceAddedRatio",
    "ThunderAddedRatio",
    "WindAddedRatio",
    "QuantumAddedRatio",
    "ImaginaryAddedRatio",
];

#[derive(Debug, Clone)]
pub struct JsonFileReferenceProvider {
    path: PathBuf,
}

/// Reads the normalized `ggstarrail-reference` bundle emitted by GIlore.
/// Every required member is checked against the manifest revision and SHA-256
/// before any public reference is exposed to the scanner.
#[derive(Debug, Clone)]
pub struct GiloreBundleReferenceProvider {
    root: PathBuf,
}

impl GiloreBundleReferenceProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Load and validate a GIlore reference bundle in one call. Inventory-only
/// schema 1.1 bundles remain valid; schema 1.2+ bundles additionally expose the
/// public achievement ID set.
pub fn load_gilore_reference_bundle(root: impl Into<PathBuf>) -> HsrResult<ReferenceCache> {
    ReferenceCache::from_provider(&GiloreBundleReferenceProvider::new(root))
}

pub use crate::embedded_reference::load_embedded_gilore_reference;

impl JsonFileReferenceProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl ReferenceProvider for JsonFileReferenceProvider {
    fn load(&self) -> HsrResult<ReferenceSnapshot> {
        let text = fs::read_to_string(&self.path).map_err(|error| {
            HsrError::new(
                "HSR-REF-READ",
                hints::READ_FAILED,
                format!("path={}; cause={error}", self.path.display()),
            )
        })?;
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            HsrError::new(
                "HSR-REF-JSON",
                hints::JSON_INVALID,
                format!("path={}; cause={error}", self.path.display()),
            )
        })?;
        reject_sensitive_fields(&value).map_err(|error| {
            HsrError::new(
                error.code(),
                hints::SENSITIVE_DATA,
                format!("path={}; cause={error}", self.path.display()),
            )
        })?;
        serde_json::from_value(value).map_err(|error| {
            HsrError::new(
                "HSR-REF-JSON",
                hints::JSON_INVALID,
                format!("path={}; cause={error}", self.path.display()),
            )
        })
    }
}

#[derive(Debug, Deserialize)]
struct GiloreManifest {
    bundle_id: String,
    game_id: String,
    schema_version: String,
    source: GiloreSource,
    files: BTreeMap<String, GiloreManifestFile>,
}

#[derive(Debug, Deserialize)]
struct GiloreSource {
    revision: String,
}

#[derive(Debug, Deserialize)]
struct GiloreManifestFile {
    byte_count: u64,
    entity_count: Option<u64>,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct GiloreMember<T> {
    bundle_id: String,
    collection: String,
    game_id: String,
    schema_version: String,
    source_revision: String,
    value: T,
}

#[derive(Debug, Deserialize)]
struct GiloreLocalizedValue {
    value: String,
}

#[derive(Debug, Deserialize)]
struct GiloreLocalizedName {
    en: GiloreLocalizedValue,
    #[serde(rename = "zh-CN")]
    zh_cn: GiloreLocalizedValue,
}

impl GiloreLocalizedName {
    fn normalized(self) -> crate::localization::BilingualName {
        crate::localization::BilingualName {
            zh_cn: self.zh_cn.value,
            en: self.en.value,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GiloreCharacter {
    id: String,
    name: GiloreLocalizedName,
    rarity: u8,
    path_id: String,
}

#[derive(Debug, Deserialize)]
struct GiloreLightCone {
    id: String,
    name: GiloreLocalizedName,
    rarity: u8,
    path_id: String,
}

#[derive(Debug, Deserialize)]
struct GiloreRelicSet {
    id: String,
    name: GiloreLocalizedName,
    kind: String,
}

#[derive(Debug, Deserialize)]
struct GiloreRelicPiece {
    id: String,
    set_id: String,
    slot: String,
    rarity: u8,
    main_affix_group: u32,
    max_level: u8,
    icon_path: String,
    name: GiloreLocalizedName,
}

#[derive(Debug, Deserialize)]
struct GilorePropertyTables {
    properties: Vec<GiloreProperty>,
}

#[derive(Debug, Deserialize)]
struct GiloreProperty {
    id: String,
    relic_name: Option<GiloreLocalizedName>,
    #[serde(default)]
    value_kind: StatValueKind,
}

#[derive(Debug, Deserialize)]
struct GiloreProgression {
    relic_main_affixes: Vec<GiloreRelicMainAffix>,
}

#[derive(Debug, Deserialize)]
struct GiloreRelicMainAffix {
    group_id: u32,
    property_id: String,
    max_level: u8,
    level_values: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct GiloreAchievement {
    id: u32,
}

impl ReferenceProvider for GiloreBundleReferenceProvider {
    fn load(&self) -> HsrResult<ReferenceSnapshot> {
        let manifest_path = self.root.join("manifest.json");
        let manifest_bytes = read_file(&manifest_path, "manifest")?;
        let manifest_value: Value = serde_json::from_slice(&manifest_bytes).map_err(|error| {
            HsrError::new(
                "HSR-GILORE-MANIFEST",
                hints::REFERENCE_INVALID,
                format!("manifest JSON invalid; cause={error}"),
            )
        })?;
        reject_sensitive_fields(&manifest_value)?;
        let manifest: GiloreManifest = serde_json::from_value(manifest_value).map_err(|error| {
            HsrError::new(
                "HSR-GILORE-MANIFEST",
                hints::REFERENCE_INVALID,
                format!("manifest JSON invalid; cause={error}"),
            )
        })?;
        validate_gilore_header(
            &manifest.bundle_id,
            &manifest.game_id,
            &manifest.schema_version,
        )?;
        require_safe_identifier("source.revision", &manifest.source.revision)?;
        let includes_achievements = gilore_schema_includes_achievements(&manifest.schema_version)?;

        let characters: GiloreMember<Vec<GiloreCharacter>> =
            self.read_member(&manifest, "characters.json", "characters")?;
        let light_cones: GiloreMember<Vec<GiloreLightCone>> =
            self.read_member(&manifest, "light_cones.json", "light_cones")?;
        let relic_sets: GiloreMember<Vec<GiloreRelicSet>> =
            self.read_member(&manifest, "relic_sets.json", "relic_sets")?;
        let relic_pieces: GiloreMember<Vec<GiloreRelicPiece>> =
            self.read_member(&manifest, "relic_pieces.json", "relic_pieces")?;
        let property_tables: GiloreMember<GilorePropertyTables> =
            self.read_member(&manifest, "property_tables.json", "property_tables")?;
        let progression: GiloreMember<GiloreProgression> =
            self.read_member(&manifest, "progression.json", "progression")?;
        let achievement_ids = if includes_achievements {
            let categories: GiloreMember<Vec<Value>> = self.read_member(
                &manifest,
                "achievement_categories.json",
                "achievement_categories",
            )?;
            if categories.value.is_empty() {
                return reference_error(
                    "achievement_categories.json must contain at least one category".to_string(),
                );
            }
            let achievements: GiloreMember<Vec<GiloreAchievement>> =
                self.read_member(&manifest, "achievements.json", "achievements")?;
            if achievements.value.is_empty() {
                return reference_error(
                    "achievements.json must contain at least one achievement".to_string(),
                );
            }
            achievements
                .value
                .into_iter()
                .map(|entry| entry.id)
                .collect()
        } else {
            Vec::new()
        };

        let sets = relic_sets
            .value
            .into_iter()
            .map(|entry| {
                let category = match entry.kind.as_str() {
                    "cavern_relic" => crate::model::GearCategory::Relic,
                    "planar_ornament" => crate::model::GearCategory::PlanarOrnament,
                    other => {
                        return Err(HsrError::new(
                            "HSR-GILORE-SET-KIND",
                            hints::REFERENCE_INVALID,
                            format!("unsupported relic set kind={other}"),
                        ))
                    },
                };
                Ok((entry.id, (entry.name.normalized(), category)))
            })
            .collect::<HsrResult<BTreeMap<_, _>>>()?;

        let characters = characters
            .value
            .into_iter()
            .map(|entry| {
                let game_id = parse_public_id("character", &entry.id)?;
                Ok(CharacterReference {
                    game_id,
                    key: entry.id,
                    name: entry.name.normalized(),
                    rarity: entry.rarity,
                    path: entry.path_id,
                })
            })
            .collect::<HsrResult<Vec<_>>>()?;
        let light_cones = light_cones
            .value
            .into_iter()
            .map(|entry| {
                let game_id = parse_public_id("light cone", &entry.id)?;
                Ok(LightConeReference {
                    game_id,
                    key: entry.id,
                    name: entry.name.normalized(),
                    rarity: entry.rarity,
                    path: entry.path_id,
                })
            })
            .collect::<HsrResult<Vec<_>>>()?;
        let gear_pieces = relic_pieces
            .value
            .into_iter()
            .map(|entry| {
                let game_id = parse_public_id("relic piece", &entry.id)?;
                let (set_name, category) = sets.get(&entry.set_id).ok_or_else(|| {
                    HsrError::new(
                        "HSR-GILORE-SET-MISSING",
                        hints::REFERENCE_INVALID,
                        format!("relic piece references unknown set={}", entry.set_id),
                    )
                })?;
                let slot = parse_gilore_slot(&entry.slot)?;
                if slot.category() != *category {
                    return Err(HsrError::new(
                        "HSR-GILORE-SLOT-KIND",
                        hints::REFERENCE_INVALID,
                        format!("piece={} slot/category disagree", entry.id),
                    ));
                }
                require_text("relicPiece.iconPath", &entry.icon_path)?;
                Ok(GearReference {
                    game_id,
                    key: entry.id,
                    name: entry.name.normalized(),
                    icon_path: entry.icon_path,
                    set_key: entry.set_id,
                    set_name: set_name.clone(),
                    rarity: entry.rarity,
                    category: *category,
                    slot,
                    main_affix_group: entry.main_affix_group,
                    max_level: entry.max_level,
                })
            })
            .collect::<HsrResult<Vec<_>>>()?;
        let stats = property_tables
            .value
            .properties
            .into_iter()
            .filter_map(|entry| {
                entry.relic_name.map(|name| StatReference {
                    key: entry.id,
                    name: name.normalized(),
                    value_kind: entry.value_kind,
                })
            })
            .collect();
        let relic_main_affixes = progression
            .value
            .relic_main_affixes
            .into_iter()
            .map(|entry| RelicMainAffixReference {
                group_id: entry.group_id,
                property_id: entry.property_id,
                max_level: entry.max_level,
                level_values: entry.level_values,
            })
            .collect();

        Ok(ReferenceSnapshot {
            schema_version: REFERENCE_SCHEMA_VERSION,
            // Namespace the producer and the audited bundle identifier. This
            // remains stable if GIlore publishes other HSR bundles later.
            provider: "gilore.ggstarrail-reference".to_string(),
            revision: manifest.source.revision,
            characters,
            light_cones,
            gear_pieces,
            stats,
            relic_main_affixes,
            achievement_ids,
        })
    }
}

impl GiloreBundleReferenceProvider {
    fn read_member<T: for<'de> Deserialize<'de>>(
        &self,
        manifest: &GiloreManifest,
        filename: &str,
        collection: &str,
    ) -> HsrResult<GiloreMember<T>> {
        let manifest_file = manifest.files.get(filename).ok_or_else(|| {
            HsrError::new(
                "HSR-GILORE-MEMBER",
                hints::REFERENCE_INVALID,
                format!("manifest omits required member={filename}"),
            )
        })?;
        let bytes = read_file(&self.root.join(filename), filename)?;
        if manifest_file.byte_count != bytes.len() as u64 {
            return Err(HsrError::new(
                "HSR-GILORE-BYTE-COUNT",
                hints::REFERENCE_INVALID,
                format!(
                    "member={filename} decoded byte count does not match manifest; expected={}; actual={}",
                    manifest_file.byte_count,
                    bytes.len()
                ),
            ));
        }
        let actual_sha = hex::encode(Sha256::digest(&bytes));
        if actual_sha != manifest_file.sha256 {
            return Err(HsrError::new(
                "HSR-GILORE-HASH",
                hints::REFERENCE_INVALID,
                format!("member={filename} SHA-256 does not match manifest"),
            ));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            HsrError::new(
                "HSR-GILORE-MEMBER",
                hints::REFERENCE_INVALID,
                format!("member={filename} JSON invalid; cause={error}"),
            )
        })?;
        reject_sensitive_fields(&value)?;
        let actual_entity_count = decoded_entity_count(collection, &value)?;
        if manifest_file.entity_count != Some(actual_entity_count) {
            return Err(HsrError::new(
                "HSR-GILORE-ENTITY-COUNT",
                hints::REFERENCE_INVALID,
                format!(
                    "member={filename} decoded entity count does not match manifest; expected={}; actual={actual_entity_count}",
                    manifest_file
                        .entity_count
                        .map_or_else(|| "null".to_string(), |count| count.to_string())
                ),
            ));
        }
        let member: GiloreMember<T> = serde_json::from_value(value).map_err(|error| {
            HsrError::new(
                "HSR-GILORE-MEMBER",
                hints::REFERENCE_INVALID,
                format!("member={filename} JSON invalid; cause={error}"),
            )
        })?;
        validate_gilore_header(&member.bundle_id, &member.game_id, &member.schema_version)?;
        if member.collection != collection
            || member.source_revision != manifest.source.revision
            || member.schema_version != manifest.schema_version
        {
            return Err(HsrError::new(
                "HSR-GILORE-COHERENCE",
                hints::REFERENCE_INVALID,
                format!("member={filename} envelope disagrees with manifest"),
            ));
        }
        Ok(member)
    }
}

fn decoded_entity_count(collection: &str, document: &Value) -> HsrResult<u64> {
    let value = document.get("value").ok_or_else(|| {
        HsrError::new(
            "HSR-GILORE-ENTITY-COUNT",
            hints::REFERENCE_INVALID,
            format!("collection={collection} has no value field"),
        )
    })?;
    let count = match collection {
        "characters"
        | "light_cones"
        | "relic_sets"
        | "relic_pieces"
        | "achievement_categories"
        | "achievements" => json_array_len(value, collection)?,
        "property_tables" => json_array_len(
            value.get("properties").unwrap_or(&Value::Null),
            "property_tables.properties",
        )?,
        "progression" => {
            let mut total = 0_u64;
            for field in [
                "items",
                "character_experience",
                "light_cone_experience",
                "relic_experience",
                "relic_main_affixes",
                "relic_sub_affixes",
            ] {
                if let Some(entries) = value.get(field) {
                    total = total
                        .checked_add(json_array_len(entries, field)?)
                        .ok_or_else(entity_count_overflow)?;
                }
            }
            if let Some(scoring) = value.get("relic_scoring") {
                for field in [
                    "main_affix_base_values",
                    "sub_affix_base_values",
                    "main_affix_character_weights",
                    "sub_affix_character_weights",
                ] {
                    if let Some(entries) = scoring.get(field) {
                        total = total
                            .checked_add(json_array_len(entries, field)?)
                            .ok_or_else(entity_count_overflow)?;
                    }
                }
            }
            total
        },
        other => {
            return Err(HsrError::new(
                "HSR-GILORE-ENTITY-COUNT",
                hints::REFERENCE_INVALID,
                format!("unsupported counted collection={other}"),
            ))
        },
    };
    Ok(count)
}

fn json_array_len(value: &Value, field: &str) -> HsrResult<u64> {
    value
        .as_array()
        .and_then(|entries| u64::try_from(entries.len()).ok())
        .ok_or_else(|| {
            HsrError::new(
                "HSR-GILORE-ENTITY-COUNT",
                hints::REFERENCE_INVALID,
                format!("counted field={field} is not a bounded JSON array"),
            )
        })
}

fn entity_count_overflow() -> HsrError {
    HsrError::new(
        "HSR-GILORE-ENTITY-COUNT",
        hints::REFERENCE_INVALID,
        "decoded entity count overflowed",
    )
}

fn read_file(path: &Path, label: &str) -> HsrResult<Vec<u8>> {
    fs::read(path).map_err(|error| {
        HsrError::new(
            "HSR-GILORE-READ",
            hints::READ_FAILED,
            format!(
                "unable to read GIlore {label}; path={}; cause={error}",
                path.display()
            ),
        )
    })
}

fn validate_gilore_header(bundle: &str, game: &str, schema: &str) -> HsrResult<()> {
    let (major, _, _) = parse_gilore_schema_version(schema)?;
    if bundle != "ggstarrail-reference"
        || game != "honkai_star_rail"
        || (major != 1 && schema != "2.1.0")
    {
        return reference_error(format!(
            "unsupported GIlore envelope bundle={bundle}, game={game}, schema={schema}"
        ));
    }
    Ok(())
}

fn gilore_schema_includes_achievements(schema: &str) -> HsrResult<bool> {
    let (major, minor, _) = parse_gilore_schema_version(schema)?;
    Ok((major == 1 && minor >= 2) || major == 2)
}

fn parse_gilore_schema_version(schema: &str) -> HsrResult<(u32, u32, u32)> {
    let components = schema
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            HsrError::new(
                "HSR-GILORE-SCHEMA",
                hints::REFERENCE_INVALID,
                "GIlore schemaVersion must use numeric major.minor.patch form",
            )
        })?;
    match components.as_slice() {
        [major, minor, patch] => Ok((*major, *minor, *patch)),
        _ => reference_error(
            "GIlore schemaVersion must use numeric major.minor.patch form".to_string(),
        ),
    }
}

fn parse_public_id(kind: &str, value: &str) -> HsrResult<u32> {
    value.parse::<u32>().map_err(|error| {
        HsrError::new(
            "HSR-GILORE-ID",
            hints::REFERENCE_INVALID,
            format!("{kind} has non-numeric public ID; cause={error}"),
        )
    })
}

fn parse_gilore_slot(slot: &str) -> HsrResult<crate::model::GearSlot> {
    use crate::model::GearSlot;
    match slot {
        "HEAD" => Ok(GearSlot::Head),
        "HAND" => Ok(GearSlot::Hands),
        "BODY" => Ok(GearSlot::Body),
        "FOOT" => Ok(GearSlot::Feet),
        "NECK" => Ok(GearSlot::PlanarSphere),
        "OBJECT" => Ok(GearSlot::LinkRope),
        other => Err(HsrError::new(
            "HSR-GILORE-SLOT",
            hints::REFERENCE_INVALID,
            format!("unsupported GIlore relic slot={other}"),
        )),
    }
}

#[derive(Debug, Clone)]
pub struct ReferenceCache {
    schema_version: u32,
    provider: String,
    revision: String,
    characters: BTreeMap<u32, CharacterReference>,
    light_cones: BTreeMap<u32, LightConeReference>,
    gear_pieces: BTreeMap<u32, GearReference>,
    stats: BTreeMap<String, StatReference>,
    relic_main_affixes: BTreeMap<(u32, String), RelicMainAffixReference>,
    achievement_ids: BTreeSet<u32>,
    packet_references: Option<crate::packet_reference::PacketReferences>,
}

impl ReferenceCache {
    pub fn character_count(&self) -> usize {
        self.characters.len()
    }

    pub fn from_provider(provider: &dyn ReferenceProvider) -> HsrResult<Self> {
        Self::from_snapshot(provider.load()?)
    }

    pub fn from_snapshot(snapshot: ReferenceSnapshot) -> HsrResult<Self> {
        validate_snapshot_header(&snapshot)?;

        let characters = collect_unique(
            snapshot.characters,
            "characters",
            |reference| reference.game_id,
            validate_character,
        )?;
        let light_cones = collect_unique(
            snapshot.light_cones,
            "lightCones",
            |reference| reference.game_id,
            validate_light_cone,
        )?;
        let gear_pieces = collect_unique(
            snapshot.gear_pieces,
            "gearPieces",
            |reference| reference.game_id,
            validate_gear,
        )?;
        let stats = collect_unique_string(
            snapshot.stats,
            "stats",
            |reference| reference.key.clone(),
            validate_stat,
        )?;
        let mut relic_main_affixes = BTreeMap::new();
        for affix in snapshot.relic_main_affixes {
            validate_relic_main_affix(&affix, &stats)?;
            let key = (affix.group_id, affix.property_id.clone());
            if relic_main_affixes.insert(key.clone(), affix).is_some() {
                return reference_error(format!(
                    "duplicate relic main affix group={} property={}",
                    key.0, key.1
                ));
            }
        }
        let mut achievement_ids = BTreeSet::new();
        for achievement_id in snapshot.achievement_ids {
            if achievement_id == 0 {
                return reference_error("achievement ID must not be zero".to_string());
            }
            if !achievement_ids.insert(achievement_id) {
                return reference_error(format!(
                    "duplicate achievementId={achievement_id} in achievements"
                ));
            }
        }

        for gear in gear_pieces.values() {
            if gear.main_affix_group != 0
                && !relic_main_affixes
                    .keys()
                    .any(|(group_id, _)| *group_id == gear.main_affix_group)
            {
                return reference_error(format!(
                    "gear gameId={} references missing main affix group={}",
                    gear.game_id, gear.main_affix_group
                ));
            }
        }

        ensure_unique_keys(
            "characters",
            characters.values().map(|reference| reference.key.as_str()),
        )?;
        ensure_unique_keys(
            "lightCones",
            light_cones.values().map(|reference| reference.key.as_str()),
        )?;
        ensure_unique_keys(
            "gearPieces",
            gear_pieces.values().map(|reference| reference.key.as_str()),
        )?;
        ensure_unique_keys(
            "stats",
            stats.values().map(|reference| reference.key.as_str()),
        )?;

        Ok(Self {
            schema_version: snapshot.schema_version,
            provider: snapshot.provider,
            revision: snapshot.revision,
            characters,
            light_cones,
            gear_pieces,
            stats,
            relic_main_affixes,
            achievement_ids,
            packet_references: None,
        })
    }

    pub fn with_packet_references(
        mut self,
        packet: crate::packet_reference::PacketReferences,
    ) -> HsrResult<Self> {
        packet.validate(&self)?;
        self.packet_references = Some(packet);
        Ok(self)
    }

    pub fn packet_references(&self) -> Option<&crate::packet_reference::PacketReferences> {
        self.packet_references.as_ref()
    }

    pub fn without_achievements(mut self) -> Self {
        self.achievement_ids.clear();
        self
    }

    pub fn with_achievement_ids(mut self, ids: Vec<u32>) -> HsrResult<Self> {
        let known: BTreeSet<_> = ids.iter().copied().collect();
        if known.is_empty() || known.contains(&0) || known.len() != ids.len() {
            return reference_error("achievement IDs must be nonempty, unique and nonzero".into());
        }
        self.achievement_ids = known;
        Ok(self)
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn has_achievement(&self, achievement_id: u32) -> bool {
        self.achievement_ids.contains(&achievement_id)
    }

    pub fn achievement_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.achievement_ids.iter().copied()
    }

    pub fn achievement_count(&self) -> usize {
        self.achievement_ids.len()
    }

    /// Require a production-sized, property-complete reference cache before a
    /// live scan, packet capture, or manager action. Fixture export intentionally
    /// remains separate so the small deterministic test bundle can still prove
    /// normalization without becoming account-safe.
    pub fn validate_live_complete_profile(&self) -> HsrResult<()> {
        if self.provider != "gilore.ggstarrail-reference" || self.schema_version != 1 {
            return live_profile_error(
                "provider/schema identity is not the audited GIlore v1 boundary",
            );
        }
        let counts = [
            (
                "characters",
                self.characters.len(),
                LIVE_REFERENCE_MIN_CHARACTERS,
            ),
            (
                "lightCones",
                self.light_cones.len(),
                LIVE_REFERENCE_MIN_LIGHT_CONES,
            ),
            (
                "gearPieces",
                self.gear_pieces.len(),
                LIVE_REFERENCE_MIN_GEAR_PIECES,
            ),
            ("stats", self.stats.len(), LIVE_REFERENCE_MIN_STATS),
            (
                "relicMainAffixes",
                self.relic_main_affixes.len(),
                LIVE_REFERENCE_MIN_MAIN_AFFIXES,
            ),
        ];
        if let Some((name, actual, minimum)) = counts
            .into_iter()
            .find(|(_, actual, minimum)| actual < minimum)
        {
            return live_profile_error(format!(
                "collection={name} is incomplete; actual={actual}; requiredMinimum={minimum}"
            ));
        }

        let slots: BTreeSet<_> = self.gear_pieces.values().map(|gear| gear.slot).collect();
        let required_slots = [
            GearSlot::Head,
            GearSlot::Hands,
            GearSlot::Body,
            GearSlot::Feet,
            GearSlot::PlanarSphere,
            GearSlot::LinkRope,
        ];
        if let Some(missing) = required_slots
            .into_iter()
            .find(|slot| !slots.contains(slot))
        {
            return live_profile_error(format!("required relic slot={missing:?} is absent"));
        }

        if let Some(missing) = LIVE_REQUIRED_STAT_KEYS
            .iter()
            .find(|key| !self.stats.contains_key(**key))
        {
            return live_profile_error(format!("required relic property={missing} is absent"));
        }
        let progressed: BTreeSet<&str> = self
            .relic_main_affixes
            .values()
            .map(|affix| affix.property_id.as_str())
            .collect();
        if let Some(missing) = LIVE_REQUIRED_MAIN_STAT_KEYS
            .iter()
            .find(|key| !progressed.contains(**key))
        {
            return live_profile_error(format!(
                "required main-affix progression property={missing} is absent"
            ));
        }
        Ok(())
    }

    pub fn character(&self, game_id: u32) -> Option<&CharacterReference> {
        self.characters.get(&game_id)
    }

    pub fn light_cone(&self, game_id: u32) -> Option<&LightConeReference> {
        self.light_cones.get(&game_id)
    }

    pub fn gear(&self, game_id: u32) -> Option<&GearReference> {
        self.gear_pieces.get(&game_id)
    }

    /// Enumerate every public relic definition in a visible set/slot/rarity
    /// bucket. GIlore intentionally contains multiple definitions in a few
    /// four-star buckets, so callers that also know the main stat should use
    /// [`Self::resolve_gear_by_set_slot_rarity_main_stat`] rather than treating
    /// this collection as a single definition.
    pub fn gear_candidates_by_set_slot_rarity(
        &self,
        set_key: &str,
        slot: GearSlot,
        rarity: u8,
    ) -> Vec<&GearReference> {
        self.gear_pieces
            .values()
            .filter(|reference| {
                reference.set_key == set_key && reference.slot == slot && reference.rarity == rarity
            })
            .collect()
    }

    /// Resolve a public relic definition from only set, slot, and rarity.
    /// This compatibility helper remains deliberately unique-only; current
    /// GIlore data contains legitimate duplicate four-star buckets.
    pub fn gear_by_set_slot_rarity(
        &self,
        set_key: &str,
        slot: GearSlot,
        rarity: u8,
    ) -> Option<&GearReference> {
        exactly_one(self.gear_candidates_by_set_slot_rarity(set_key, slot, rarity))
    }

    /// Resolve a relic using its set, slot, rarity, main stat and level.
    /// A main-affix property can distinguish most duplicate
    /// set/slot/rarity buckets. If two definitions expose the same property and
    /// progression, the narrow visible-equivalence rule is applied; otherwise
    /// ambiguity fails closed.
    pub fn resolve_gear_by_set_slot_rarity_main_stat(
        &self,
        set_key: &str,
        slot: GearSlot,
        rarity: u8,
        stat_key: &str,
        level: u8,
    ) -> Option<&GearReference> {
        self.select_gear_by_main_stat(
            self.gear_candidates_by_set_slot_rarity(set_key, slot, rarity),
            stat_key,
            level,
            None,
        )
    }

    /// Return the main-stat value for a uniquely resolved relic piece. Raw
    /// ratio values in GIlore are converted to percentage points so packet and
    /// screen observations share one public contract.
    /// Any missing, ambiguous, stale, or legacy progression data fails closed.
    pub fn relic_main_stat_value(
        &self,
        set_key: &str,
        slot: GearSlot,
        rarity: u8,
        stat_key: &str,
        level: u8,
    ) -> Option<f64> {
        let gear = self.gear_by_set_slot_rarity(set_key, slot, rarity)?;
        self.relic_main_stat_value_for_piece(gear, stat_key, level)
    }

    /// Return a main-stat value for an already identified public relic
    /// definition. Binding the lookup to its game ID avoids reintroducing the
    /// set/slot/rarity ambiguity after OCR or a manager matcher has established
    /// the exact piece.
    pub fn relic_main_stat_value_for_piece(
        &self,
        gear: &GearReference,
        stat_key: &str,
        level: u8,
    ) -> Option<f64> {
        let cached = self.gear(gear.game_id)?;
        if cached != gear || cached.main_affix_group == 0 || level > cached.max_level {
            return None;
        }
        let affix = self.relic_main_affix_for_piece(cached, stat_key)?;
        if level > affix.max_level {
            return None;
        }
        let raw = *affix.level_values.get(usize::from(level))?;
        let display = match self.stat(stat_key)?.value_kind {
            StatValueKind::Flat => raw,
            StatValueKind::Ratio => raw * 100.0,
            StatValueKind::Unknown => return None,
        };
        Some(if display == -0.0 { 0.0 } else { display })
    }

    /// Check that a captured main-stat number agrees with the public
    /// progression for an identified piece. HSR renders flat main stats as an
    /// integer and ratios with one decimal place; both truncation and rounding
    /// are accepted because OCR does not retain the UI formatter's fractional
    /// provenance. Candidate selection remains unique, so this tolerance can
    /// only turn uncertainty into a fail-closed ambiguity.
    pub fn relic_main_stat_display_matches_for_piece(
        &self,
        gear: &GearReference,
        stat_key: &str,
        level: u8,
        displayed_value: f64,
    ) -> bool {
        let Some(expected) = self.relic_main_stat_value_for_piece(gear, stat_key, level) else {
            return false;
        };
        let Some(stat) = self.stat(stat_key) else {
            return false;
        };
        main_stat_display_matches(expected, displayed_value, stat.value_kind)
    }

    /// Validate a supplied public piece ID against the visible main-stat
    /// evidence and return the canonical definition for its observational
    /// equivalence class. This is the shared boundary for fixture, screen, and
    /// packet observations: an alternate special definition such as 55001 is
    /// normalized to 51013 only when name, icon, and the full observed-property
    /// progression are identical.
    pub fn canonical_gear_for_observation(
        &self,
        piece_id: u32,
        stat_key: &str,
        level: u8,
        displayed_value: f64,
    ) -> Option<&GearReference> {
        let supplied = self.gear(piece_id)?;
        if !self.relic_main_stat_display_matches_for_piece(
            supplied,
            stat_key,
            level,
            displayed_value,
        ) {
            return None;
        }
        let supplied_progression = self.relic_main_affix_for_piece(supplied, stat_key)?;
        self.gear_candidates_by_set_slot_rarity(&supplied.set_key, supplied.slot, supplied.rarity)
            .into_iter()
            .filter(|candidate| visible_gear_identity_eq(supplied, candidate))
            .filter(|candidate| {
                self.relic_main_affix_for_piece(candidate, stat_key)
                    .is_some_and(|progression| {
                        main_affix_progression_eq(supplied_progression, progression)
                    })
            })
            .min_by_key(|candidate| candidate.game_id)
    }

    pub fn stat(&self, key: &str) -> Option<&StatReference> {
        self.stats.get(key)
    }

    pub fn resolve_character_name(&self, text: &str) -> Option<&CharacterReference> {
        unique_best_name(text, self.characters.values(), |entry| &entry.name)
    }

    /// Short equipment labels provide too little text for fuzzy matching:
    /// a stray OCR glyph must not become a one-character character name.
    pub fn resolve_character_name_exact(&self, text: &str) -> Option<&CharacterReference> {
        let text = normalize_name(text);
        if text.is_empty() {
            return None;
        }
        exactly_one(
            self.characters
                .values()
                .filter(|entry| {
                    text == normalize_name(&entry.name.zh_cn)
                        || text == normalize_name(&entry.name.en)
                })
                .collect(),
        )
    }

    /// Enumerate best-scoring character-name candidates. Multiple public
    /// variants such as March 7th deliberately remain present until visible
    /// Path evidence is applied.
    pub fn character_name_candidates(&self, text: &str) -> Vec<&CharacterReference> {
        best_name_candidates(text, self.characters.values(), |entry| &entry.name)
    }

    /// Resolve a character header using both its bilingual visible name and
    /// Path label. Odd/even Trailblazer definitions remain ambiguous because
    /// they share both pieces of visible evidence.
    pub fn resolve_character_name_and_path(
        &self,
        name: &str,
        path: &str,
    ) -> Option<&CharacterReference> {
        exactly_one(
            self.character_name_candidates(name)
                .into_iter()
                .filter(|entry| character_path_matches(&entry.path, path))
                .collect(),
        )
    }

    /// Resolve the character-details header (`Path／Name`). Trailblazer
    /// templates are named `{NICKNAME}`: they answer to the configured
    /// nickname for the configured gender only, and never match without one.
    pub fn resolve_character_header(
        &self,
        name: &str,
        path: Option<&str>,
        trailblazer: Option<&TrailblazerIdentity>,
    ) -> Option<&CharacterReference> {
        let nickname = trailblazer.map(|identity| BilingualName {
            zh_cn: identity.nickname.clone(),
            en: identity.nickname.clone(),
        });
        let visible: Vec<(&CharacterReference, &BilingualName)> = self
            .characters
            .values()
            .filter_map(|entry| {
                if entry.name.zh_cn != TRAILBLAZER_NAME_PLACEHOLDER {
                    return Some((entry, &entry.name));
                }
                let identity = trailblazer?;
                (TrailblazerGender::of(entry.game_id) == Some(identity.gender))
                    .then_some((entry, nickname.as_ref()?))
            })
            .collect();
        let direct = exactly_one(
            best_name_candidates(name, &visible, |entry| entry.1)
                .into_iter()
                .map(|entry| entry.0)
                .filter(|entry| path.is_none_or(|path| character_path_matches(&entry.path, path)))
                .collect(),
        );
        if direct.is_some() || path.is_some() {
            return direct;
        }
        // OCR may retain Path and Name while dropping only their slash.
        // Reuse the canonical path labels and require an exact normalized
        // suffix name. Do not fuzzily guess a split or override a direct name.
        let mut joined = BTreeMap::new();
        for (entry, visible_name) in visible {
            for label in character_path_labels(&entry.path) {
                if let Some(suffix) = name.trim().strip_prefix(label) {
                    let suffix = normalize_name(suffix);
                    if !suffix.is_empty()
                        && (suffix == normalize_name(&visible_name.zh_cn)
                            || suffix == normalize_name(&visible_name.en))
                    {
                        joined.insert(entry.game_id, entry);
                    }
                }
            }
        }
        exactly_one(joined.into_values().collect())
    }

    pub fn resolve_light_cone_name(&self, text: &str) -> Option<&LightConeReference> {
        unique_best_name(text, self.light_cones.values(), |entry| &entry.name)
    }

    pub fn resolve_gear_name(&self, text: &str, rarity: u8) -> Option<&GearReference> {
        exactly_one(self.gear_name_candidates(text, rarity))
    }

    /// Return every best-scoring public relic definition for the visible name
    /// and rarity. The result is stable by public numeric ID and deliberately
    /// retains truly duplicate definitions for main-affix disambiguation.
    pub fn gear_name_candidates(&self, text: &str, rarity: u8) -> Vec<&GearReference> {
        best_name_candidates(
            text,
            self.gear_pieces
                .values()
                .filter(|entry| entry.rarity == rarity),
            |entry| &entry.name,
        )
    }

    /// Resolve a screen-captured relic from its name/rarity and main-stat
    /// property/value. A result is returned only when public progression makes
    /// one definition provable (or the definitions are canonically visible-
    /// equivalent); stale values and unresolved ambiguity return `None`.
    pub fn resolve_gear_name_with_main_stat(
        &self,
        text: &str,
        rarity: u8,
        stat_key: &str,
        level: u8,
        displayed_value: f64,
    ) -> Option<&GearReference> {
        self.select_gear_by_main_stat(
            self.gear_name_candidates(text, rarity),
            stat_key,
            level,
            Some(displayed_value),
        )
    }

    pub fn resolve_gear_name_any_rarity(&self, text: &str) -> Option<&GearReference> {
        // Collapse rarity variants by logical piece identity before applying
        // the uniqueness gate used for semantic panel discovery.
        let mut logical = BTreeMap::new();
        for entry in self.gear_pieces.values() {
            logical
                .entry((entry.set_key.as_str(), entry.slot))
                .or_insert(entry);
        }
        unique_best_name(text, logical.values().copied(), |entry| &entry.name)
    }

    pub fn resolve_stat_name(&self, text: &str) -> Option<&StatReference> {
        unique_best_name(text, self.stats.values(), |entry| &entry.name)
    }

    /// Resolve a colliding visible stat label using the percentage marker (or
    /// lack of one) as independent value-kind evidence.
    pub fn resolve_stat_name_by_kind(
        &self,
        text: &str,
        value_kind: StatValueKind,
    ) -> Option<&StatReference> {
        if value_kind == StatValueKind::Unknown {
            return None;
        }
        unique_best_name(
            text,
            self.stats
                .values()
                .filter(|entry| entry.value_kind == value_kind),
            |entry| &entry.name,
        )
    }

    fn select_gear_by_main_stat<'a>(
        &'a self,
        candidates: impl IntoIterator<Item = &'a GearReference>,
        stat_key: &str,
        level: u8,
        displayed_value: Option<f64>,
    ) -> Option<&'a GearReference> {
        let compatible: Vec<_> = candidates
            .into_iter()
            .filter(|gear| {
                displayed_value.map_or_else(
                    || {
                        self.relic_main_stat_value_for_piece(gear, stat_key, level)
                            .is_some()
                    },
                    |displayed| {
                        self.relic_main_stat_display_matches_for_piece(
                            gear, stat_key, level, displayed,
                        )
                    },
                )
            })
            .collect();
        exactly_one(compatible.clone())
            .or_else(|| canonical_visibly_equivalent_gear(self, &compatible, stat_key))
    }

    fn relic_main_affix_for_piece(
        &self,
        gear: &GearReference,
        stat_key: &str,
    ) -> Option<&RelicMainAffixReference> {
        let affix = self
            .relic_main_affixes
            .get(&(gear.main_affix_group, stat_key.to_string()))?;
        (affix.max_level == gear.max_level).then_some(affix)
    }
}

fn validate_snapshot_header(snapshot: &ReferenceSnapshot) -> HsrResult<()> {
    if snapshot.schema_version != REFERENCE_SCHEMA_VERSION {
        return reference_error(format!(
            "unsupported schemaVersion={}; expected={REFERENCE_SCHEMA_VERSION}",
            snapshot.schema_version
        ));
    }
    require_text("provider", &snapshot.provider)?;
    require_text("revision", &snapshot.revision)?;
    require_safe_identifier("provider", &snapshot.provider)?;
    require_safe_identifier("revision", &snapshot.revision)
}

fn validate_character(reference: &CharacterReference) -> HsrResult<()> {
    validate_common_reference(
        "character",
        reference.game_id,
        &reference.key,
        &reference.name,
        reference.rarity,
    )?;
    require_text("character.path", &reference.path)
}

fn validate_light_cone(reference: &LightConeReference) -> HsrResult<()> {
    validate_common_reference(
        "lightCone",
        reference.game_id,
        &reference.key,
        &reference.name,
        reference.rarity,
    )?;
    require_text("lightCone.path", &reference.path)
}

fn validate_gear(reference: &GearReference) -> HsrResult<()> {
    validate_common_reference(
        "gear",
        reference.game_id,
        &reference.key,
        &reference.name,
        reference.rarity,
    )?;
    require_text("gear.setKey", &reference.set_key)?;
    if reference.main_affix_group != 0 {
        require_text("gear.iconPath", &reference.icon_path)?;
    }
    if !reference.set_name.is_complete() {
        return reference_error(format!(
            "gear gameId={} has an incomplete bilingual setName",
            reference.game_id
        ));
    }
    if reference.slot.category() != reference.category {
        return reference_error(format!(
            "gear gameId={} category does not match slot {:?}",
            reference.game_id, reference.slot
        ));
    }
    if (reference.main_affix_group == 0) != (reference.max_level == 0) {
        return reference_error(format!(
            "gear gameId={} must provide both mainAffixGroup and maxLevel",
            reference.game_id
        ));
    }
    Ok(())
}

fn validate_stat(reference: &StatReference) -> HsrResult<()> {
    require_text("stat.key", &reference.key)?;
    if !reference.name.is_complete() {
        return reference_error(format!(
            "stat key={} has an incomplete bilingual name",
            reference.key
        ));
    }
    Ok(())
}

fn validate_relic_main_affix(
    reference: &RelicMainAffixReference,
    stats: &BTreeMap<String, StatReference>,
) -> HsrResult<()> {
    if reference.group_id == 0 {
        return reference_error("relic main affix groupId must not be zero".to_string());
    }
    require_text("relicMainAffix.propertyId", &reference.property_id)?;
    let stat = stats.get(&reference.property_id).ok_or_else(|| {
        HsrError::new(
            "HSR-REF-INVALID",
            hints::REFERENCE_INVALID,
            format!(
                "relic main affix references unknown property={}",
                reference.property_id
            ),
        )
    })?;
    if stat.value_kind == StatValueKind::Unknown {
        return reference_error(format!(
            "relic main affix property={} has unknown value kind",
            reference.property_id
        ));
    }
    if reference.level_values.len() != usize::from(reference.max_level) + 1 {
        return reference_error(format!(
            "relic main affix group={} property={} has {} values for maxLevel={}",
            reference.group_id,
            reference.property_id,
            reference.level_values.len(),
            reference.max_level
        ));
    }
    if reference
        .level_values
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return reference_error(format!(
            "relic main affix group={} property={} contains an invalid value",
            reference.group_id, reference.property_id
        ));
    }
    Ok(())
}

fn collect_unique_string<T, F, V>(
    values: Vec<T>,
    collection: &str,
    key: F,
    validate: V,
) -> HsrResult<BTreeMap<String, T>>
where
    F: Fn(&T) -> String,
    V: Fn(&T) -> HsrResult<()>,
{
    let mut result = BTreeMap::new();
    for value in values {
        validate(&value)?;
        let entry_key = key(&value);
        if result.insert(entry_key.clone(), value).is_some() {
            return reference_error(format!("duplicate key={entry_key} in {collection}"));
        }
    }
    Ok(result)
}

fn validate_common_reference(
    kind: &str,
    game_id: u32,
    key: &str,
    name: &crate::localization::BilingualName,
    rarity: u8,
) -> HsrResult<()> {
    if game_id == 0 {
        return reference_error(format!("{kind} gameId must not be zero"));
    }
    require_text(&format!("{kind}.key"), key)?;
    if !name.is_complete() {
        return reference_error(format!(
            "{kind} gameId={game_id} has an incomplete bilingual name"
        ));
    }
    if rarity == 0 {
        return reference_error(format!("{kind} gameId={game_id} rarity must not be zero"));
    }
    Ok(())
}

fn collect_unique<T, F, V>(
    values: Vec<T>,
    collection: &str,
    key: F,
    validate: V,
) -> HsrResult<BTreeMap<u32, T>>
where
    F: Fn(&T) -> u32,
    V: Fn(&T) -> HsrResult<()>,
{
    let mut result = BTreeMap::new();
    for value in values {
        validate(&value)?;
        let game_id = key(&value);
        if result.insert(game_id, value).is_some() {
            return reference_error(format!("duplicate gameId={game_id} in {collection}"));
        }
    }
    Ok(result)
}

fn require_text(field: &str, value: &str) -> HsrResult<()> {
    if value.trim().is_empty() {
        reference_error(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn require_safe_identifier(field: &str, value: &str) -> HsrResult<()> {
    if value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        reference_error(format!(
            "{field} must be a 1-128 character ASCII identifier"
        ))
    } else {
        Ok(())
    }
}

fn ensure_unique_keys<'a>(
    collection: &str,
    keys: impl IntoIterator<Item = &'a str>,
) -> HsrResult<()> {
    let mut seen = std::collections::BTreeSet::new();
    for key in keys {
        if !seen.insert(key) {
            return reference_error(format!("duplicate key={key} in {collection}"));
        }
    }
    Ok(())
}

fn reference_error<T>(detail: String) -> HsrResult<T> {
    Err(HsrError::new(
        "HSR-REF-INVALID",
        hints::REFERENCE_INVALID,
        detail,
    ))
}

fn live_profile_error<T>(detail: impl Into<String>) -> HsrResult<T> {
    Err(HsrError::new(
        "HSR-REF-LIVE-INCOMPLETE",
        hints::REFERENCE_INVALID,
        detail,
    ))
}

fn exactly_one<T>(mut values: Vec<T>) -> Option<T> {
    if values.len() == 1 {
        values.pop()
    } else {
        None
    }
}

fn main_stat_display_matches(expected: f64, displayed: f64, value_kind: StatValueKind) -> bool {
    if !expected.is_finite() || !displayed.is_finite() || displayed < 0.0 {
        return false;
    }
    const EPSILON: f64 = 1e-6;
    let close = |left: f64, right: f64| (left - right).abs() <= EPSILON;
    if close(expected, displayed) {
        return true;
    }
    match value_kind {
        StatValueKind::Flat => {
            close(expected.trunc(), displayed) || close(expected.round(), displayed)
        },
        StatValueKind::Ratio => {
            let truncated = (expected * 10.0).trunc() / 10.0;
            let rounded = (expected * 10.0).round() / 10.0;
            close(truncated, displayed) || close(rounded, displayed)
        },
        StatValueKind::Unknown => false,
    }
}

fn canonical_visibly_equivalent_gear<'a>(
    references: &ReferenceCache,
    candidates: &[&'a GearReference],
    stat_key: &str,
) -> Option<&'a GearReference> {
    let first = *candidates.first()?;
    let first_progression = references.relic_main_affix_for_piece(first, stat_key)?;
    let one_visible_class = candidates.iter().copied().all(|candidate| {
        visible_gear_identity_eq(first, candidate)
            && references
                .relic_main_affix_for_piece(candidate, stat_key)
                .is_some_and(|progression| {
                    main_affix_progression_eq(first_progression, progression)
                })
    });
    one_visible_class
        .then(|| candidates.iter().copied().min_by_key(|entry| entry.game_id))
        .flatten()
}

fn visible_gear_identity_eq(left: &GearReference, right: &GearReference) -> bool {
    left.set_key == right.set_key
        && left.slot == right.slot
        && left.rarity == right.rarity
        && left.name == right.name
        && left.icon_path == right.icon_path
}

fn main_affix_progression_eq(
    left: &RelicMainAffixReference,
    right: &RelicMainAffixReference,
) -> bool {
    left.max_level == right.max_level && left.level_values == right.level_values
}

fn unique_best_name<'a, T: 'a>(
    observed: &str,
    candidates: impl IntoIterator<Item = &'a T>,
    name: impl Fn(&T) -> &crate::localization::BilingualName,
) -> Option<&'a T> {
    exactly_one(best_name_candidates(observed, candidates, name))
}

fn best_name_candidates<'a, T: 'a>(
    observed: &str,
    candidates: impl IntoIterator<Item = &'a T>,
    name: impl Fn(&T) -> &crate::localization::BilingualName,
) -> Vec<&'a T> {
    let observed = normalize_name(observed);
    if observed.is_empty() {
        return Vec::new();
    }

    let mut scored = Vec::new();
    for candidate in candidates {
        let bilingual = name(candidate);
        let Some(score) = [bilingual.zh_cn.as_str(), bilingual.en.as_str()]
            .into_iter()
            .map(normalize_name)
            .filter(|value| !value.is_empty())
            .map(|value| {
                if value == observed {
                    (0, value.chars().count())
                } else if value.contains(&observed) || observed.contains(&value) {
                    (
                        value.chars().count().abs_diff(observed.chars().count()),
                        value.chars().count(),
                    )
                } else {
                    (levenshtein_chars(&value, &observed), value.chars().count())
                }
            })
            .min_by_key(|(distance, _)| *distance)
        else {
            continue;
        };
        scored.push((candidate, score.0, score.1));
    }

    let Some(best_distance) = scored.iter().map(|(_, distance, _)| *distance).min() else {
        return Vec::new();
    };
    // Exact/substring matches are accepted. Fuzzy matching is deliberately
    // conservative and must be unique: at most 25% of the longer name, capped
    // at three characters to avoid silently turning noisy OCR into identity.
    let observed_len = observed.chars().count();
    scored
        .into_iter()
        .filter_map(|(candidate, distance, name_len)| {
            let allowance = observed_len.max(name_len).div_ceil(4).min(3);
            (distance == best_distance && distance <= allowance).then_some(candidate)
        })
        .collect()
}

/// Reference names carry client rich-text markup (`银狼LV.<unbreak>999</unbreak>`)
/// that is never rendered, so tags are dropped before comparing with OCR.
fn normalize_name(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {},
            _ if character.is_alphanumeric() => normalized.extend(character.to_lowercase()),
            _ => {},
        }
    }
    normalized
}

fn character_path_matches(canonical_path: &str, observed_path: &str) -> bool {
    let observed = normalize_name(observed_path);
    if observed.is_empty() {
        return false;
    }
    character_path_labels(canonical_path)
        .iter()
        .map(|label| normalize_name(label))
        .any(|label| label == observed)
}

fn character_path_labels(canonical_path: &str) -> &'static [&'static str] {
    match canonical_path {
        "Warrior" => &["Warrior", "Destruction", "The Destruction", "毁灭", "毀滅"],
        "Rogue" => &["Rogue", "Hunt", "The Hunt", "巡猎", "巡獵"],
        "Mage" => &["Mage", "Erudition", "The Erudition", "智识", "智識"],
        "Shaman" => &["Shaman", "Harmony", "The Harmony", "同谐", "同諧"],
        "Warlock" => &["Warlock", "Nihility", "The Nihility", "虚无", "虛無"],
        "Knight" => &["Knight", "Preservation", "The Preservation", "存护", "存護"],
        "Priest" => &["Priest", "Abundance", "The Abundance", "丰饶", "豐饒"],
        "Memory" => &["Memory", "Remembrance", "The Remembrance", "记忆", "記憶"],
        "Elation" => &["Elation", "The Elation", "欢愉", "歡愉"],
        _ => &[],
    }
}

fn levenshtein_chars(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right_chars.iter().enumerate() {
            current.push(
                (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(previous[right_index] + usize::from(left_char != *right_char)),
            );
        }
        previous = current;
    }
    previous[right_chars.len()]
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn levenshtein_is_unicode_character_based() {
        assert_eq!(levenshtein_chars("暴击率", "暴擊率"), 1);
        assert_eq!(levenshtein_chars("March7th", "March7th"), 0);
    }

    fn header_id(
        references: &ReferenceCache,
        name: &str,
        path: Option<&str>,
        trailblazer: Option<&TrailblazerIdentity>,
    ) -> Option<u32> {
        references
            .resolve_character_header(name, path, trailblazer)
            .map(|entry| entry.game_id)
    }

    #[test]
    fn character_header_without_slash_uses_exact_path_and_name_evidence() {
        let references = crate::load_embedded_gilore_reference().unwrap();
        assert_eq!(header_id(&references, "毁灭雪衣", None, None), Some(1214));
        assert_eq!(
            header_id(&references, "Destruction Xueyi", None, None),
            Some(1214)
        );
        assert_eq!(header_id(&references, "雪衣", None, None), Some(1214));
        assert_eq!(header_id(&references, "存护雪衣", None, None), None);
        assert_eq!(header_id(&references, "毁灭雪", None, None), None);
        assert_eq!(header_id(&references, "巡猎三月七", None, None), Some(1224));
        assert_eq!(header_id(&references, "三月七", None, None), None);
    }

    #[test]
    fn rich_text_markup_does_not_hide_silver_wolf_lv999() {
        let references = crate::load_embedded_gilore_reference().unwrap();

        assert_eq!(
            header_id(&references, "银狼LV.999", Some("欢愉"), None),
            Some(1506)
        );
        assert_eq!(header_id(&references, "银狼LV.999", None, None), Some(1506));
        assert_eq!(
            header_id(&references, "银狼", Some("虚无"), None),
            Some(1006)
        );
    }

    #[test]
    fn trailblazer_nickname_resolves_by_gender_and_path() {
        let references = crate::load_embedded_gilore_reference().unwrap();
        let stelle = TrailblazerIdentity {
            nickname: "青雀又又又和了".to_string(),
            gender: TrailblazerGender::Stelle,
        };
        let caelus = TrailblazerIdentity {
            gender: TrailblazerGender::Caelus,
            ..stelle.clone()
        };

        assert_eq!(
            header_id(&references, "青雀又又又和了", Some("欢愉"), Some(&stelle)),
            Some(8010)
        );
        assert_eq!(
            header_id(&references, "青雀又又又和了", Some("毁灭"), Some(&stelle)),
            Some(8002)
        );
        assert_eq!(
            header_id(&references, "青雀又又又和了", Some("欢愉"), Some(&caelus)),
            Some(8009)
        );
        assert_eq!(
            header_id(&references, "青雀又又又和了", Some("欢愉"), None),
            None
        );
        assert_eq!(header_id(&references, "NICKNAME", Some("欢愉"), None), None);
        assert_eq!(
            header_id(&references, "青雀", Some("智识"), Some(&stelle)),
            Some(1201)
        );
    }
}

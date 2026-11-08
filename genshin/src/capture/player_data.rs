/// Converts captured packet data into GOOD v3 export format.
///
/// Ported from irminsul's `player_data.rs`, adapted to output yas's own GOOD models.
use std::collections::HashMap;

use auto_artifactarium::r#gen::protos::{AvatarInfo, Item};
use indexmap::IndexMap;

use crate::scanner::common::character_element::{
    good_element_from_skill_depot, MULTI_ELEMENT_CHARACTERS,
};
use crate::scanner::common::models::{
    GoodArtifact, GoodCharacter, GoodExport, GoodSubStat, GoodTalent, GoodWeapon,
};

use crate::game_data::types::{to_good_key, DataCache, Property, SkillType};

// --- Game-internal property IDs (content IDs, stable within a version) ---
// These come from the game's `AvatarInfo.prop_map` and may shift across major versions.
const PROP_LEVEL: u32 = 4001;
const PROP_ASCENSION: u32 = 1002;
// `avatar_type == 1` means a formal (playable) character, filtering out trial/test avatars.
const AVATAR_TYPE_FORMAL: u32 = 1;

/// Settings for filtering exported data.
///
/// Rarity rules are hardcoded (not user-configurable):
/// - Weapons: 3★ and above
/// - Artifacts: 4★ and above, including 4★ drops of 5★ sets
#[derive(Clone, Debug)]
pub struct CaptureExportSettings {
    pub include_characters: bool,
    pub include_artifacts: bool,
    pub include_weapons: bool,
    pub include_achievements: bool,

    pub min_character_level: u32,
    pub min_character_ascension: u32,
    pub min_character_constellation: u32,

    pub min_artifact_level: u32,

    pub min_weapon_level: u32,
    pub min_weapon_refinement: u32,
    pub min_weapon_ascension: u32,
}

impl Default for CaptureExportSettings {
    fn default() -> Self {
        Self {
            include_characters: true,
            include_artifacts: true,
            include_weapons: true,
            include_achievements: true,
            min_character_level: 0,
            min_character_ascension: 0,
            min_character_constellation: 0,
            min_artifact_level: 0,
            min_weapon_level: 0,
            min_weapon_refinement: 0,
            min_weapon_ascension: 0,
        }
    }
}

/// Minimum weapon rarity to export (3★+).
const MIN_WEAPON_RARITY: u32 = 3;
/// Minimum artifact rarity to export (4★+).
const MIN_ARTIFACT_RARITY: u32 = 4;

/// Accumulates packet data and exports to GOOD format.
pub struct PlayerData {
    data_cache: DataCache,
    characters: Vec<AvatarInfo>,
    items: Vec<Item>,
    completed_achievement_ids: Vec<u32>,
    character_equip_guid_map: HashMap<u64, u32>,
}

impl PlayerData {
    pub fn new(data_cache: DataCache) -> Self {
        Self {
            data_cache,
            characters: Vec::new(),
            items: Vec::new(),
            completed_achievement_ids: Vec::new(),
            character_equip_guid_map: HashMap::new(),
        }
    }

    /// Clear data accumulated by the previous capture session.
    pub fn begin_capture(&mut self) {
        self.characters.clear();
        self.items.clear();
        self.completed_achievement_ids.clear();
        self.character_equip_guid_map.clear();
    }

    /// Count characters (formal avatars only, avatar_type == 1).
    pub fn character_count(&self) -> usize {
        self.characters
            .iter()
            .filter(|c| c.avatar_type == AVATAR_TYPE_FORMAL)
            .count()
    }

    /// Count weapons among captured items.
    pub fn weapon_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.has_equip() && item.equip().has_weapon())
            .count()
    }

    /// Count artifacts (reliquaries) among captured items.
    pub fn artifact_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.has_equip() && item.equip().has_reliquary())
            .count()
    }

    pub fn achievement_count(&self) -> usize {
        self.completed_achievement_ids.len()
    }

    pub fn process_achievements(&mut self, completed_ids: &[u32]) {
        // Achievement notifications may be split. Merge them just like store
        // item batches so a later packet cannot discard earlier completions.
        self.completed_achievement_ids
            .extend_from_slice(completed_ids);
        self.completed_achievement_ids.sort_unstable();
        self.completed_achievement_ids.dedup();
    }

    pub fn process_characters(&mut self, avatars: &[AvatarInfo]) {
        self.characters = avatars.to_vec();
        self.rebuild_equipment_locations();
    }

    pub fn merge_characters(&mut self, avatars: &[AvatarInfo]) {
        for avatar in avatars {
            if let Some(old) = self
                .characters
                .iter_mut()
                .find(|old| old.guid == avatar.guid && old.avatar_id == avatar.avatar_id)
            {
                *old = avatar.clone();
            } else {
                self.characters.push(avatar.clone());
            }
        }
        self.rebuild_equipment_locations();
    }

    fn rebuild_equipment_locations(&mut self) {
        self.character_equip_guid_map.clear();
        for avatar in &self.characters {
            for guid in &avatar.equip_guid_list {
                self.character_equip_guid_map
                    .insert(*guid, avatar.avatar_id);
            }
        }
    }

    pub fn process_items(&mut self, items: &[Item]) {
        // Store categories may arrive in separate notifications. Merge by GUID
        // so a later weapon batch cannot discard an earlier artifact batch (or
        // vice versa), while still replacing refreshed data for the same item.
        let mut index_by_guid: HashMap<u64, usize> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| (item.guid, index))
            .collect();
        for item in items {
            if let Some(index) = index_by_guid.get(&item.guid).copied() {
                self.items[index] = item.clone();
            } else {
                index_by_guid.insert(item.guid, self.items.len());
                self.items.push(item.clone());
            }
        }
    }

    pub fn export(&self, settings: &CaptureExportSettings) -> anyhow::Result<GoodExport> {
        let characters = if settings.include_characters {
            Some(self.export_characters(settings))
        } else {
            None
        };

        let weapons = if settings.include_weapons {
            Some(self.export_weapons(settings))
        } else {
            None
        };

        let artifacts = if settings.include_artifacts {
            Some(self.export_artifacts(settings))
        } else {
            None
        };

        let achievements = if settings.include_achievements {
            Some(self.completed_achievement_ids.clone())
        } else {
            None
        };

        Ok(GoodExport {
            format: "GOOD".to_string(),
            version: 3,
            source: "yas-GOODScanner".to_string(),
            metadata: None,
            characters,
            weapons,
            artifacts,
            achievements,
        })
    }

    fn resolve_location(&self, guid: u64) -> String {
        self.character_equip_guid_map
            .get(&guid)
            .and_then(|id| {
                self.data_cache
                    .get_character(*id)
                    .map(|name| to_good_key(name))
            })
            .unwrap_or_default()
    }

    fn export_characters(&self, settings: &CaptureExportSettings) -> Vec<GoodCharacter> {
        self.characters
            .iter()
            .filter_map(|character| {
                // Filter to formal avatars only (type 1)
                if character.avatar_type != AVATAR_TYPE_FORMAL {
                    return None;
                }

                let name = capture_lookup(
                    self.data_cache.get_character(character.avatar_id),
                    "character",
                    character.avatar_id,
                )?;
                let level = capture_lookup(
                    character.prop_map.get(&PROP_LEVEL),
                    "character level",
                    PROP_LEVEL,
                )?
                .val as u32;
                let ascension = capture_lookup(
                    character.prop_map.get(&PROP_ASCENSION),
                    "character ascension",
                    PROP_ASCENSION,
                )?
                .val as u32;
                let constellation = character.talent_id_list.len() as u32;

                let mut auto = 1u32;
                let mut skill = 1u32;
                let mut burst = 1u32;

                for (id, lvl) in &character.skill_level_map {
                    let Some(ty) = self.data_cache.get_skill_type(*id) else {
                        continue;
                    };
                    match ty {
                        SkillType::Auto => auto = *lvl,
                        SkillType::Skill => skill = *lvl,
                        SkillType::Burst => burst = *lvl,
                    }
                }

                if level < settings.min_character_level
                    || ascension < settings.min_character_ascension
                    || constellation < settings.min_character_constellation
                {
                    return None;
                }

                let key = to_good_key(name);
                let element = if MULTI_ELEMENT_CHARACTERS.contains(&key.as_str()) {
                    good_element_from_skill_depot(character.skill_depot_id).map(str::to_string)
                } else {
                    None
                };

                Some(GoodCharacter {
                    key,
                    level: level as i32,
                    constellation: constellation as i32,
                    ascension: ascension as i32,
                    talent: GoodTalent {
                        auto: auto as i32,
                        skill: skill as i32,
                        burst: burst as i32,
                    },
                    element,
                })
            })
            .collect()
    }

    fn export_artifacts(&self, settings: &CaptureExportSettings) -> Vec<GoodArtifact> {
        self.items
            .iter()
            .filter_map(|item| {
                if !item.has_equip() {
                    return None;
                }
                let equip = item.equip();
                let location = self.resolve_location(item.guid);

                if !equip.has_reliquary() {
                    return None;
                }
                let artifact_data = capture_lookup(
                    self.data_cache.get_artifact(item.item_id),
                    "artifact",
                    item.item_id,
                )?;
                let artifact = equip.reliquary();

                // Accumulate substats: group by property, sum values, track initial + per-roll
                let mut substats: IndexMap<Property, (f64, f64, Vec<f64>)> = IndexMap::new();
                for substat_id in artifact.append_prop_id_list.iter() {
                    let substat = capture_lookup(
                        self.data_cache.get_affix(*substat_id),
                        "artifact substat",
                        *substat_id,
                    )?;
                    let entry = substats.entry(substat.property).or_insert((
                        0.0,
                        substat.value,
                        Vec::new(),
                    ));
                    entry.0 += substat.value;
                    entry.2.push(substat.value);
                }
                let substats: Vec<GoodSubStat> = substats
                    .into_iter()
                    .map(|(property, (value, initial_value, rolls))| GoodSubStat {
                        key: property.good_name().to_string(),
                        value: round_stat(property, value),
                        initial_value: Some(round_stat(property, initial_value)),
                        rolls,
                    })
                    .collect();

                let unactivated_substats: Vec<GoodSubStat> = artifact
                    .unactivated_prop_id_list
                    .iter()
                    .filter_map(|substat_id| {
                        let substat = capture_lookup(
                            self.data_cache.get_affix(*substat_id),
                            "artifact substat",
                            *substat_id,
                        )?;
                        let rounded = round_stat(substat.property, substat.value);
                        Some(GoodSubStat {
                            key: substat.property.good_name().to_string(),
                            value: rounded,
                            initial_value: Some(rounded),
                            rolls: vec![substat.value],
                        })
                    })
                    .collect();

                let total_rolls = artifact.append_prop_id_list.len() as i32;
                let level = capture_lookup(
                    artifact.level.checked_sub(1),
                    "artifact level",
                    artifact.level,
                )? as i32;
                let rarity = artifact_data.rarity as i32;
                let astral_mark = artifact.starred;
                let elixir_crafted = !artifact.elixer_choices.is_empty();
                let main_stat_key = capture_lookup(
                    self.data_cache.get_property(artifact.main_prop_id),
                    "artifact main stat",
                    artifact.main_prop_id,
                )?
                .good_name()
                .to_string();

                if (level as u32) < settings.min_artifact_level
                    || (rarity as u32) < MIN_ARTIFACT_RARITY
                {
                    return None;
                }

                Some(GoodArtifact {
                    set_key: to_good_key(&artifact_data.set),
                    slot_key: artifact_data.slot.good_name().to_string(),
                    level,
                    rarity,
                    main_stat_key,
                    substats,
                    location,
                    lock: equip.is_locked,
                    astral_mark,
                    elixir_crafted,
                    unactivated_substats,
                    total_rolls: Some(total_rolls),
                })
            })
            .collect()
    }

    fn export_weapons(&self, settings: &CaptureExportSettings) -> Vec<GoodWeapon> {
        self.items
            .iter()
            .filter_map(|item| {
                if !item.has_equip() {
                    return None;
                }
                let equip = item.equip();
                let location = self.resolve_location(item.guid);

                if !equip.has_weapon() {
                    return None;
                }
                let weapon_data = capture_lookup(
                    self.data_cache.get_weapon(item.item_id),
                    "weapon",
                    item.item_id,
                )?;
                let weapon = equip.weapon();
                let refinement = weapon
                    .affix_map
                    .values()
                    .cloned()
                    .next()
                    .unwrap_or_default()
                    + 1;

                let level = weapon.level;
                let ascension = weapon.promote_level;

                if level < settings.min_weapon_level
                    || refinement < settings.min_weapon_refinement
                    || ascension < settings.min_weapon_ascension
                    || weapon_data.rarity < MIN_WEAPON_RARITY
                {
                    return None;
                }

                Some(GoodWeapon {
                    key: to_good_key(&weapon_data.name),
                    level: level as i32,
                    ascension: ascension as i32,
                    refinement: refinement as i32,
                    rarity: weapon_data.rarity as i32,
                    location,
                    lock: equip.is_locked,
                })
            })
            .collect()
    }
}

/// Missing reference/protocol fields must not silently discard captured records.
fn capture_lookup<T>(value: Option<T>, field: &'static str, id: u32) -> Option<T> {
    if value.is_none() {
        yas::log_warn!(
            "部分抓包数据无法转换：缺少字段或映射 {}，ID={}。请更新抓包器和游戏数据，并查看导出数量。",
            "Some captured data could not be converted: missing field or mapping {}, ID={}. Update the scanner and game data, and check exported counts.",
            field, id,
        );
    }
    value
}

/// Round a stat value the same way the game does (f64 half-up rounding).
///
/// Note: Rust's `f64::round()` is actually round-half-away-from-zero (correct
/// for positive values), but we use the explicit floor(x+0.5) form to make the
/// intent clear. Verified against 274 rounding boundary cases from packet
/// capture vs OCR — the game uses half-up, not banker's rounding.
///
/// Percentages round to 1 decimal; flat stats round to integers.
fn round_stat(property: Property, value: f64) -> f64 {
    if property.is_percentage() {
        (value * 10.0 + 0.5 + 1e-9).floor() / 10.0
    } else {
        (value + 0.5 + 1e-9).floor()
    }
}

#[cfg(test)]
mod tests {
    use auto_artifactarium::r#gen::protos::PropValue;

    use super::*;

    fn captured_character(avatar_id: u32, skill_depot_id: u32) -> AvatarInfo {
        let mut character = AvatarInfo::new();
        character.avatar_id = avatar_id;
        character.avatar_type = AVATAR_TYPE_FORMAL;
        character.skill_depot_id = skill_depot_id;

        let mut level = PropValue::new();
        level.val = 90;
        character.prop_map.insert(PROP_LEVEL, level);

        let mut ascension = PropValue::new();
        ascension.val = 6;
        character.prop_map.insert(PROP_ASCENSION, ascension);
        character
    }

    #[test]
    fn round_stat_percentage() {
        assert_eq!(round_stat(Property::CritRate, 3.89), 3.9);
        assert_eq!(round_stat(Property::CritDamage, 7.75), 7.8);
        assert_eq!(round_stat(Property::HpPercent, 5.25), 5.3);
        assert_eq!(round_stat(Property::AttackPercent, 5.24), 5.2);
    }

    #[test]
    fn round_stat_flat() {
        assert_eq!(round_stat(Property::Hp, 298.7), 299.0);
        assert_eq!(round_stat(Property::Attack, 19.4), 19.0);
        assert_eq!(round_stat(Property::Defense, 23.5), 24.0);
        assert_eq!(round_stat(Property::ElementalMastery, 16.3), 16.0);
    }

    #[test]
    fn round_stat_half_up_not_bankers() {
        // 62.5 must round to 63 (half-up), not 62 (banker's round-to-even)
        // Real case: DEF rolls 16.20 + 23.15 + 23.15 = 62.50
        assert_eq!(round_stat(Property::Defense, 62.5), 63.0);
        // Percentage: 5.25 must round to 5.3 (half-up), not 5.2 (banker's)
        assert_eq!(round_stat(Property::HpPercent, 5.25), 5.3);
    }

    #[test]
    fn export_default_settings() {
        let data_cache = DataCache {
            version: 1,
            git_hash: String::new(),
            affix_map: HashMap::new(),
            artifact_map: HashMap::new(),
            character_map: HashMap::new(),
            material_map: HashMap::new(),
            property_map: HashMap::new(),
            set_map: HashMap::new(),
            skill_type_map: HashMap::new(),
            weapon_map: HashMap::new(),
        };
        let player = PlayerData::new(data_cache);
        let export = player.export(&CaptureExportSettings::default()).unwrap();
        assert_eq!(export.format, "GOOD");
        assert_eq!(export.version, 3);
        assert!(export.metadata.is_none());
        assert!(serde_json::to_value(&export)
            .unwrap()
            .get("metadata")
            .is_none());
        // Empty data → empty lists
        assert_eq!(export.characters.unwrap().len(), 0);
        assert_eq!(export.weapons.unwrap().len(), 0);
        assert_eq!(export.artifacts.unwrap().len(), 0);
        assert_eq!(export.achievements.unwrap().len(), 0);
    }

    #[test]
    fn character_updates_keep_other_characters_and_rebuild_locations() {
        let cache = serde_json::from_str(r#"{"version":1,"git_hash":"test"}"#).unwrap();
        let mut player = PlayerData::new(cache);
        let first = AvatarInfo {
            guid: 1,
            avatar_id: 10000001,
            avatar_type: 1,
            equip_guid_list: vec![10],
            ..Default::default()
        };
        let second = AvatarInfo {
            guid: 2,
            avatar_id: 10000002,
            avatar_type: 1,
            equip_guid_list: vec![20],
            ..Default::default()
        };
        player.process_characters(&[first.clone(), second]);
        let updated = AvatarInfo {
            equip_guid_list: vec![30],
            ..first
        };
        player.merge_characters(&[updated.clone()]);
        player.merge_characters(&[updated]);
        assert_eq!(player.character_count(), 2);
        assert!(!player.character_equip_guid_map.contains_key(&10));
        assert_eq!(player.character_equip_guid_map.get(&20), Some(&10000002));
        assert_eq!(player.character_equip_guid_map.get(&30), Some(&10000001));
        player.begin_capture();
        assert_eq!(player.character_count(), 0);
        assert!(player.character_equip_guid_map.is_empty());
    }

    #[test]
    fn export_keeps_all_four_and_five_star_artifacts_and_three_to_five_star_weapons() {
        use crate::game_data::types::{Artifact, ArtifactSlot, Weapon};
        use auto_artifactarium::r#gen::protos::{Equip, Reliquary, Weapon as ProtoWeapon};

        let mut cache: DataCache =
            serde_json::from_str(r#"{"version":1,"git_hash":"test"}"#).unwrap();
        cache.property_map.insert(1, Property::Hp);
        let mut items = Vec::new();
        for rarity in 1..=5 {
            let mut item = Item::new();
            item.item_id = rarity;
            item.guid = u64::from(rarity);
            cache.artifact_map.insert(
                rarity,
                Artifact {
                    set: "Gladiator's Finale".to_string(),
                    slot: ArtifactSlot::Flower,
                    rarity,
                },
            );
            let mut reliquary = Reliquary::new();
            reliquary.level = 1;
            reliquary.main_prop_id = 1;
            let mut equip = Equip::new();
            equip.set_reliquary(reliquary);
            item.set_equip(equip);
            items.push(item);

            let mut item = Item::new();
            item.item_id = rarity + 10;
            item.guid = u64::from(rarity + 10);
            cache.weapon_map.insert(
                rarity + 10,
                Weapon {
                    name: format!("Test Weapon {}", rarity),
                    rarity,
                },
            );
            let mut weapon = ProtoWeapon::new();
            weapon.level = 1;
            let mut equip = Equip::new();
            equip.set_weapon(weapon);
            item.set_equip(equip);
            items.push(item);
        }
        let mut player = PlayerData::new(cache);
        player.process_items(&items);
        let export = player.export(&CaptureExportSettings::default()).unwrap();
        assert_eq!(
            export
                .artifacts
                .unwrap()
                .iter()
                .map(|a| a.rarity)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
        assert_eq!(
            export
                .weapons
                .unwrap()
                .iter()
                .map(|w| w.rarity)
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn export_respects_include_flags() {
        let data_cache = DataCache {
            version: 1,
            git_hash: String::new(),
            affix_map: HashMap::new(),
            artifact_map: HashMap::new(),
            character_map: HashMap::new(),
            material_map: HashMap::new(),
            property_map: HashMap::new(),
            set_map: HashMap::new(),
            skill_type_map: HashMap::new(),
            weapon_map: HashMap::new(),
        };
        let player = PlayerData::new(data_cache);
        let settings = CaptureExportSettings {
            include_characters: false,
            include_weapons: false,
            include_artifacts: true,
            include_achievements: false,
            ..Default::default()
        };
        let export = player.export(&settings).unwrap();
        assert!(export.characters.is_none());
        assert!(export.weapons.is_none());
        assert!(export.artifacts.is_some());
        assert!(export.achievements.is_none());
    }

    #[test]
    fn export_achievements_are_sorted_and_deduplicated() {
        let data_cache = DataCache {
            version: 1,
            git_hash: String::new(),
            affix_map: HashMap::new(),
            artifact_map: HashMap::new(),
            character_map: HashMap::new(),
            material_map: HashMap::new(),
            property_map: HashMap::new(),
            set_map: HashMap::new(),
            skill_type_map: HashMap::new(),
            weapon_map: HashMap::new(),
        };
        let mut player = PlayerData::new(data_cache);
        player.process_achievements(&[82003, 80001, 82003]);

        let export = player.export(&CaptureExportSettings::default()).unwrap();
        assert_eq!(export.achievements, Some(vec![80001, 82003]));
    }

    #[test]
    fn capture_exports_elements_for_traveler_manekin_and_manekina() {
        let mut character_map = HashMap::new();
        character_map.insert(10000005, "Traveler".to_string());
        character_map.insert(10000007, "Traveler".to_string());
        character_map.insert(10000117, "Manekin".to_string());
        character_map.insert(10000118, "Manekina".to_string());
        let data_cache = DataCache {
            version: 1,
            git_hash: String::new(),
            affix_map: HashMap::new(),
            artifact_map: HashMap::new(),
            character_map,
            material_map: HashMap::new(),
            property_map: HashMap::new(),
            set_map: HashMap::new(),
            skill_type_map: HashMap::new(),
            weapon_map: HashMap::new(),
        };
        let mut player = PlayerData::new(data_cache);
        player.process_characters(&[
            captured_character(10000005, 507),   // male Traveler, Electro
            captured_character(10000007, 708),   // female Traveler, Dendro
            captured_character(10000117, 11706), // Manekin, Anemo
            captured_character(10000118, 11803), // Manekina, Hydro
        ]);

        let characters = player
            .export(&CaptureExportSettings::default())
            .unwrap()
            .characters
            .unwrap();
        let exported: Vec<_> = characters
            .iter()
            .map(|character| (character.key.as_str(), character.element.as_deref()))
            .collect();

        assert_eq!(
            exported,
            vec![
                ("Traveler", Some("Electro")),
                ("Traveler", Some("Dendro")),
                ("Manekin", Some("Anemo")),
                ("Manekina", Some("Hydro")),
            ]
        );
    }
}

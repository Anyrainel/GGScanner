//! Login inventory recognition follows Reliquary's record types, but searches
//! every repeated protobuf field instead of command IDs or outer field tags.
//! Inner records are validated against downloaded public data before acceptance.
use std::collections::{BTreeMap, BTreeSet};

use crate::packet_reference::PacketReferences;
use protobuf::Message;

use super::{
    proto::{Avatar::Avatar, AvatarPathData::AvatarPathData, Equipment::Equipment, Relic::Relic},
    protocol::{containers, parse_message, WireValue},
    CaptureTargets, HSR_CAPTURE_REVISION,
};
use crate::{model::*, reference::ReferenceCache, HsrError, HsrResult, LocalizedText};

pub struct InventoryDecoder {
    pub continuous: bool,
    pub(super) revision: u64,
    references: ReferenceCache,
    targets: CaptureTargets,
    affixes: PacketReferences,
    cones_by_id: BTreeMap<u32, Equipment>,
    relics_by_id: BTreeMap<u32, Relic>,
    pub export_details: crate::scanner_export::CaptureExportDetails,
    pub characters: Option<Vec<ObservedCharacter>>,
    pub light_cones: Option<Vec<ObservedLightCone>>,
    pub relics: Option<Vec<ObservedGear>>,
}

impl InventoryDecoder {
    pub fn new(references: ReferenceCache, targets: CaptureTargets) -> HsrResult<Self> {
        let affixes = references
            .packet_references()
            .cloned()
            .ok_or_else(|| invalid("capture reference has no packet tables"))?;
        Ok(Self {
            continuous: false,
            revision: 0,
            cones_by_id: BTreeMap::new(),
            relics_by_id: BTreeMap::new(),
            references,
            targets,
            affixes,
            export_details: Default::default(),
            characters: None,
            light_cones: None,
            relics: None,
        })
    }

    pub fn reset(&mut self) {
        self.revision = 0;
        self.cones_by_id.clear();
        self.relics_by_id.clear();
        self.export_details = Default::default();
        self.characters = None;
        self.light_cones = None;
        self.relics = None;
    }

    pub fn complete(&self) -> bool {
        (!self.targets.characters || self.characters.is_some())
            && (!self.targets.light_cones || self.light_cones.is_some())
            && (!self.targets.relics || self.relics.is_some())
    }

    pub fn snapshot(&self) -> Option<ObservationSnapshot> {
        if !self.complete() {
            return None;
        }
        Some(ObservationSnapshot {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            evidence: ObservationEvidence {
                kind: EvidenceKind::PacketCapture,
                revision: HSR_CAPTURE_REVISION.to_owned(),
                coverage: InventoryCoverage {
                    characters: if self.targets.characters {
                        CoverageLevel::Complete
                    } else {
                        CoverageLevel::Unknown
                    },
                    light_cones: if self.targets.light_cones {
                        CoverageLevel::Complete
                    } else {
                        CoverageLevel::Unknown
                    },
                    relics: if self.targets.relics {
                        CoverageLevel::Complete
                    } else {
                        CoverageLevel::Unknown
                    },
                },
            },
            characters: self.characters.clone().unwrap_or_default(),
            light_cones: self.light_cones.clone().unwrap_or_default(),
            gear: self.relics.clone().unwrap_or_default(),
        })
    }

    pub fn receive(&mut self, bytes: &[u8]) -> HsrResult<()> {
        if self.complete() && !self.continuous {
            return Ok(());
        }
        for container in containers(bytes) {
            let mut groups: BTreeMap<u32, Vec<&[u8]>> = BTreeMap::new();
            let fields = parse_message(container).unwrap_or_default();
            // GetAvatarDataScRsp carries is_get_all=true; AvatarSync does not.
            // Infer the boolean's position, not its rotating field number.
            let has_get_all = fields
                .iter()
                .any(|field| matches!(field.value, WireValue::Varint(1)));
            for field in fields {
                if let WireValue::Bytes(value) = field.value {
                    groups.entry(field.number).or_default().push(value);
                }
            }
            // A login character response contains both base progression and
            // path records. Do not mistake a single-character update for it.
            let bases = unique_group(&groups, |bytes| {
                let value = Avatar::parse_from_bytes(bytes).ok()?;
                (self.references.character(value.base_avatar_id).is_some()
                    && (1..=80).contains(&value.level)
                    && value.promotion <= 6
                    && value.first_met_time_stamp >= 1_600_000_000)
                    .then_some(value)
            });
            let paths = unique_group(&groups, |bytes| {
                let value = AvatarPathData::parse_from_bytes(bytes).ok()?;
                (self.references.character(value.avatar_id).is_some()
                    && value.rank <= 6
                    && !value.avatar_path_skill_tree.is_empty()
                    && value
                        .avatar_path_skill_tree
                        .iter()
                        .all(|s| s.point_id > 0 && s.level <= 20))
                .then_some(value)
            });
            if self.targets.characters
                && (self.characters.is_none() || self.continuous)
                && (has_get_all || (self.continuous && self.characters.is_some()))
            {
                if let (Some((_, bases)), Some((_, paths))) = (bases, paths) {
                    let bases: BTreeMap<_, _> =
                        bases.into_iter().map(|b| (b.base_avatar_id, b)).collect();
                    for base in bases.values() {
                        self.record_trailblazer_from_base(base)?;
                    }
                    let mut characters = Vec::new();
                    let mut seen = BTreeSet::new();
                    for path in paths {
                        if !seen.insert(path.avatar_id) {
                            return Err(invalid("duplicate character path"));
                        }
                        let base_id = self
                            .affixes
                            .base_avatars
                            .get(&path.avatar_id)
                            .copied()
                            .unwrap_or(path.avatar_id);
                        let base = bases.get(&base_id).ok_or_else(|| {
                            invalid(format!(
                                "character {} has no base progression",
                                path.avatar_id
                            ))
                        })?;
                        self.record_trailblazer_from_path_id(path.avatar_id);
                        self.export_details
                            .characters
                            .insert(path.avatar_id, character_details(&path));
                        characters.push(ObservedCharacter {
                            character_id: path.avatar_id,
                            level: base.level as u8,
                            ascension: base.promotion as u8,
                            eidolon: path.rank as u8,
                        });
                    }
                    self.revision += 1;
                    characters.sort_by_key(|c| c.character_id);
                    if !has_get_all {
                        let roster = self.characters.get_or_insert_with(Vec::new);
                        for character in characters {
                            if let Some(old) = roster
                                .iter_mut()
                                .find(|old| old.character_id == character.character_id)
                            {
                                *old = character;
                            } else {
                                roster.push(character);
                            }
                        }
                        roster.sort_by_key(|c| c.character_id);
                    } else {
                        self.characters = Some(characters);
                    }
                }
            }
            // Require both inventory record families in the same container.
            // Missing collections are never reported as captured empty data.
            if (self.targets.light_cones && (self.light_cones.is_none() || self.continuous))
                || (self.targets.relics && (self.relics.is_none() || self.continuous))
            {
                let cones = unique_group(&groups, |bytes| {
                    let value = Equipment::parse_from_bytes(bytes).ok()?;
                    (self.references.light_cone(value.tid).is_some()
                        && value.unique_id > 0
                        && (1..=80).contains(&value.level)
                        && (1..=5).contains(&value.rank)
                        && value.promotion <= 6)
                        .then_some(value)
                });
                let relics = unique_group(&groups, |bytes| {
                    let value = Relic::parse_from_bytes(bytes).ok()?;
                    (self.references.gear(value.tid).is_some()
                        && value.unique_id > 0
                        && value.main_affix_id > 0
                        && value.level <= 15
                        && value.sub_affix_list.len() <= 4)
                        .then_some(value)
                });
                let full = cones.is_some() && relics.is_some();
                let update = self.continuous
                    && self.light_cones.is_some()
                    && self.relics.is_some()
                    && (cones.is_some() || relics.is_some());
                if full || update {
                    if matches!((&cones, &relics), (Some((a, _)), Some((b, _))) if a == b) {
                        continue;
                    }
                    let mut incoming = BTreeSet::new();
                    for cone in cones.into_iter().flat_map(|(_, values)| values) {
                        if !incoming.insert(cone.unique_id) {
                            return Err(invalid("duplicate light cone instance"));
                        }
                        self.cones_by_id.insert(cone.unique_id, cone);
                    }
                    incoming.clear();
                    for relic in relics.into_iter().flat_map(|(_, values)| values) {
                        if !incoming.insert(relic.unique_id) {
                            return Err(invalid("duplicate relic instance"));
                        }
                        self.relics_by_id.insert(relic.unique_id, relic);
                    }
                    let cones: Vec<_> = self.cones_by_id.values().cloned().collect();
                    let relics: Vec<_> = self.relics_by_id.values().cloned().collect();
                    let mut ids = BTreeSet::new();
                    let mut normalized_cones = Vec::new();
                    for cone in cones.into_iter().filter(|_| self.targets.light_cones) {
                        if !ids.insert(cone.unique_id) {
                            return Err(invalid("duplicate light cone instance"));
                        }
                        normalized_cones.push(ObservedLightCone {
                            light_cone_id: cone.tid,
                            level: cone.level as u8,
                            ascension: cone.promotion as u8,
                            superimposition: cone.rank as u8,
                            equipped_character_id: self.location(cone.equip_avatar_id)?,
                            lock: Some(cone.is_protected),
                        });
                    }
                    ids.clear();
                    let mut normalized_relics = Vec::new();
                    for relic in relics.into_iter().filter(|_| self.targets.relics) {
                        if !ids.insert(relic.unique_id) {
                            return Err(invalid("duplicate relic instance"));
                        }
                        normalized_relics.push(self.relic(relic)?);
                    }
                    self.export_details.instance_ids =
                        Some(crate::scanner_export::CaptureInstanceIds {
                            light_cones: self
                                .cones_by_id
                                .keys()
                                .copied()
                                .filter(|_| self.targets.light_cones)
                                .collect(),
                            relics: self
                                .relics_by_id
                                .keys()
                                .copied()
                                .filter(|_| self.targets.relics)
                                .collect(),
                        });
                    self.revision += 1;
                    if self.targets.light_cones {
                        self.light_cones = Some(normalized_cones);
                    }
                    if self.targets.relics {
                        self.relics = Some(normalized_relics);
                    }
                }
            }
        }
        Ok(())
    }

    fn record_trailblazer_from_base(&mut self, base: &Avatar) -> HsrResult<()> {
        let path_id = base.cur_multi_path_avatar_type;
        let Some(gender) = crate::scanner_export::trailblazer_gender(path_id) else {
            return Ok(());
        };
        let Some(character) = self.references.character(path_id) else {
            return Ok(());
        };
        self.export_details.trailblazer = Some(gender.to_owned());
        self.export_details.current_trailblazer_path =
            Some(crate::scanner_export::path_name(&character.path)?.to_owned());
        Ok(())
    }

    fn record_trailblazer_from_path_id(&mut self, avatar_id: u32) {
        if self.export_details.trailblazer.is_some() {
            return;
        }
        if let Some(gender) = crate::scanner_export::trailblazer_gender(avatar_id) {
            self.export_details.trailblazer = Some(gender.to_owned());
        }
    }

    fn location(&self, id: u32) -> HsrResult<Option<u32>> {
        if id == 0 {
            return Ok(None);
        }
        self.references
            .character(id)
            .map(|_| Some(id))
            .ok_or_else(|| invalid(format!("unknown equipped character {id}")))
    }

    fn relic(&self, relic: Relic) -> HsrResult<ObservedGear> {
        let piece = self
            .references
            .gear(relic.tid)
            .ok_or_else(|| invalid("unknown relic"))?;
        let main = self
            .affixes
            .main
            .iter()
            .find(|a| a.group == piece.main_affix_group && a.id == relic.main_affix_id)
            .ok_or_else(|| invalid(format!("unknown main affix for relic {}", relic.tid)))?;
        let main_value = self
            .references
            .relic_main_stat_value_for_piece(piece, &main.property, relic.level as u8)
            .ok_or_else(|| invalid("main affix progression unavailable"))?;
        let mut substats = Vec::new();
        for sub in relic.sub_affix_list {
            let affix = self
                .affixes
                .sub
                .iter()
                .find(|a| a.group == u32::from(piece.rarity) && a.id == sub.affix_id)
                .ok_or_else(|| invalid(format!("unknown sub affix {}", sub.affix_id)))?;
            if sub.cnt == 0 || sub.cnt > 6 || sub.step > sub.cnt * 2 {
                return Err(invalid("invalid substat roll counts"));
            }
            let stat = self
                .references
                .stat(&affix.property)
                .ok_or_else(|| invalid("unknown substat property"))?;
            let factor = match stat.value_kind {
                StatValueKind::Ratio => 100.0,
                StatValueKind::Flat => 1.0,
                StatValueKind::Unknown => return Err(invalid("unknown substat units")),
            };
            substats.push(ObservedSubstat {
                stat_key: affix.property.clone(),
                value: (f64::from(sub.cnt) * affix.base + f64::from(sub.step) * affix.step)
                    * factor,
            });
        }
        Ok(ObservedGear {
            piece_id: relic.tid,
            level: relic.level as u8,
            main_stat_key: main.property.clone(),
            main_stat_value: main_value,
            substats,
            equipped_character_id: self.location(relic.equip_avatar_id)?,
            lock: Some(relic.is_protected),
            discard: Some(relic.is_discarded),
        })
    }
}

fn unique_group<'a, T>(
    groups: &BTreeMap<u32, Vec<&'a [u8]>>,
    decode: impl Fn(&'a [u8]) -> Option<T>,
) -> Option<(u32, Vec<T>)> {
    let mut found = None;
    for (&tag, records) in groups {
        if let Some(values) = records
            .iter()
            .map(|record| decode(record))
            .collect::<Option<Vec<_>>>()
        {
            if found.is_some() {
                return None;
            }
            found = Some((tag, values));
        }
    }
    found
}

fn invalid(detail: impl Into<String>) -> HsrError {
    HsrError::new("HSR-CAPTURE-INVENTORY", LocalizedText::new(
        "无法识别完整的星穹铁道库存。请刷新游戏数据后重新登录抓包；若仍失败，请更新 GGScanner。",
        "The complete Star Rail inventory could not be decoded. Refresh game data and capture a new login; if it still fails, update GGScanner."), detail)
}

// Current AvatarPathSkillTree uses public point anchors 1-22 (Reliquary v23).
fn character_details(path: &AvatarPathData) -> crate::scanner_export::CharacterDetails {
    let mut result = crate::scanner_export::CharacterDetails {
        ability_version: Some(path.skilltree_version),
        ..Default::default()
    };
    for name in ["basic", "skill", "ult", "talent"] {
        result.skills.insert(name.into(), 0);
    }
    for i in 1..=3 {
        result.traces.insert(format!("ability_{i}"), false);
    }
    for i in 1..=10 {
        result.traces.insert(format!("stat_{i}"), false);
    }
    result.traces.insert("special".into(), false);
    let mut memosprite = BTreeMap::new();
    for point in &path.avatar_path_skill_tree {
        match point.point_id {
            1..=4 => {
                result.skills.insert(
                    ["basic", "skill", "ult", "talent"][(point.point_id - 1) as usize].into(),
                    point.level,
                );
            },
            6..=8 => {
                result
                    .traces
                    .insert(format!("ability_{}", point.point_id - 5), point.level > 0);
            },
            9..=18 => {
                result
                    .traces
                    .insert(format!("stat_{}", point.point_id - 8), point.level > 0);
            },
            19 | 20 => {
                memosprite.insert(
                    if point.point_id == 19 {
                        "skill"
                    } else {
                        "talent"
                    }
                    .into(),
                    point.level,
                );
            },
            21 => {
                result.traces.insert("special".into(), point.level > 0);
            },
            22 => {
                result.skills.insert("elation".into(), point.level);
            },
            _ => {},
        }
    }
    if !memosprite.is_empty() {
        result.memosprite = Some(memosprite);
    }
    result
}

#![cfg(feature = "capture")]

use hsr_scanner::{
    build_export, load_embedded_gilore_reference,
    packet_capture::{
        proto::{
            Avatar::Avatar, AvatarPathData::AvatarPathData,
            AvatarPathSkillTree::AvatarPathSkillTree, Equipment::Equipment, Relic::Relic,
            RelicAffix::RelicAffix,
        },
        CaptureTargets, HsrPacketDecoder,
    },
    ValidatedObservationSnapshot,
};
use protobuf::{CodedOutputStream, Message};

fn group(tag: u32, messages: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut output = CodedOutputStream::vec(&mut bytes);
    for message in messages {
        output.write_bytes(tag, message).unwrap();
    }
    output.flush().unwrap();
    drop(output);
    bytes
}

fn characters(tag: u32, path_tag: u32) -> Vec<u8> {
    character_packet(tag, path_tag, true)
}

fn character_packet(tag: u32, path_tag: u32, get_all: bool) -> Vec<u8> {
    character_packet_for(tag, path_tag, get_all, 1001)
}

fn character_packet_for(tag: u32, path_tag: u32, get_all: bool, character_id: u32) -> Vec<u8> {
    let base = Avatar {
        base_avatar_id: character_id,
        level: 80,
        promotion: 6,
        first_met_time_stamp: 1_700_000_000,
        ..Default::default()
    };
    let path = AvatarPathData {
        avatar_id: character_id,
        rank: 2,
        avatar_path_skill_tree: vec![AvatarPathSkillTree {
            point_id: 1,
            level: 6,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut bytes = group(tag, &[base.write_to_bytes().unwrap()]);
    bytes.extend(group(path_tag, &[path.write_to_bytes().unwrap()]));
    let mut out = CodedOutputStream::vec(&mut bytes);
    out.write_bool(700, get_all).unwrap();
    out.flush().unwrap();
    drop(out);
    bytes
}

#[test]
fn character_sync_is_not_a_complete_roster() {
    let mut decoder = HsrPacketDecoder::new(
        load_embedded_gilore_reference().unwrap(),
        CaptureTargets {
            achievements: false,
            ..Default::default()
        },
    )
    .unwrap();
    decoder
        .receive_command(&character_packet(13, 77, false))
        .unwrap();
    assert!(!decoder.state().has_characters);
    decoder.receive_command(&characters(13, 77)).unwrap();
    assert!(decoder.state().has_characters);
}

#[test]
fn trailblazer_path_uses_base_progression() {
    let mut decoder = HsrPacketDecoder::new(
        load_embedded_gilore_reference().unwrap(),
        CaptureTargets {
            achievements: false,
            ..Default::default()
        },
    )
    .unwrap();
    let base = Avatar {
        base_avatar_id: 8001,
        cur_multi_path_avatar_type: 8005,
        level: 70,
        promotion: 5,
        first_met_time_stamp: 1_700_000_000,
        ..Default::default()
    };
    let path = AvatarPathData {
        avatar_id: 8005,
        rank: 4,
        avatar_path_skill_tree: vec![AvatarPathSkillTree {
            point_id: 1,
            level: 6,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut bytes = group(31, &[base.write_to_bytes().unwrap()]);
    bytes.extend(group(47, &[path.write_to_bytes().unwrap()]));
    let mut out = CodedOutputStream::vec(&mut bytes);
    out.write_bool(800, true).unwrap();
    out.flush().unwrap();
    drop(out);
    decoder.receive_command(&bytes).unwrap();
    decoder.receive_command(&bag(6, 7)).unwrap();
    let state = decoder.state();
    let character = &state.inventory.as_ref().unwrap().characters[0];
    assert_eq!(
        (
            character.character_id,
            character.level,
            character.ascension,
            character.eidolon
        ),
        (8005, 70, 5, 4)
    );
    let common = hsr_scanner::scanner_export::build_scanner_export(
        state.inventory.as_ref().unwrap(),
        &load_embedded_gilore_reference().unwrap(),
        &state.export_details,
        Some(&[4010101]),
    )
    .unwrap();
    assert_eq!(common["metadata"]["trailblazer"], "Caelus");
    assert_eq!(common["metadata"]["current_trailblazer_path"], "Harmony");
    assert_eq!(common["achievements"], serde_json::json!([4010101]));
}

fn bag(tag: u32, relic_tag: u32) -> Vec<u8> {
    bag_for(tag, relic_tag, 1001)
}

fn bag_for(tag: u32, relic_tag: u32, character_id: u32) -> Vec<u8> {
    let cone = Equipment {
        tid: 23005,
        unique_id: 77,
        level: 80,
        promotion: 6,
        rank: 3,
        equip_avatar_id: character_id,
        is_protected: true,
        ..Default::default()
    };
    let relic = Relic {
        tid: 61011,
        unique_id: 88,
        level: 15,
        main_affix_id: 1,
        equip_avatar_id: character_id,
        is_protected: true,
        sub_affix_list: vec![RelicAffix {
            affix_id: 7,
            cnt: 2,
            step: 2,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut bytes = group(tag, &[cone.write_to_bytes().unwrap()]);
    bytes.extend(group(relic_tag, &[relic.write_to_bytes().unwrap()]));
    bytes
}

#[test]
fn continuous_capture_merges_single_gear_updates_after_complete_inventory() {
    let mut decoder = HsrPacketDecoder::new(
        load_embedded_gilore_reference().unwrap(),
        CaptureTargets {
            achievements: false,
            ..Default::default()
        },
    )
    .unwrap();
    decoder.enable_continuous();
    let mut cone = Equipment {
        tid: 23005,
        unique_id: 77,
        level: 70,
        promotion: 5,
        rank: 2,
        equip_avatar_id: 1001,
        is_protected: false,
        ..Default::default()
    };
    // An isolated update cannot establish a complete inventory.
    decoder
        .receive_command(&group(53, &[cone.write_to_bytes().unwrap()]))
        .unwrap();
    assert!(!decoder.state().complete);
    decoder.receive_command(&characters(13, 77)).unwrap();
    decoder.receive_command(&bag(54, 65)).unwrap();
    assert!(decoder.state().complete);
    let before = decoder.state().inventory.clone().unwrap();
    decoder
        .receive_command(&group(53, &[cone.write_to_bytes().unwrap()]))
        .unwrap();
    let changed = decoder.state().inventory.clone().unwrap();
    assert_eq!(changed.light_cones.len(), 1);
    assert_eq!(changed.light_cones[0].level, 70);
    assert_eq!(changed.light_cones[0].lock, Some(false));
    assert_eq!(changed.gear, before.gear);
    cone.unique_id = 79;
    decoder
        .receive_command(&group(995, &[cone.write_to_bytes().unwrap()]))
        .unwrap();
    assert_eq!(
        decoder
            .state()
            .inventory
            .as_ref()
            .unwrap()
            .light_cones
            .len(),
        2
    );
    // Repeated packets must replace by instance ID, never duplicate gear.
    decoder
        .receive_command(&group(995, &[cone.write_to_bytes().unwrap()]))
        .unwrap();
    assert_eq!(
        decoder
            .state()
            .inventory
            .as_ref()
            .unwrap()
            .light_cones
            .len(),
        2
    );
}

#[test]
fn rotated_outer_fields_and_nested_wrappers_export_inventory() {
    let references = load_embedded_gilore_reference().unwrap();
    for (a, b) in [(1, 2), (777, 991), (31, 42)] {
        let mut decoder = HsrPacketDecoder::new(
            references.clone(),
            CaptureTargets {
                achievements: false,
                ..Default::default()
            },
        )
        .unwrap();
        decoder
            .receive_command(&group(63, &[characters(a, b)]))
            .unwrap();
        assert!(decoder.state().has_characters);
        assert!(!decoder.state().complete);
        decoder.receive_command(&group(55, &[bag(b, a)])).unwrap();
        let state = decoder.state();
        assert!(state.complete);
        assert_eq!(
            (
                state.character_count,
                state.light_cone_count,
                state.relic_count
            ),
            (1, 1, 1)
        );
        let export = build_export(
            ValidatedObservationSnapshot::from_packet_capture(state.inventory.clone().unwrap())
                .unwrap(),
            &references,
        )
        .unwrap();
        assert_eq!(export.characters[0].eidolon, 2);
        assert_eq!(export.light_cones[0].superimposition, 3);
        assert_eq!(export.relics[0].gear.level, 15);
        let json = serde_json::to_string(&export).unwrap();
        assert!(!json.contains("unique_id"));
        assert!(!json.contains("first_met"));
    }
}

#[test]
fn achievements_are_optional_but_cannot_complete_inventory_capture_alone() {
    let refs = load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), CaptureTargets::default()).unwrap();
    let records = refs
        .achievement_ids()
        .take(6)
        .map(|id| {
            let mut bytes = Vec::new();
            let mut out = CodedOutputStream::vec(&mut bytes);
            out.write_uint32(12, id).unwrap();
            out.write_uint32(1, 3).unwrap();
            out.write_uint32(3, 1_700_000_000).unwrap();
            // Current Quest records may have additional packed counters.
            out.write_bytes(10, &[1, 2, 3]).unwrap();
            out.flush().unwrap();
            drop(out);
            bytes
        })
        .collect::<Vec<_>>();
    decoder
        .receive_command(&group(67, &[group(10, &records)]))
        .unwrap();
    let mut only = HsrPacketDecoder::new(
        refs.clone(),
        CaptureTargets {
            characters: false,
            light_cones: false,
            relics: false,
            achievements: true,
        },
    )
    .unwrap();
    only.receive_command(&group(67, &[group(10, &records)]))
        .unwrap();
    assert!(only.state().complete);
    let inventory = only.state().inventory.clone().unwrap();
    assert!(
        inventory.characters.is_empty()
            && inventory.light_cones.is_empty()
            && inventory.gear.is_empty()
    );
    hsr_scanner::pipeline::build_export_with_achievements(
        ValidatedObservationSnapshot::from_packet_capture(inventory).unwrap(),
        hsr_scanner::pipeline::build_achievement_snapshot(
            only.state()
                .quests
                .iter()
                .map(|quest| quest.id)
                .collect::<Vec<_>>(),
            "test",
            &refs,
        )
        .unwrap(),
        &refs,
    )
    .unwrap();
    assert!(decoder.state().has_achievements);
    assert_eq!(decoder.state().achievement_count, 6);
    assert!(!decoder.state().complete);
    decoder.receive_command(&bag(6, 7)).unwrap();
    assert!(!decoder.state().complete);
    decoder.receive_command(&characters(22, 23)).unwrap();
    assert!(decoder.state().complete);
}

#[test]
fn malformed_and_ambiguous_records_do_not_claim_complete_data() {
    let refs = load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(
        refs,
        CaptureTargets {
            achievements: false,
            ..Default::default()
        },
    )
    .unwrap();
    for bytes in [&[0xff; 8][..], &[0x12, 0xff], &[]] {
        decoder.receive_command(bytes).unwrap();
        assert!(!decoder.state().complete);
    }
    // Two independently valid base collections are ambiguous.
    let mut ambiguous = characters(6, 7);
    let base = Avatar {
        base_avatar_id: 1001,
        level: 80,
        promotion: 6,
        first_met_time_stamp: 1_700_000_000,
        ..Default::default()
    };
    ambiguous.extend(group(8, &[base.write_to_bytes().unwrap()]));
    decoder.receive_command(&ambiguous).unwrap();
    assert!(!decoder.state().has_characters);
}

#[test]
fn each_inventory_selection_completes_without_other_categories_and_exports_only_selected_data() {
    use hsr_scanner::model::CoverageLevel;
    let refs = load_embedded_gilore_reference().unwrap();
    for index in 0..3 {
        let targets = CaptureTargets {
            characters: index == 0,
            light_cones: index == 1,
            relics: index == 2,
            achievements: false,
        };
        let mut decoder = HsrPacketDecoder::new(refs.clone(), targets).unwrap();
        // Do not supply an unrequested roster or achievement response.
        decoder
            .receive_command(&if index == 0 {
                characters(22, 23)
            } else {
                bag(6, 7)
            })
            .unwrap();
        assert!(
            decoder.state().complete,
            "selection {index} did not complete"
        );
        let snapshot = decoder.state().inventory.clone().unwrap();
        assert_eq!(snapshot.characters.len(), usize::from(targets.characters));
        assert_eq!(snapshot.light_cones.len(), usize::from(targets.light_cones));
        assert_eq!(snapshot.gear.len(), usize::from(targets.relics));
        let coverage = &snapshot.evidence.coverage;
        for (selected, actual) in [
            (targets.characters, coverage.characters),
            (targets.light_cones, coverage.light_cones),
            (targets.relics, coverage.relics),
        ] {
            assert_eq!(
                actual,
                if selected {
                    CoverageLevel::Complete
                } else {
                    CoverageLevel::Unknown
                }
            );
        }
        let export = build_export(
            ValidatedObservationSnapshot::from_packet_capture(snapshot).unwrap(),
            &refs,
        )
        .unwrap();
        let json = serde_json::to_value(export).unwrap();
        assert_eq!(
            json["characters"].as_array().unwrap().len(),
            usize::from(targets.characters)
        );
        assert_eq!(
            json["lightCones"].as_array().unwrap().len(),
            usize::from(targets.light_cones)
        );
    }
}

#[test]
fn no_selected_categories_is_rejected() {
    assert!(HsrPacketDecoder::new(
        load_embedded_gilore_reference().unwrap(),
        CaptureTargets {
            characters: false,
            light_cones: false,
            relics: false,
            achievements: false,
        }
    )
    .is_err());
}

#[test]
fn continuous_selected_gear_category_keeps_replacing_instances() {
    for light_cones in [false, true] {
        let mut decoder = HsrPacketDecoder::new(
            load_embedded_gilore_reference().unwrap(),
            CaptureTargets {
                characters: false,
                light_cones,
                relics: !light_cones,
                achievements: false,
            },
        )
        .unwrap();
        decoder.enable_continuous();
        decoder.receive_command(&bag(6, 7)).unwrap();
        assert!(decoder.state().complete);
        let update = if light_cones {
            Equipment {
                tid: 23005,
                unique_id: 77,
                level: 70,
                promotion: 5,
                rank: 1,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap()
        } else {
            Relic {
                tid: 61011,
                unique_id: 88,
                level: 4,
                main_affix_id: 1,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap()
        };
        decoder.receive_command(&group(99, &[update])).unwrap();
        let inventory = decoder.state().inventory.as_ref().unwrap();
        if light_cones {
            assert_eq!(inventory.light_cones.len(), 1);
            assert_eq!(inventory.light_cones[0].level, 70);
            assert!(inventory.gear.is_empty());
        } else {
            assert_eq!(inventory.gear.len(), 1);
            assert_eq!(inventory.gear[0].level, 4);
            assert!(inventory.light_cones.is_empty());
        }
    }
}

#[test]
fn common_export_preserves_selected_capture_records_and_character_progression() {
    let refs = load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(
        refs.clone(),
        CaptureTargets {
            achievements: false,
            ..Default::default()
        },
    )
    .unwrap();
    decoder.receive_command(&characters(22, 23)).unwrap();
    decoder.receive_command(&bag(6, 7)).unwrap();
    let state = decoder.state();
    let common = hsr_scanner::scanner_export::build_scanner_export(
        state.inventory.as_ref().unwrap(),
        &refs,
        &state.export_details,
        None,
    )
    .unwrap();
    assert!(common.get("achievements").is_none());
    assert_eq!(common["characters"][0]["skills"]["basic"], 6);
    assert_eq!(common["characters"][0]["ability_version"], 0);
    assert_eq!(common["light_cones"][0]["location"], "1001");
    assert_eq!(common["light_cones"][0]["id"], "23005");
    assert_eq!(common["relics"][0]["set_id"], "101");
    assert_eq!(common["relics"][0]["slot"], "Head");
    assert_eq!(common["relics"][0]["mainstat"], "HP");
    assert_eq!(common["relics"][0]["substats"][0]["key"], "SPD");
    assert!(
        common["relics"][0]["substats"][0]["value"]
            .as_f64()
            .unwrap()
            > 4.0
    );
    assert_ne!(common["relics"][0]["_uid"], "88");
}

#[test]
fn collaboration_roster_and_equipment_export_with_embedded_references() {
    let refs = load_embedded_gilore_reference().unwrap();
    for id in [1014, 1015, 1508, 1509] {
        for bag_first in [true, false] {
            let mut decoder = HsrPacketDecoder::new(
                refs.clone(),
                CaptureTargets {
                    achievements: false,
                    ..Default::default()
                },
            )
            .unwrap();
            let roster = character_packet_for(777, 991, true, id);
            let bag = bag_for(37, 41, id);
            let packets = if bag_first {
                [&bag, &roster]
            } else {
                [&roster, &bag]
            };
            for packet in packets {
                decoder.receive_command(packet).unwrap();
            }
            let state = decoder.state();
            assert!(state.complete, "collaboration character {id}");
            let snapshot = state.inventory.clone().unwrap();
            let common = hsr_scanner::scanner_export::build_scanner_export(
                &snapshot,
                &refs,
                &state.export_details,
                None,
            )
            .unwrap();
            assert_eq!(common["characters"][0]["id"], id.to_string());
            assert_eq!(common["light_cones"][0]["location"], id.to_string());
            assert_eq!(common["relics"][0]["location"], id.to_string());
            if id == 1508 && bag_first {
                if let Ok(path) = std::env::var("HSR_COLLABORATION_FIXTURE") {
                    std::fs::write(path, serde_json::to_vec_pretty(&common).unwrap()).unwrap();
                }
            }
            let export = build_export(
                ValidatedObservationSnapshot::from_packet_capture(snapshot).unwrap(),
                &refs,
            )
            .unwrap();
            assert_eq!(export.characters[0].game_id, id);
            assert_eq!(
                export.light_cones[0].location_key.as_ref(),
                Some(&export.characters[0].key)
            );
            assert_eq!(
                export.relics[0].gear.location_key.as_ref(),
                Some(&export.characters[0].key)
            );
        }
    }
}

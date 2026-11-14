//! Synthetic login/transport regressions. Never commit local raw capture data.
use super::*;
use crate::network::tests::{command_frame, session_key};
use protobuf::{CodedOutputStream, Message};

const VERSION: u32 = 2_920_436_228;
const SEED_A: u64 = 0x1234567890;
const SEED_B: u64 = 0x2345678901;

fn dispatch_key() -> Vec<u8> {
    let mut key = vec![0; 4096];
    key[..4].copy_from_slice(&VERSION.to_be_bytes());
    key
}

fn configure(decoder: &mut HsrPacketDecoder) {
    decoder.dispatch_keys = HashMap::from([(VERSION, dispatch_key())]);
    decoder.sniffer = GameSniffer::new().set_initial_keys(decoder.dispatch_keys.clone());
}

fn fields(write: impl FnOnce(&mut CodedOutputStream<'_>)) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut out = CodedOutputStream::vec(&mut bytes);
    write(&mut out);
    out.flush().unwrap();
    drop(out);
    bytes
}

fn token(uid: u32, seed: u64) -> Vec<u8> {
    fields(|out| {
        out.write_uint64(12, seed).unwrap();
        out.write_uint32(15, uid).unwrap();
    })
}

fn login(nickname: &str) -> Vec<u8> {
    let basic = fields(|out| {
        out.write_string(1, nickname).unwrap();
        out.write_uint32(2, 70).unwrap();
    });
    fields(|out| out.write_bytes(4, &basic).unwrap())
}

fn roster() -> Vec<u8> {
    use super::proto::{
        Avatar::Avatar, AvatarPathData::AvatarPathData, AvatarPathSkillTree::AvatarPathSkillTree,
    };
    let base = Avatar {
        base_avatar_id: 1001,
        level: 80,
        promotion: 6,
        first_met_time_stamp: 1_700_000_000,
        ..Default::default()
    };
    let path = AvatarPathData {
        avatar_id: 1001,
        avatar_path_skill_tree: vec![AvatarPathSkillTree {
            point_id: 1,
            level: 6,
            ..Default::default()
        }],
        ..Default::default()
    };
    fields(|out| {
        out.write_bytes(13, &base.write_to_bytes().unwrap())
            .unwrap();
        out.write_bytes(77, &path.write_to_bytes().unwrap())
            .unwrap();
        out.write_bool(700, true).unwrap();
    })
}

fn bag(cone_id: u32, level: u32) -> Vec<u8> {
    use super::proto::{Equipment::Equipment, Relic::Relic};
    let cone = Equipment {
        tid: 23005,
        unique_id: cone_id,
        level,
        promotion: 6,
        rank: 1,
        ..Default::default()
    };
    let relic = Relic {
        tid: 61011,
        unique_id: 88,
        level: 15,
        main_affix_id: 1,
        ..Default::default()
    };
    fields(|out| {
        out.write_bytes(54, &cone.write_to_bytes().unwrap())
            .unwrap();
        out.write_bytes(65, &relic.write_to_bytes().unwrap())
            .unwrap();
    })
}

fn incoming(conv: u32, sequence: u32, id: u16, body: &[u8], key: &[u8]) -> Vec<u8> {
    command_frame(conv, sequence, id, body, key, PacketDirection::Received)
}

fn frames(conv: u32, uid: u32, seed: u64, nickname: &str) -> Vec<Vec<u8>> {
    let key = session_key(seed);
    vec![
        incoming(conv, 0, 16, &token(uid, seed), &dispatch_key()),
        incoming(conv, 1, 25, &login(nickname), &key),
        incoming(conv, 2, 65500, &roster(), &key),
        incoming(conv, 3, 65500, &bag(77, 80), &key),
    ]
}

fn connection_frame(code: u32, conv: u32) -> Vec<u8> {
    let mut packet = incoming(0, 0, 0, &[], &[0; 4096]);
    packet.truncate(42);
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(&conv.to_be_bytes());
    payload.extend_from_slice(&[0; 8]);
    payload.extend_from_slice(&code.to_be_bytes());
    packet.extend_from_slice(&payload);
    packet[16..18].copy_from_slice(&((20 + 8 + payload.len()) as u16).to_be_bytes());
    packet[38..40].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet
}

fn targets() -> CaptureTargets {
    CaptureTargets {
        achievements: false,
        ..Default::default()
    }
}

fn sync(relics: &[u32], cones: &[u32], packed: bool) -> Vec<u8> {
    fields(|out| {
        if packed {
            out.write_repeated_packed_uint32(15, relics).unwrap();
            out.write_repeated_packed_uint32(4, cones).unwrap();
        } else {
            for id in relics {
                out.write_uint32(15, *id).unwrap();
            }
            for id in cones {
                out.write_uint32(4, *id).unwrap();
            }
        }
    })
}

fn relic_sync(id: u32, level: u32) -> Vec<u8> {
    let relic = proto::Relic::Relic {
        tid: 61011,
        unique_id: id,
        level,
        main_affix_id: 1,
        ..Default::default()
    };
    fields(|out| {
        out.write_bytes(5, &relic.write_to_bytes().unwrap())
            .unwrap()
    })
}

#[test]
fn verified_sync_replaces_gear_and_keeps_cumulative_deletions_until_login_reset() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    decoder.enable_continuous();
    configure(&mut decoder);
    for frame in frames(11, 600_000_001, SEED_A, "First") {
        decoder.receive_packet(frame).unwrap();
    }
    let first = decoder.state().build_scanner_export(&refs).unwrap();
    let relic_id = first["relics"][0]["_uid"].clone();
    let cone_id = first["light_cones"][0]["_uid"].clone();
    let key = session_key(SEED_A);
    // The same instance is replaced, while a new instance appends.
    for (sequence, id, level) in [(4, 88, 0), (5, 88, 4), (6, 89, 8)] {
        decoder
            .receive_packet(incoming(11, sequence, 634, &relic_sync(id, level), &key))
            .unwrap();
    }
    let upgraded = decoder.state().build_scanner_export(&refs).unwrap();
    assert_eq!(upgraded["relics"].as_array().unwrap().len(), 2);
    assert_eq!(upgraded["relics"][0]["_uid"], relic_id);
    assert_eq!(upgraded["relics"][0]["level"], 4);
    let second_id = upgraded["relics"][1]["_uid"].clone();
    let mut upgrade_and_consume = relic_sync(89, 12);
    upgrade_and_consume.extend(sync(&[88], &[], true));
    decoder
        .receive_packet(incoming(11, 7, 634, &upgrade_and_consume, &key))
        .unwrap();
    let consumed = decoder.state().build_scanner_export(&refs).unwrap();
    assert_eq!(consumed["relics"].as_array().unwrap().len(), 1);
    assert_eq!(consumed["relics"][0]["_uid"], second_id);
    assert_eq!(consumed["relics"][0]["level"], 12);
    // A client can miss the first deletion and still receive both later.
    decoder
        .receive_packet(incoming(11, 8, 634, &sync(&[89], &[77], false), &key))
        .unwrap();
    let empty = decoder.state().build_scanner_export(&refs).unwrap();
    assert!(decoder.state().complete);
    assert_eq!(decoder.state().relic_count, 0);
    assert_eq!(decoder.state().light_cone_count, 0);
    assert_eq!(empty["relics"], serde_json::json!([]));
    assert_eq!(empty["light_cones"], serde_json::json!([]));
    assert_eq!(
        empty["generator"]["deletedInstanceIds"]["relics"],
        serde_json::json!([relic_id, second_id])
    );
    assert_eq!(
        empty["generator"]["deletedInstanceIds"]["lightCones"],
        serde_json::json!([cone_id])
    );
    assert_eq!(empty["generator"]["itemIdentity"], "account-instance-v1");
    let mut anonymous = decoder.state().export_details.clone();
    anonymous.uid = None;
    let anonymous_export = crate::scanner_export::build_scanner_export(
        decoder.state().inventory.as_ref().unwrap(),
        &refs,
        &anonymous,
        None,
    )
    .unwrap();
    assert!(anonymous_export["generator"]
        .get("deletedInstanceIds")
        .is_none());

    // Reappearing IDs remove their own tombstones, retaining other deletions.
    decoder
        .receive_packet(incoming(11, 9, 634, &relic_sync(88, 8), &key))
        .unwrap();
    let readded = decoder.state().build_scanner_export(&refs).unwrap();
    assert_eq!(readded["relics"][0]["_uid"], relic_id);
    assert_eq!(
        readded["generator"]["deletedInstanceIds"]["relics"],
        serde_json::json!([second_id])
    );
    // A fresh login resets tombstones along with its baseline.
    for frame in frames(22, 600_000_001, SEED_B, "First") {
        decoder.receive_packet(frame).unwrap();
    }
    let reset = decoder.state().build_scanner_export(&refs).unwrap();
    assert!(reset["generator"].get("deletedInstanceIds").is_none());
    assert_eq!(reset["relics"][0]["_uid"], relic_id);
}

#[test]
fn destructive_sync_requires_verified_revision_direction_command_and_known_instance() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    decoder.enable_continuous();
    configure(&mut decoder);
    let key = session_key(SEED_A);
    let mut login = frames(11, 600_000_001, SEED_A, "First").into_iter();
    decoder.receive_packet(login.next().unwrap()).unwrap();
    // A delta before the full bag does not establish data or a tombstone.
    decoder
        .receive_packet(incoming(11, 1, 634, &sync(&[88], &[77], true), &key))
        .unwrap();
    assert!(!decoder.state().has_items);
    decoder
        .receive_packet(incoming(11, 2, 65500, &roster(), &key))
        .unwrap();
    decoder
        .receive_packet(incoming(11, 3, 65500, &bag(77, 80), &key))
        .unwrap();
    let original = decoder.state().build_scanner_export(&refs).unwrap();
    decoder
        .receive_packet(incoming(11, 4, 65500, &sync(&[88], &[77], true), &key))
        .unwrap();
    decoder.receive_command(&sync(&[88], &[77], true)).unwrap();
    decoder
        .receive_packet(command_frame(
            11,
            0,
            634,
            &sync(&[88], &[77], true),
            &key,
            PacketDirection::Sent,
        ))
        .unwrap();
    // Unknown IDs and IDs from the other family never produce tombstones.
    decoder
        .receive_packet(incoming(
            11,
            5,
            634,
            &sync(&[77, 123456], &[88, 654321], false),
            &key,
        ))
        .unwrap();
    assert_eq!(
        decoder.state().build_scanner_export(&refs).unwrap(),
        original
    );
    assert!(
        inventory_sync::deletions(Some(VERSION + 1), 634, &sync(&[88], &[77], true))
            .unwrap()
            .is_none()
    );
    assert!(
        inventory_sync::deletions(Some(VERSION), 674, &sync(&[88], &[77], true))
            .unwrap()
            .is_none()
    );
}

#[test]
fn malformed_verified_deletion_does_not_partially_mutate_inventory() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    decoder.enable_continuous();
    configure(&mut decoder);
    for frame in frames(11, 600_000_001, SEED_A, "First") {
        decoder.receive_packet(frame).unwrap();
    }
    let original = decoder.state().build_scanner_export(&refs).unwrap();
    let mut malformed = sync(&[88], &[], false);
    malformed.extend(fields(|out| out.write_bytes(4, &[0x80]).unwrap()));
    assert!(decoder
        .receive_packet(incoming(11, 4, 634, &malformed, &session_key(SEED_A)))
        .is_err());
    assert_eq!(
        decoder.state().build_scanner_export(&refs).unwrap(),
        original
    );
    for body in [
        fields(|out| out.write_uint64(15, u64::from(u32::MAX) + 1).unwrap()),
        fields(|out| out.write_uint32(4, 0).unwrap()),
        fields(|out| out.write_fixed32(15, 88).unwrap()),
        fields(|out| out.write_uint32(99, 88).unwrap()),
    ] {
        assert!(inventory_sync::deletions(Some(VERSION), 634, &body).is_err());
    }
}

#[test]
fn normal_and_continuous_exports_share_verified_identity_and_opaque_item_ids() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut normal = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    configure(&mut normal);
    for frame in frames(11, 600_000_001, SEED_A, "Synthetic Captain") {
        normal.receive_packet(frame).unwrap();
    }
    let normal_export = normal.state().build_scanner_export(&refs).unwrap();
    normal.receive_packet(connection_frame(404, 11)).unwrap();
    assert_eq!(
        normal.state().build_scanner_export(&refs).unwrap(),
        normal_export
    );

    assert_eq!(normal_export["metadata"]["uid"], 600_000_001);
    assert_eq!(normal_export["metadata"]["nickname"], "Synthetic Captain");
    assert_eq!(
        normal_export["generator"]["itemIdentity"],
        "account-instance-v1"
    );
    let id = normal_export["light_cones"][0]["_uid"].as_str().unwrap();
    assert_eq!(id.len(), 64);
    assert!(id
        .bytes()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    assert_ne!(id, normal_export["relics"][0]["_uid"].as_str().unwrap());

    let published = Arc::new(Mutex::new(Vec::new()));
    let store = published.clone();
    let export_refs = refs.clone();
    let mut monitor = HsrCaptureMonitor::new(
        Arc::new(Mutex::new(HsrCaptureState::default())),
        refs,
        targets(),
    )
    .unwrap()
    .with_continuous(move |state| {
        if state.complete {
            store
                .lock()
                .unwrap()
                .push(state.build_scanner_export(&export_refs).unwrap());
        }
    });
    configure(&mut monitor.decoder);
    monitor.started = Some(tokio::time::Instant::now());
    for frame in frames(11, 600_000_001, SEED_A, "Synthetic Captain") {
        monitor.handle_packet(frame);
    }
    assert_eq!(published.lock().unwrap().last(), Some(&normal_export));
}

#[test]
fn new_account_retires_old_conversation_and_requires_fresh_baseline() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    decoder.enable_continuous();
    configure(&mut decoder);
    for frame in frames(11, 600_000_001, SEED_A, "First") {
        decoder.receive_packet(frame).unwrap();
    }
    let first = decoder.state().build_scanner_export(&refs).unwrap();
    let generation = decoder.state().session_generation;
    let mut second = frames(22, 600_000_002, SEED_B, "Second").into_iter();
    decoder.receive_packet(second.next().unwrap()).unwrap();
    assert!(!decoder.state().complete);
    assert_eq!(decoder.state().export_details.uid, Some(600_000_002));
    assert!(decoder.state().inventory.is_none());
    assert!(decoder.state().session_generation > generation);
    assert!(decoder.state().build_scanner_export(&refs).is_err());
    decoder
        .receive_packet(incoming(11, 4, 65500, &bag(99, 70), &session_key(SEED_A)))
        .unwrap();
    assert!(!decoder.state().has_items);
    for frame in second {
        decoder.receive_packet(frame).unwrap();
    }
    let second = decoder.state().build_scanner_export(&refs).unwrap();
    assert_eq!(second["metadata"]["uid"], 600_000_002);
    assert_eq!(second["metadata"]["nickname"], "Second");
    assert_eq!(second["light_cones"].as_array().unwrap().len(), 1);
    assert_ne!(
        first["light_cones"][0]["_uid"],
        second["light_cones"][0]["_uid"]
    );
    decoder.receive_packet(connection_frame(404, 11)).unwrap();
    decoder.receive_packet(connection_frame(255, 0)).unwrap();
    decoder.receive_packet(connection_frame(325, 11)).unwrap();
    assert_eq!(decoder.state().build_scanner_export(&refs).unwrap(), second);
    // Duplicate current establishment must preserve its learned session key.
    decoder.receive_packet(connection_frame(325, 22)).unwrap();
    assert_eq!(decoder.state().build_scanner_export(&refs).unwrap(), second);

    decoder
        .receive_packet(incoming(
            11,
            5,
            25,
            &login("Delayed First"),
            &session_key(SEED_A),
        ))
        .unwrap();
    assert_eq!(decoder.state().build_scanner_export(&refs).unwrap(), second);

    for frame in frames(33, 600_000_002, SEED_A, "Second Renamed") {
        decoder.receive_packet(frame).unwrap();
    }
    let reconnected = decoder.state().build_scanner_export(&refs).unwrap();
    assert_eq!(
        reconnected["light_cones"][0]["_uid"],
        second["light_cones"][0]["_uid"]
    );
    assert_eq!(reconnected["metadata"]["nickname"], "Second Renamed");
    decoder
        .receive_packet(incoming(33, 4, 65500, &bag(77, 70), &session_key(SEED_A)))
        .unwrap();
    let upgraded = decoder.state().build_scanner_export(&refs).unwrap();
    assert_eq!(
        upgraded["light_cones"][0]["_uid"],
        reconnected["light_cones"][0]["_uid"]
    );
    assert_eq!(upgraded["light_cones"][0]["level"], 70);
}

#[test]
fn outgoing_or_unreviewed_identity_is_never_attached_to_an_inventory() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    configure(&mut decoder);
    decoder
        .receive_packet(command_frame(
            11,
            0,
            16,
            &token(600_000_001, SEED_A),
            &dispatch_key(),
            PacketDirection::Sent,
        ))
        .unwrap();
    assert!(decoder.account.is_none());
    // Decrypted body replay remains diagnostic data; it cannot prove identity
    // or produce a successful public capture export.
    decoder.receive_command(&roster()).unwrap();
    decoder.receive_command(&bag(77, 80)).unwrap();
    assert_eq!(
        decoder
            .state()
            .build_scanner_export(&refs)
            .unwrap_err()
            .code(),
        "HSR-CAPTURE-UID"
    );
    let anonymous = crate::scanner_export::build_scanner_export(
        decoder.state().inventory.as_ref().unwrap(),
        &refs,
        &decoder.state().export_details,
        None,
    )
    .unwrap();
    assert!(anonymous["metadata"]["uid"].is_null());
    assert!(anonymous["metadata"].get("nickname").is_none());
    assert!(anonymous["generator"].get("itemIdentity").is_none());
    assert_eq!(anonymous["light_cones"][0]["_uid"], "1");
    let mut details = decoder.state().export_details.clone();
    details.uid = Some(0);
    let zero_uid = crate::scanner_export::build_scanner_export(
        decoder.state().inventory.as_ref().unwrap(),
        &refs,
        &details,
        None,
    )
    .unwrap();
    assert!(zero_uid["generator"].get("itemIdentity").is_none());
    details.uid = Some(600_000_001);
    details.instance_ids.as_mut().unwrap().relics.clear();
    let misaligned = crate::scanner_export::build_scanner_export(
        decoder.state().inventory.as_ref().unwrap(),
        &refs,
        &details,
        None,
    )
    .unwrap();
    assert!(misaligned["generator"].get("itemIdentity").is_none());
}

#[test]
fn identified_disconnect_then_unreviewed_login_cannot_export_or_reuse_old_uid() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    decoder.enable_continuous();
    configure(&mut decoder);
    decoder.dispatch_keys.insert(0, vec![0; 4096]);
    decoder.sniffer = GameSniffer::new().set_initial_keys(decoder.dispatch_keys.clone());
    for frame in frames(11, 600_000_001, SEED_A, "First") {
        decoder.receive_packet(frame).unwrap();
    }
    decoder.receive_packet(connection_frame(404, 11)).unwrap();
    assert!(!decoder.state().complete);
    assert_eq!(decoder.state().export_details.uid, None);
    decoder.receive_packet(connection_frame(325, 22)).unwrap();
    decoder
        .receive_packet(incoming(22, 0, 16, &token(600_000_002, SEED_B), &[0; 4096]))
        .unwrap();
    let key = session_key(SEED_B);
    decoder
        .receive_packet(incoming(22, 1, 25, &login("Unreviewed"), &key))
        .unwrap();
    decoder
        .receive_packet(incoming(22, 2, 65500, &roster(), &key))
        .unwrap();
    let error = decoder
        .receive_packet(incoming(22, 3, 65500, &bag(77, 70), &key))
        .unwrap_err();
    assert_eq!(error.code(), "HSR-CAPTURE-UID");
    assert!(!decoder.state().complete);
    assert_eq!(decoder.state().export_details.uid, None);
    assert_eq!(decoder.state().export_details.nickname, None);
    assert!(decoder.state().build_scanner_export(&refs).is_err());
    decoder
        .receive_packet(incoming(11, 4, 65500, &bag(99, 80), &session_key(SEED_A)))
        .unwrap();
    assert_eq!(decoder.state().export_details.uid, None);
    assert!(!decoder.state().complete);
}

#[test]
fn unreviewed_login_without_a_handshake_invalidates_the_previous_account() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let mut decoder = HsrPacketDecoder::new(refs.clone(), targets()).unwrap();
    decoder.enable_continuous();
    configure(&mut decoder);
    decoder.dispatch_keys.insert(0, vec![0; 4096]);
    decoder.sniffer = GameSniffer::new().set_initial_keys(decoder.dispatch_keys.clone());
    for frame in frames(11, 600_000_001, SEED_A, "First") {
        decoder.receive_packet(frame).unwrap();
    }
    assert!(decoder.state().complete);
    let generation = decoder.state().session_generation;
    // pktmon can miss establishment; a decoded new seed still proves that the
    // previous inventory no longer belongs to the active login.
    decoder
        .receive_packet(incoming(22, 0, 16, &token(600_000_002, SEED_B), &[0; 4096]))
        .unwrap();
    assert!(decoder.state().session_generation > generation);
    assert_eq!(decoder.state().export_details.uid, None);
    assert!(decoder.state().inventory.is_none());
    assert!(!decoder.state().complete);
    assert!(decoder.state().build_scanner_export(&refs).is_err());
    decoder
        .receive_packet(incoming(11, 4, 65500, &bag(99, 80), &session_key(SEED_A)))
        .unwrap();
    assert!(!decoder.state().has_items);
    let key = session_key(SEED_B);
    decoder
        .receive_packet(incoming(22, 1, 65500, &roster(), &key))
        .unwrap();
    let error = decoder
        .receive_packet(incoming(22, 2, 65500, &bag(77, 70), &key))
        .unwrap_err();
    assert_eq!(error.code(), "HSR-CAPTURE-UID");
    assert_eq!(decoder.state().export_details.uid, None);
    assert!(!decoder.state().complete);
}

#[test]
fn normal_and_continuous_monitors_report_unverified_identity_instead_of_success() {
    for continuous in [false, true] {
        for reviewed_revision in [false, true] {
            let refs = crate::load_embedded_gilore_reference().unwrap();
            let shared = Arc::new(Mutex::new(HsrCaptureState::default()));
            let observed = Arc::new(Mutex::new(Vec::new()));
            let store = observed.clone();
            let mut monitor =
                HsrCaptureMonitor::new(shared.clone(), refs.clone(), targets()).unwrap();
            if continuous {
                monitor =
                    monitor.with_continuous(move |state| store.lock().unwrap().push(state.clone()));
            }
            configure(&mut monitor.decoder);
            let dispatch = if reviewed_revision {
                dispatch_key()
            } else {
                vec![0; 4096]
            };
            let version = if reviewed_revision { VERSION } else { 0 };
            monitor.decoder.dispatch_keys = HashMap::from([(version, dispatch.clone())]);
            monitor.decoder.sniffer =
                GameSniffer::new().set_initial_keys(monitor.decoder.dispatch_keys.clone());
            monitor.started = Some(tokio::time::Instant::now());
            let uid = if reviewed_revision { 0 } else { 600_000_001 };
            monitor.handle_packet(incoming(11, 0, 16, &token(uid, SEED_A), &dispatch));
            let key = session_key(SEED_A);
            monitor.handle_packet(incoming(11, 1, 65500, &roster(), &key));
            monitor.handle_packet(incoming(11, 2, 65500, &bag(77, 80), &key));
            let state = shared.lock().unwrap();
            assert!(!state.complete);
            assert!(!state.capturing);
            let error = state
                .error
                .as_ref()
                .expect("missing UID must be actionable");
            assert_eq!(error.code(), "HSR-CAPTURE-UID");
            assert!(error
                .hint()
                .select(crate::Language::En)
                .contains("log in after starting capture"));
            assert!(state.build_scanner_export(&refs).is_err());
            if continuous {
                let observed = observed.lock().unwrap();
                assert!(observed.iter().all(|snapshot| !snapshot.complete));
                assert!(observed.last().unwrap().error.is_some());
            }
        }
    }
}

#[test]
fn a_batched_unverified_login_and_inventory_clears_shared_identity_before_error() {
    let refs = crate::load_embedded_gilore_reference().unwrap();
    let shared = Arc::new(Mutex::new(HsrCaptureState::default()));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let store = observed.clone();
    let mut monitor = HsrCaptureMonitor::new(shared.clone(), refs.clone(), targets())
        .unwrap()
        .with_continuous(move |state| store.lock().unwrap().push(state.clone()));
    configure(&mut monitor.decoder);
    monitor.started = Some(tokio::time::Instant::now());
    for frame in frames(11, 600_000_001, SEED_A, "First") {
        monitor.handle_packet(frame);
    }
    let generation = shared.lock().unwrap().session_generation;
    assert_eq!(shared.lock().unwrap().export_details.uid, Some(600_000_001));
    let key = session_key(SEED_B);
    let frames = [
        incoming(22, 0, 16, &token(0, SEED_B), &dispatch_key()),
        incoming(22, 1, 65500, &roster(), &key),
        incoming(22, 2, 65500, &bag(77, 70), &key),
    ];
    let mut packet = frames[0][..42].to_vec();
    for frame in frames {
        packet.extend_from_slice(&frame[42..]);
    }
    let payload_len = packet.len() - 42;
    packet[16..18].copy_from_slice(&((20 + 8 + payload_len) as u16).to_be_bytes());
    packet[38..40].copy_from_slice(&((8 + payload_len) as u16).to_be_bytes());
    monitor.handle_packet(packet);
    let state = shared.lock().unwrap();
    assert!(state.session_generation > generation);
    assert_eq!(state.export_details.uid, None);
    assert_eq!(state.export_details.nickname, None);
    assert!(!state.complete);
    assert_eq!(state.error.as_ref().unwrap().code(), "HSR-CAPTURE-UID");
    assert!(state.build_scanner_export(&refs).is_err());
    let observed = observed.lock().unwrap();
    assert_eq!(observed.last().unwrap().export_details.uid, None);
    assert!(observed.last().unwrap().error.is_some());
}

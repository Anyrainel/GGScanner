#![cfg(feature = "capture")]
use hsr_scanner::packet_capture::protocol::decode_quest_command;
use protobuf::CodedOutputStream;

fn record(id: u32, status: u32, packed: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut out = CodedOutputStream::vec(&mut bytes);
    out.write_uint32(11, id).unwrap();
    if status != 0 {
        out.write_uint32(3, status).unwrap();
    }
    out.write_uint32(8, 100).unwrap();
    out.write_uint64(10, 1700000000).unwrap();
    if packed {
        out.write_bytes(7, &[1, 2, 3]).unwrap();
    } else {
        out.write_uint32(7, 1).unwrap();
        out.write_uint32(7, 2).unwrap();
    }
    out.flush().unwrap();
    drop(out);
    bytes
}
fn response(records: Vec<Vec<u8>>) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut out = CodedOutputStream::vec(&mut bytes);
    for record in records {
        out.write_bytes(9, &record).unwrap();
    }
    out.flush().unwrap();
    drop(out);
    bytes
}
#[test]
fn all_quests_survive_without_references_or_id_prefix_filters() {
    let bytes = response(
        (0..6)
            .map(|i| record(4000001 + i, i % 5, i % 2 == 0))
            .collect(),
    );
    let decoded = decode_quest_command(&bytes).unwrap().unwrap();
    assert_eq!(decoded.len(), 6);
    assert_eq!(decoded[0].status, 0);
    assert_eq!(decoded[0].counters, vec![1, 2, 3]);
    assert_eq!(decoded[1].counters, vec![1, 2]);
    assert_eq!(decoded[0].progress, 100);
    let serialized = serde_json::to_string(&decoded).unwrap();
    assert!(!serialized.contains("1700000000"));
    assert!(!serialized.contains("finish_time"));
}
#[test]
fn corrupt_or_duplicate_records_never_become_partial_complete_snapshots() {
    let mut records = (0..6).map(|i| record(100 + i, 1, true)).collect::<Vec<_>>();
    records[5] = vec![0x80];
    assert!(decode_quest_command(&response(records)).is_err());
    assert!(decode_quest_command(&response(vec![record(100, 1, true); 6])).is_err());
    assert!(decode_quest_command(&response(
        (0..6).map(|i| record(100 + i, 5, true)).collect()
    ))
    .is_err());
}
#[test]
fn incremental_updates_and_non_quest_messages_are_absent() {
    assert_eq!(decode_quest_command(&[0x80]).unwrap(), None);
    assert_eq!(
        decode_quest_command(&response(vec![record(100, 1, true)])).unwrap(),
        None
    );
    let mut bytes = response((0..6).map(|i| record(100 + i, 1, true)).collect());
    bytes.extend_from_slice(&[8, 1]); // Unexpected outer field, e.g. notification.
    assert_eq!(decode_quest_command(&bytes).unwrap(), None);
}

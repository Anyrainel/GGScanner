//! Conservative inventory delta recognition. Command IDs and outer field tags
//! rotate between versions, so match the store notification's complete shape.
//! Reviewed against teyvat.me-capture, revision
//! 05a3bb8b0f8c02432c307ae43ba68277e2141206, crates/protocol/src/lib.rs.
use auto_artifactarium::r#gen::protos::Unk;
use protobuf::{Message, UnknownValueRef};

const MAX_DELTA_ITEMS: usize = 4096;

/// StoreItemDelNotify has exactly a packed GUID list and store_type (1 or 2).
/// A GUID elsewhere can name an upgrade target or a newly created artifact;
/// intersecting arbitrary GUIDs with held inventory would delete those items.
pub(super) fn deleted_items(data: &[u8], uid: u32) -> Option<Vec<u64>> {
    let message = Unk::parse_from_bytes(data).ok()?;
    let mut packed = None;
    let mut store = None;
    for (tag, value) in message.unknown_fields().iter() {
        match value {
            UnknownValueRef::LengthDelimited(bytes) if packed.is_none() => {
                packed = Some((tag, bytes));
            },
            UnknownValueRef::Varint(value) if store.is_none() && matches!(value, 1 | 2) => {
                store = Some(tag);
            },
            _ => return None,
        }
    }
    let (tag, bytes) = packed?;
    if tag == store? || uid == 0 {
        return None;
    }
    let mut guids = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let (guid, consumed) = read_varint(&bytes[offset..])?;
        if guid >> 32 != u64::from(uid) || guids.len() == MAX_DELTA_ITEMS {
            return None;
        }
        guids.push(guid);
        offset += consumed;
    }
    (!guids.is_empty()).then_some(guids)
}

fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, byte) in bytes.iter().copied().take(10).enumerate() {
        if index == 9 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 800_123_456;

    fn guid(counter: u64) -> u64 {
        (u64::from(UID) << 32) | counter
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while value >= 0x80 {
            bytes.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        bytes.push(value as u8);
        bytes
    }

    fn packet(guids: &[u64], store: Option<u64>) -> Unk {
        let mut packet = Unk::new();
        packet
            .mut_unknown_fields()
            .add_length_delimited(4, guids.iter().flat_map(|guid| varint(*guid)).collect());
        if let Some(store) = store {
            packet.mut_unknown_fields().add_varint(15, store);
        }
        packet
    }

    #[test]
    fn delete_requires_one_packed_list_and_store_for_the_verified_account() {
        for store in [1, 2] {
            let bytes = packet(&[guid(17), guid(4096)], Some(store))
                .write_to_bytes()
                .unwrap();
            assert_eq!(deleted_items(&bytes, UID), Some(vec![guid(17), guid(4096)]));
            assert!(deleted_items(&bytes, UID + 1).is_none());
            assert!(deleted_items(&bytes, 0).is_none());
        }
        for guids in [vec![], vec![10_000_001], vec![guid(1), guid(2) + (1 << 32)]] {
            assert!(
                deleted_items(&packet(&guids, Some(1)).write_to_bytes().unwrap(), UID).is_none()
            );
        }
    }

    #[test]
    fn upgrade_lock_and_strongbox_replies_cannot_delete_kept_items() {
        // Strongbox output lists created GUIDs but has no store_type.
        assert!(deleted_items(
            &packet(&[guid(901), guid(902)], None)
                .write_to_bytes()
                .unwrap(),
            UID
        )
        .is_none());
        assert!(
            deleted_items(&packet(&[guid(1)], Some(7)).write_to_bytes().unwrap(), UID).is_none()
        );
        // Upgrade/promote and lock replies name a target as a scalar GUID.
        for fields in [
            vec![(1, guid(77)), (5, 5), (11, 4)],
            vec![(2, 80), (10, guid(77)), (14, 70)],
            vec![(7, 1), (13, guid(5))],
        ] {
            let mut reply = Unk::new();
            for (tag, value) in fields {
                reply.mut_unknown_fields().add_varint(tag, value);
            }
            assert!(deleted_items(&reply.write_to_bytes().unwrap(), UID).is_none());
        }
    }

    #[test]
    fn malformed_or_extra_fields_cannot_delete_items() {
        let mut extra = packet(&[guid(1)], Some(1));
        extra.mut_unknown_fields().add_varint(6, 1);
        assert!(deleted_items(&extra.write_to_bytes().unwrap(), UID).is_none());
        let mut fixed = packet(&[guid(1)], Some(1));
        fixed.mut_unknown_fields().add_fixed32(6, 1);
        assert!(deleted_items(&fixed.write_to_bytes().unwrap(), UID).is_none());
        for suffix in [vec![0x80], vec![0xff; 10]] {
            let mut bytes = varint(guid(1));
            bytes.extend(suffix);
            let mut malformed = Unk::new();
            malformed
                .mut_unknown_fields()
                .add_length_delimited(4, bytes);
            malformed.mut_unknown_fields().add_varint(15, 1);
            assert!(deleted_items(&malformed.write_to_bytes().unwrap(), UID).is_none());
        }
        let mut unpacked = Unk::new();
        unpacked.mut_unknown_fields().add_varint(4, guid(1));
        unpacked.mut_unknown_fields().add_varint(15, 1);
        assert!(deleted_items(&unpacked.write_to_bytes().unwrap(), UID).is_none());
    }
}

//! Destructive deltas require the reviewed command and dispatch revision.
//! Reliquary 938287c5 (4.6), command_id.rs / PlayerSyncScNotify.rs:
//! command 634, del_equipment_list=4, del_relic_list=15. Outer inventory
//! discovery remains heuristic; arbitrary integer lists never delete items.
use super::protocol::{parse_message, WireValue};
use crate::{scanner_export::CaptureInstanceIds, HsrError, HsrResult, LocalizedText};

pub(super) fn deletions(
    dispatch_key: Option<u32>,
    command: u16,
    bytes: &[u8],
) -> HsrResult<Option<CaptureInstanceIds>> {
    if dispatch_key != Some(2_920_436_228) || command != 634 {
        return Ok(None);
    }
    let fields = parse_message(bytes).ok_or_else(invalid)?;
    let mut deleted = CaptureInstanceIds::default();
    for field in fields {
        match (field.number, field.value) {
            (tag @ (4 | 15), value) => {
                let ids = if tag == 4 {
                    &mut deleted.light_cones
                } else {
                    &mut deleted.relics
                };
                match value {
                    WireValue::Varint(value) => push_id(ids, value)?,
                    WireValue::Bytes(bytes) => {
                        let mut input = protobuf::CodedInputStream::from_bytes(bytes);
                        while !input.eof().map_err(|_| invalid())? {
                            push_id(ids, input.read_uint64().map_err(|_| invalid())?)?;
                        }
                    },
                    _ => return Err(invalid()),
                }
            },
            // All other fields in this reviewed notification are messages.
            (
                1 | 2 | 5..=12 | 242 | 267 | 287 | 492 | 620 | 781 | 789 | 1747 | 1757,
                WireValue::Bytes(_),
            ) => {},
            _ => return Err(invalid()),
        }
    }
    deleted.light_cones.sort_unstable();
    deleted.light_cones.dedup();
    deleted.relics.sort_unstable();
    deleted.relics.dedup();
    Ok(Some(deleted))
}

fn push_id(ids: &mut Vec<u32>, value: u64) -> HsrResult<()> {
    let id = u32::try_from(value)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(invalid)?;
    ids.push(id);
    Ok(())
}

fn invalid() -> HsrError {
    HsrError::new(
        "HSR-CAPTURE-SYNC",
        LocalizedText::new(
            "星穹铁道库存变更无法识别，请更新 GGScanner 后重新登录抓包。",
            "Star Rail inventory changes could not be decoded. Update GGScanner and capture a new login.",
        ),
        "malformed or unsupported reviewed PlayerSyncScNotify layout",
    )
}

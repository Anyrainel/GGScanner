//! Decode the reviewed 4.7 GetQuestDataScRsp schema, without a public ID allowlist.
//! Source: IceDynamix/reliquary, src/network/command/proto/{Quest,GetQuestDataScRsp}.rs.
//! Status, progress and packed counters are public Quest state. Completion times
//! and raw packets are never retained in this export.
use crate::{model::ObservedQuest, HsrError, HsrResult, LocalizedText};
use std::collections::BTreeSet;
const MAX_PROTOBUF_FIELD_NUMBER: u64 = (1 << 29) - 1;

pub fn decode_quest_command(bytes: &[u8]) -> HsrResult<Option<Vec<ObservedQuest>>> {
    let Some(fields) = parse_message(bytes) else {
        return Ok(None);
    };
    // Full response: retcode field 4 and repeated Quest field 9. Other response
    // types and incremental Quest updates must not become complete snapshots.
    if fields.iter().any(|field| !matches!(field.number, 4 | 9)) {
        return Ok(None);
    }
    let records = fields
        .iter()
        .filter(|field| field.number == 9)
        .collect::<Vec<_>>();
    if records.len() < 5 {
        return Ok(None);
    }
    if fields
        .iter()
        .any(|field| field.number == 4 && !matches!(field.value, WireValue::Varint(0)))
    {
        return Ok(None);
    }
    let mut quests = Vec::new();
    let mut ids = BTreeSet::new();
    for field in records {
        let WireValue::Bytes(bytes) = field.value else {
            return Ok(None);
        };
        let Some(quest) = decode_quest(bytes) else {
            return Err(invalid(
                "full Quest response contains an unsupported or malformed record",
            ));
        };
        if !ids.insert(quest.id) {
            return Err(invalid("duplicate Quest ID in full response"));
        }
        quests.push(quest);
    }
    quests.sort_unstable_by_key(|quest| quest.id);
    Ok(Some(quests))
}

fn invalid(detail: &str) -> HsrError {
    HsrError::new(
        "HSR-QUEST-PROTOCOL",
        LocalizedText::new(
            "任务协议数据无法识别，请更新抓包器后重试。",
            "Quest data could not be decoded. Update the capture tool and retry.",
        ),
        detail,
    )
}

fn decode_quest(bytes: &[u8]) -> Option<ObservedQuest> {
    let fields = parse_message(bytes)?;
    let mut scalar_tags = BTreeSet::new();
    let mut quest = ObservedQuest {
        id: 0,
        status: 0,
        progress: 0,
        counters: Vec::new(),
    };
    for field in fields {
        match (field.number, field.value) {
            (3 | 8 | 10 | 11, WireValue::Varint(value)) => {
                if !scalar_tags.insert(field.number) {
                    return None;
                }
                match field.number {
                    3 => quest.status = u32::try_from(value).ok()?,
                    11 => quest.id = u32::try_from(value).ok()?,
                    8 => quest.progress = u32::try_from(value).ok()?,
                    _ => (), // Completion timestamp intentionally discarded.
                }
            },
            (7, WireValue::Varint(value)) => quest.counters.push(u32::try_from(value).ok()?),
            (7, WireValue::Bytes(bytes)) => {
                let mut cursor = 0;
                while cursor < bytes.len() {
                    quest
                        .counters
                        .push(u32::try_from(read_varint(bytes, &mut cursor)?).ok()?);
                    if quest.counters.len() > 256 {
                        return None;
                    }
                }
            },
            _ => return None,
        }
    }
    (quest.id > 0 && quest.status <= 4 && quest.counters.len() <= 256).then_some(quest)
}

/// Token responses contain one 64-bit seed alongside public-size integers
/// and strings. Infer its tag only during dispatch decryption; never use a
/// command ID or rotate the session key on later gameplay packets.
pub(crate) fn infer_session_seed(bytes: &[u8]) -> Option<u64> {
    let fields = parse_message(bytes)?;
    if fields.len() > 8 {
        return None;
    }
    let mut seeds = fields.iter().filter_map(|field| match field.value {
        WireValue::Varint(value) if value > u32::MAX as u64 => Some(value),
        _ => None,
    });
    let seed = seeds.next()?;
    seeds.next().is_none().then_some(seed)
}

/// Walk well-formed protobuf wrappers, bounded by depth and total input size.
/// Each candidate stays intact so a rejected entry cannot silently disappear.
pub(crate) fn containers(bytes: &[u8]) -> Vec<&[u8]> {
    fn visit<'a>(bytes: &'a [u8], depth: usize, out: &mut Vec<&'a [u8]>) {
        if depth > 4 || out.len() >= 4096 {
            return;
        }
        let Some(fields) = parse_message(bytes) else {
            return;
        };
        out.push(bytes);
        for field in fields {
            if let WireValue::Bytes(child) = field.value {
                visit(child, depth + 1, out);
            }
        }
    }
    let mut result = Vec::new();
    if bytes.len() <= 16 * 1024 * 1024 {
        visit(bytes, 0, &mut result);
    }
    result
}

#[derive(Clone, Copy)]
pub(crate) struct WireField<'a> {
    pub number: u32,
    pub value: WireValue<'a>,
}

#[derive(Clone, Copy)]
pub(crate) enum WireValue<'a> {
    Varint(u64),
    Fixed64,
    Bytes(&'a [u8]),
    Fixed32,
}

pub(crate) fn parse_message(bytes: &[u8]) -> Option<Vec<WireField<'_>>> {
    let mut cursor = 0usize;
    let mut fields = Vec::new();
    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor)?;
        let field_number = key >> 3;
        let wire_type = key & 0x07;
        if field_number == 0 || field_number > MAX_PROTOBUF_FIELD_NUMBER {
            return None;
        }
        let number = u32::try_from(field_number).ok()?;

        let value = match wire_type {
            0 => WireValue::Varint(read_varint(bytes, &mut cursor)?),
            1 => {
                cursor = cursor.checked_add(8)?;
                if cursor > bytes.len() {
                    return None;
                }
                WireValue::Fixed64
            },
            2 => {
                let length = usize::try_from(read_varint(bytes, &mut cursor)?).ok()?;
                let end = cursor.checked_add(length)?;
                let value = bytes.get(cursor..end)?;
                cursor = end;
                WireValue::Bytes(value)
            },
            // Deprecated protobuf groups are not used by this protocol. A
            // group would make structural inference ambiguous, so reject it.
            3 | 4 => return None,
            5 => {
                cursor = cursor.checked_add(4)?;
                if cursor > bytes.len() {
                    return None;
                }
                WireValue::Fixed32
            },
            _ => return None,
        };
        fields.push(WireField { number, value });
    }
    Some(fields)
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

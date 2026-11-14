//! Account identity for the reviewed HSR 4.7 incoming login protocol only.
//! Never infer a UID from a plausible integer or a nickname from arbitrary text.
use crate::packet_capture::protocol::{parse_message, WireValue};

pub(super) const REVIEWED_DISPATCH_KEY: u32 = 2_766_937_619;
pub(super) const TOKEN_RESPONSE: u16 = 71;
pub(super) const LOGIN_RESPONSE: u16 = 19;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountIdentity {
    pub uid: u32,
    pub nickname: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct IdentityUpdate {
    pub account: AccountIdentity,
    pub new_login: bool,
}

// Pinned to Reliquary ba9319be (v25.0.0), token/login schemas and crypto key.
// A different dispatch-key revision must be reviewed before its identity is used.
pub(super) fn token_uid(version: Option<u32>, command_id: u16, bytes: &[u8]) -> Option<u32> {
    if version != Some(REVIEWED_DISPATCH_KEY) || command_id != TOKEN_RESPONSE {
        return None;
    }
    let fields = parse_message(bytes)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut uid = None;
    let mut seed = false;
    for field in fields {
        if !seen.insert(field.number) {
            return None;
        }
        match (field.number, field.value) {
            (11, WireValue::Varint(0)) => {},
            (5 | 7 | 8, WireValue::Bytes(_)) => {},
            (9, WireValue::Varint(value)) if value > u32::MAX as u64 => seed = true,
            (10, WireValue::Varint(value)) => {
                uid = u32::try_from(value)
                    .ok()
                    .filter(|value| *value >= 100_000_000);
            },
            _ => return None,
        }
    }
    seed.then_some(uid).flatten()
}

pub(super) fn login_nickname(
    version: Option<u32>,
    command_id: u16,
    bytes: &[u8],
) -> Option<String> {
    if version != Some(REVIEWED_DISPATCH_KEY) || command_id != LOGIN_RESPONSE {
        return None;
    }
    let fields = parse_message(bytes)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut basic = None;
    for field in fields {
        if !seen.insert(field.number) {
            return None;
        }
        match (field.number, field.value) {
            (4, WireValue::Varint(0)) => {},
            (10, WireValue::Bytes(value)) => basic = Some(value),
            (1 | 11, WireValue::Bytes(_)) => {},
            (7 | 9, WireValue::Varint(0 | 1)) => {},
            (3 | 12 | 13 | 14, WireValue::Varint(_)) => {},
            _ => return None,
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut nickname = None;
    let mut level = false;
    for field in parse_message(basic?)? {
        if !seen.insert(field.number) {
            return None;
        }
        match (field.number, field.value) {
            (1, WireValue::Bytes(value)) if value.len() <= 512 => {
                let value = std::str::from_utf8(value).ok()?;
                if value.trim().is_empty()
                    || value.chars().count() > 64
                    || value.chars().any(char::is_control)
                {
                    return None;
                }
                nickname = Some(value.to_owned());
            },
            (2, WireValue::Varint(value)) if (1..=70).contains(&value) => level = true,
            (3..=8, WireValue::Varint(_)) => {},
            _ => return None,
        }
    }
    level.then_some(nickname).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_wire_identity_requires_exact_version_command_and_success() {
        // Synthetic HSR 4.7 raw tags: seed=9, UID=10; no private capture bytes.
        let token = [
            0x48, 0x80, 0x80, 0x80, 0x80, 0x10, 0x50, 0x81, 0x8c, 0x8d, 0x9e, 0x02,
        ];
        assert_eq!(
            token_uid(Some(REVIEWED_DISPATCH_KEY), TOKEN_RESPONSE, &token),
            Some(600_000_001)
        );
        assert_eq!(token_uid(Some(0), TOKEN_RESPONSE, &token), None);
        assert_eq!(token_uid(Some(2_920_436_228), TOKEN_RESPONSE, &token), None);
        assert_eq!(token_uid(Some(REVIEWED_DISPATCH_KEY), 65500, &token), None);
        let previous_layout = [
            0x60, 0x80, 0x80, 0x80, 0x80, 0x10, 0x78, 0x81, 0x8c, 0x8d, 0x9e, 0x02,
        ];
        assert_eq!(
            token_uid(
                Some(REVIEWED_DISPATCH_KEY),
                TOKEN_RESPONSE,
                &previous_layout
            ),
            None
        );
        let mut failed = token.to_vec();
        failed.extend([0x58, 1]);
        assert_eq!(
            token_uid(Some(REVIEWED_DISPATCH_KEY), TOKEN_RESPONSE, &failed),
            None
        );
        let mut duplicate = token.to_vec();
        duplicate.extend([0x50, 1]);
        assert_eq!(
            token_uid(Some(REVIEWED_DISPATCH_KEY), TOKEN_RESPONSE, &duplicate),
            None
        );
        // Login field10 -> PlayerBasicInfo nickname field1, level field2.
        let login = [0x52, 5, 0x0a, 1, b'A', 0x10, 70];
        assert_eq!(
            login_nickname(Some(REVIEWED_DISPATCH_KEY), LOGIN_RESPONSE, &login).as_deref(),
            Some("A")
        );
        assert_eq!(login_nickname(None, LOGIN_RESPONSE, &login), None);
        let mut failed = login.to_vec();
        failed.extend([0x20, 1]);
        assert_eq!(
            login_nickname(Some(REVIEWED_DISPATCH_KEY), LOGIN_RESPONSE, &failed),
            None
        );
    }
}

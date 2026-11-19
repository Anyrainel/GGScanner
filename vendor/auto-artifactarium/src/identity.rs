//! Public game UID from a successful, RSA-verified Genshin login response.
//!
//! Reviewed 7.2 layout, also checked against a retained official-server login:
//! https://github.com/MeChen618/AstaPS/blob/a49f51aa6f2787ffbc25cee46801b70dee860d23/src/main/proto/GetPlayerTokenRsp.proto
//! Account UID (field 5) and game UID (field 9) are different identifiers.
//! Never infer identity from PacketHead.user_id or arbitrary large integers.
//!
//! Identity is accepted only for the reviewed layout. Other client builds (for
//! example a channel build that changes the key ID or the seed field) are
//! reported through [`LoginLayout`] instead: it describes the shape of the
//! response so a capture can be diagnosed, and never carries the UID, an
//! account identifier, a token, a seed or key material.
use base64::{Engine, prelude::BASE64_STANDARD};
use protobuf::{Message, UnknownValueRef};
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey};

use crate::r#gen::protos::Unk;

/// Field numbers of the reviewed 7.2 login layout.
const FIELD_GAME_UID: u32 = 9;
const FIELD_RETCODE: u32 = 14;
const FIELD_KEY_ID: u32 = 440;
const FIELD_SERVER_RAND_KEY: u32 = 477;

/// Key IDs whose bundled RSA private key can decrypt the session seed.
const SUPPORTED_KEY_IDS: [u32; 2] = [4, 5];

/// Smallest plausible public game UID (9 digits). Internal server accounts use
/// shorter identifiers and are never an export identity.
const MIN_GAME_UID: u32 = 100_000_000;

/// Sanitized shape of one incoming login response.
///
/// Contains field numbers and scalar protocol values only. It is safe to log,
/// show in diagnostics and copy out of a capture session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginLayout {
    pub retcode: Option<u32>,
    pub key_id: Option<u32>,
    /// A game UID scalar was present in the reviewed field.
    pub has_game_uid: bool,
    /// Length-delimited fields whose value decrypts with a bundled RSA key to
    /// the eight-byte session seed. An unrecognized client build normally
    /// keeps the seed in a field other than [`FIELD_SERVER_RAND_KEY`].
    pub seed_fields: Vec<u32>,
    /// The reviewed layout was accepted and produced a game UID.
    pub accepted: bool,
}

impl LoginLayout {
    /// Language-neutral single line for diagnostics. No identity data.
    pub fn summary(&self) -> String {
        let seeds = if self.seed_fields.is_empty() {
            "none".to_owned()
        } else {
            self.seed_fields
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "retcode={};keyId={};gameUid={};seedFields={};accepted={}",
            self.retcode
                .map_or_else(|| "none".to_owned(), |v| v.to_string()),
            self.key_id
                .map_or_else(|| "none".to_owned(), |v| v.to_string()),
            if self.has_game_uid {
                "present"
            } else {
                "absent"
            },
            seeds,
            self.accepted,
        )
    }
}

/// Scalar and seed facts read from a login response, before acceptance rules.
#[derive(Debug, Default)]
struct LoginFields {
    game_uid: Option<u64>,
    retcode: Option<u32>,
    key_id: Option<u32>,
    seed_fields: Vec<u32>,
}

impl LoginFields {
    fn layout(&self, accepted: bool) -> LoginLayout {
        LoginLayout {
            retcode: self.retcode,
            key_id: self.key_id,
            has_game_uid: self.game_uid.is_some(),
            seed_fields: self.seed_fields.clone(),
            accepted,
        }
    }
}

/// Read the reviewed scalar fields and locate the session seed by decrypting
/// every length-delimited value with the bundled RSA keys.
///
/// Returns `None` when the body is not a protobuf message or repeats/mistypes a
/// reviewed field, which is what the acceptance rules already reject.
fn read_login_fields(data: &[u8], rsa_keys: &[RsaPrivateKey]) -> Option<LoginFields> {
    let message = Unk::parse_from_bytes(data).ok()?;
    let mut fields = LoginFields::default();
    let mut seen_game_uid = false;
    let mut seen_retcode = false;
    let mut seen_key_id = false;
    let mut seen_server_rand_key = false;
    for (field, value) in message.unknown_fields().iter() {
        match (field, value) {
            (FIELD_GAME_UID, UnknownValueRef::Varint(value)) if !seen_game_uid => {
                seen_game_uid = true;
                fields.game_uid = Some(value);
            },
            (FIELD_RETCODE, UnknownValueRef::Varint(value)) if !seen_retcode => {
                seen_retcode = true;
                fields.retcode = u32::try_from(value).ok();
            },
            (FIELD_KEY_ID, UnknownValueRef::Varint(value)) if !seen_key_id => {
                seen_key_id = true;
                fields.key_id = u32::try_from(value).ok();
            },
            (FIELD_SERVER_RAND_KEY, UnknownValueRef::LengthDelimited(value))
                if !seen_server_rand_key =>
            {
                seen_server_rand_key = true;
                if decrypts_to_seed(value, rsa_keys) {
                    fields.seed_fields.push(field);
                }
            },
            // Reject duplicates and wrong wire types instead of choosing a value.
            (FIELD_GAME_UID | FIELD_RETCODE | FIELD_KEY_ID | FIELD_SERVER_RAND_KEY, _) => {
                return None;
            },
            (_, UnknownValueRef::LengthDelimited(value)) => {
                if decrypts_to_seed(value, rsa_keys) {
                    fields.seed_fields.push(field);
                }
            },
            _ => {},
        }
    }
    Some(fields)
}

/// True when `value` is base64 text holding an eight-byte seed encrypted with a
/// bundled RSA private key's public counterpart.
fn decrypts_to_seed(value: &[u8], rsa_keys: &[RsaPrivateKey]) -> bool {
    let Ok(encrypted) = BASE64_STANDARD.decode(value) else {
        return false;
    };
    rsa_keys.iter().any(|key| {
        key.decrypt(Pkcs1v15Encrypt, &encrypted)
            .is_ok_and(|seed| seed.len() == 8)
    })
}

/// Public game UID from a reviewed, RSA-verified login response.
///
/// Rejects every layout this module was not reviewed against, including
/// channel builds that move the seed to another field or use another key ID.
pub(crate) fn token_uid(data: &[u8], rsa_keys: &[RsaPrivateKey]) -> Option<u32> {
    accepted_uid(&read_login_fields(data, rsa_keys)?)
}

/// Acceptance rules for the reviewed login layout, applied once to the fields
/// read by [`read_login_fields`]. Both identity and diagnostics share them.
fn accepted_uid(fields: &LoginFields) -> Option<u32> {
    if fields.retcode.unwrap_or(0) != 0
        || !fields
            .key_id
            .is_some_and(|id| SUPPORTED_KEY_IDS.contains(&id))
    {
        return None;
    }
    // Verify this specific schema field. A seed-looking value elsewhere in the
    // message is evidence of another layout, not of this one.
    if !fields.seed_fields.contains(&FIELD_SERVER_RAND_KEY) {
        return None;
    }
    u32::try_from(fields.game_uid?)
        .ok()
        .filter(|uid| *uid >= MIN_GAME_UID)
}

/// Describe an incoming login response for capture diagnostics.
///
/// Returns `None` when the body is not a protobuf message. The summary never
/// leaves the game UID, account identifiers, tokens, seeds or keys.
pub(crate) fn probe(data: &[u8], rsa_keys: &[RsaPrivateKey]) -> Option<LoginLayout> {
    let fields = read_login_fields(data, rsa_keys)?;
    let accepted = accepted_uid(&fields).is_some();
    Some(fields.layout(accepted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::CodedOutputStream;
    use rsa::{RsaPublicKey, pkcs1::DecodeRsaPrivateKey, rand_core::OsRng};

    fn key() -> RsaPrivateKey {
        RsaPrivateKey::from_pkcs1_pem(include_str!("../keys/private_key_4.pem")).unwrap()
    }

    fn token(uid: u64, retcode: u32, seed_field: u32) -> Vec<u8> {
        let encrypted = RsaPublicKey::from(&key())
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &123456789u64.to_be_bytes())
            .unwrap();
        let mut body = Vec::new();
        let mut out = CodedOutputStream::vec(&mut body);
        out.write_string(5, "900000002").unwrap(); // HoYoverse account ID, not game UID.
        out.write_uint64(9, uid).unwrap();
        out.write_uint32(14, retcode).unwrap();
        out.write_uint32(440, 4).unwrap();
        out.write_bytes(seed_field, BASE64_STANDARD.encode(encrypted).as_bytes())
            .unwrap();
        out.flush().unwrap();
        drop(out);
        body
    }

    /// A channel build: the seed still decrypts with a bundled key, but it is
    /// not in field 477 and the key ID is not one this decoder was reviewed for.
    fn channel_token(uid: u64, retcode: u32, key_id: u32, seed_field: u32) -> Vec<u8> {
        let encrypted = RsaPublicKey::from(&key())
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &123456789u64.to_be_bytes())
            .unwrap();
        let mut body = Vec::new();
        let mut out = CodedOutputStream::vec(&mut body);
        out.write_uint64(9, uid).unwrap();
        out.write_uint32(14, retcode).unwrap();
        out.write_uint32(440, key_id).unwrap();
        out.write_bytes(seed_field, BASE64_STANDARD.encode(encrypted).as_bytes())
            .unwrap();
        out.flush().unwrap();
        drop(out);
        body
    }

    #[test]
    fn successful_reviewed_token_exports_only_its_game_uid() {
        let keys = [key()];
        for uid in [100_000_000, 800_000_001, u32::MAX] {
            assert_eq!(token_uid(&token(u64::from(uid), 0, 477), &keys), Some(uid));
        }
    }

    #[test]
    fn identity_requires_success_and_the_reviewed_rsa_field() {
        let keys = [key()];
        for body in [
            token(800_000_001, 1, 477),
            token(0, 0, 477),
            token(99_999_999, 0, 477),
            token(u64::from(u32::MAX) + 1, 0, 477),
            token(800_000_001, 0, 883),
            vec![0xff],
        ] {
            assert_eq!(token_uid(&body, &keys), None);
        }
        assert_eq!(token_uid(&token(800_000_001, 0, 477), &[]), None);
    }

    #[test]
    fn conflicting_or_mistyped_identity_is_not_silently_accepted() {
        let keys = [key()];
        for (field, wrong_type) in [(9, false), (14, false), (440, false), (477, true)] {
            let mut body = token(800_000_001, 0, 477);
            let mut out = CodedOutputStream::vec(&mut body);
            if wrong_type {
                out.write_uint32(field, 1).unwrap();
            } else {
                out.write_uint32(field, 800_000_002).unwrap();
            }
            out.flush().unwrap();
            drop(out);
            assert_eq!(token_uid(&body, &keys), None);
        }
    }

    #[test]
    fn reviewed_layout_reports_acceptance_without_identity_data() {
        let keys = [key()];
        let layout = probe(&token(800_000_001, 0, 477), &keys).unwrap();
        assert!(layout.accepted);
        assert_eq!(layout.retcode, Some(0));
        assert_eq!(layout.key_id, Some(4));
        assert!(layout.has_game_uid);
        assert_eq!(layout.seed_fields, vec![477]);
        let summary = layout.summary();
        assert!(!summary.contains("800000001"), "{summary}");
        assert!(!summary.contains("900000002"), "{summary}");
        assert_eq!(
            summary,
            "retcode=0;keyId=4;gameUid=present;seedFields=477;accepted=true"
        );
    }

    #[test]
    fn channel_layout_is_reported_but_never_accepted() {
        let keys = [key()];
        let body = channel_token(800_000_001, 0, 9, 477);
        assert_eq!(token_uid(&body, &keys), None);
        let layout = probe(&body, &keys).unwrap();
        assert!(!layout.accepted);
        assert_eq!(
            layout.summary(),
            "retcode=0;keyId=9;gameUid=present;seedFields=477;accepted=false"
        );

        // A moved seed field is visible even when the key ID matches.
        let moved = channel_token(800_000_001, 0, 4, 479);
        assert_eq!(token_uid(&moved, &keys), None);
        let layout = probe(&moved, &keys).unwrap();
        assert!(!layout.accepted);
        assert_eq!(layout.seed_fields, vec![479]);
        assert_eq!(
            layout.summary(),
            "retcode=0;keyId=4;gameUid=present;seedFields=479;accepted=false"
        );
    }

    #[test]
    fn probe_separates_non_token_payloads_from_unreviewed_layouts() {
        let keys = [key()];
        assert_eq!(probe(&[0xff], &keys), None);
        // No seed at all: still a reportable layout, just not a login response.
        let layout = probe(&[8, 42], &keys).unwrap();
        assert_eq!(
            layout.summary(),
            "retcode=none;keyId=none;gameUid=absent;seedFields=none;accepted=false"
        );
    }
}

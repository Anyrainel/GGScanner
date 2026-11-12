//! Public game UID from a successful, RSA-verified Genshin login response.
//!
//! Reviewed 7.2 layout, also checked against a retained official-server login:
//! https://github.com/MeChen618/AstaPS/blob/a49f51aa6f2787ffbc25cee46801b70dee860d23/src/main/proto/GetPlayerTokenRsp.proto
//! Account UID (field 5) and game UID (field 9) are different identifiers.
//! Never infer identity from PacketHead.user_id or arbitrary large integers.
use base64::{Engine, prelude::BASE64_STANDARD};
use protobuf::{Message, UnknownValueRef};
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey};

use crate::r#gen::protos::Unk;

pub(crate) fn token_uid(data: &[u8], rsa_keys: &[RsaPrivateKey]) -> Option<u32> {
    let message = Unk::parse_from_bytes(data).ok()?;
    let mut uid = None;
    let mut retcode = None;
    let mut key_id = None;
    let mut server_rand_key = None;
    for (field, value) in message.unknown_fields().iter() {
        match (field, value) {
            (9, UnknownValueRef::Varint(value)) if uid.is_none() => uid = Some(value),
            (14, UnknownValueRef::Varint(value)) if retcode.is_none() => retcode = Some(value),
            (440, UnknownValueRef::Varint(value)) if key_id.is_none() => key_id = Some(value),
            (477, UnknownValueRef::LengthDelimited(value)) if server_rand_key.is_none() => {
                server_rand_key = Some(value);
            },
            // Reject duplicates and wrong wire types instead of choosing a value.
            (9 | 14 | 440 | 477, _) => return None,
            _ => {},
        }
    }
    if retcode.unwrap_or(0) != 0 || !matches!(key_id, Some(4 | 5)) {
        return None;
    }
    let uid = u32::try_from(uid?).ok().filter(|uid| *uid >= 100_000_000)?;
    let encrypted = BASE64_STANDARD.decode(server_rand_key?).ok()?;
    // Verify this specific schema field, not a different RSA-shaped value in
    // the message. No authentication token, key or seed leaves this module.
    rsa_keys
        .iter()
        .any(|key| {
            key.decrypt(Pkcs1v15Encrypt, &encrypted)
                .is_ok_and(|seed| seed.len() == 8)
        })
        .then_some(uid)
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
}

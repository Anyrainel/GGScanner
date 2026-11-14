use hsr_scanner::{
    load_embedded_gilore_reference, EMBEDDED_GILORE_COMMIT, EMBEDDED_GILORE_MANIFEST_SHA256,
    EMBEDDED_GILORE_SOURCE_REVISION, EMBEDDED_REFERENCE_ACHIEVEMENT_COUNT,
    EMBEDDED_REFERENCE_BYTE_COUNT, EMBEDDED_REFERENCE_PROVIDER,
    EMBEDDED_REFERENCE_SENTINEL_ACHIEVEMENT_ID, EMBEDDED_REFERENCE_SHA256,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const EMBEDDED_BYTES: &[u8] = include_bytes!("../assets/gilore_reference_v1.json");

#[test]
fn embedded_reference_is_integrity_pinned_private_and_live_complete() {
    assert_eq!(
        EMBEDDED_GILORE_COMMIT,
        "0e63fc8fce3e00d1dadef9c695e121e89644aeca"
    );
    assert_eq!(
        EMBEDDED_GILORE_SOURCE_REVISION,
        "dfdfe3337bcfa14f000074d2dae7058118193ea6"
    );
    assert_eq!(
        EMBEDDED_GILORE_MANIFEST_SHA256,
        "910879a53d50bf3470f0a2427711a04d71dfca0175faf8c98c294d523ed8eb24"
    );
    assert_eq!(EMBEDDED_BYTES.len(), EMBEDDED_REFERENCE_BYTE_COUNT);
    assert_eq!(
        hex::encode(Sha256::digest(EMBEDDED_BYTES)),
        EMBEDDED_REFERENCE_SHA256
    );

    let value: Value = serde_json::from_slice(EMBEDDED_BYTES).expect("embedded JSON must parse");
    assert_eq!(
        serde_json::to_vec(&value).expect("embedded JSON must reserialize"),
        EMBEDDED_BYTES,
        "the checked-in document must remain deterministic minified JSON"
    );

    let cache = load_embedded_gilore_reference().expect("embedded reference must validate");
    cache
        .validate_live_complete_profile()
        .expect("embedded reference must be safe for account-facing flows");
    assert_eq!(cache.provider(), EMBEDDED_REFERENCE_PROVIDER);
    assert_eq!(cache.revision(), EMBEDDED_GILORE_SOURCE_REVISION);
    assert_eq!(
        cache.achievement_count(),
        EMBEDDED_REFERENCE_ACHIEVEMENT_COUNT
    );
    assert!(cache.has_achievement(EMBEDDED_REFERENCE_SENTINEL_ACHIEVEMENT_ID));
    assert!(cache.character(1511).is_some());
    for id in [22009, 23065] {
        assert!(cache.light_cone(id).is_some(), "current Light Cone {id}");
    }
    for id in [63295, 63305] {
        assert!(cache.gear(id).is_some(), "current Planar Ornament {id}");
    }
}

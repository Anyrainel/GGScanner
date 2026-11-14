use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    error::{hints, HsrError, HsrResult},
    model::ReferenceSnapshot,
    privacy::reject_sensitive_fields,
    reference::{GiloreBundleReferenceProvider, ReferenceCache, ReferenceProvider},
};

pub const EMBEDDED_REFERENCE_FORMAT_VERSION: u32 = 1;
pub const EMBEDDED_REFERENCE_PROVIDER: &str = "gilore.ggstarrail-reference";
// The stable historical GIlore namespace is now produced by HoyoData.
pub const EMBEDDED_GILORE_COMMIT: &str = "0e63fc8fce3e00d1dadef9c695e121e89644aeca";
pub const EMBEDDED_GILORE_SOURCE_REVISION: &str = "dfdfe3337bcfa14f000074d2dae7058118193ea6";
pub const EMBEDDED_GILORE_MANIFEST_SHA256: &str =
    "910879a53d50bf3470f0a2427711a04d71dfca0175faf8c98c294d523ed8eb24";
pub const EMBEDDED_REFERENCE_SHA256: &str =
    "874cfdbb9bc4e75c33f08082c0bc01ea386a110f631ce07c64e03fb55b9c427d";
pub const EMBEDDED_REFERENCE_BYTE_COUNT: usize = 349_106;
pub const EMBEDDED_REFERENCE_ACHIEVEMENT_COUNT: usize = 1_993;
pub const EMBEDDED_REFERENCE_SENTINEL_ACHIEVEMENT_ID: u32 = 4_010_101;

const EMBEDDED_REFERENCE_BYTES: &[u8] = include_bytes!("../assets/gilore_reference_v1.json");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EmbeddedReferenceDocument {
    format_version: u32,
    provenance: EmbeddedReferenceProvenance,
    snapshot: ReferenceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EmbeddedReferenceProvenance {
    provider: String,
    gilore_commit: String,
    source_revision: String,
    source_bundle_manifest_sha256: String,
}

/// Load the audited, account-independent GIlore reference snapshot compiled
/// into GGScannerOCR and GGScanner. The bytes, provenance, normalized schema,
/// privacy boundary, live-profile completeness, and achievement set are all
/// validated before the cache is returned.
pub fn load_embedded_gilore_reference() -> HsrResult<ReferenceCache> {
    if EMBEDDED_REFERENCE_BYTES.len() != EMBEDDED_REFERENCE_BYTE_COUNT {
        return Err(embedded_error(
            "HSR-REF-EMBEDDED-SIZE",
            format!(
                "embedded byte count mismatch; expected={EMBEDDED_REFERENCE_BYTE_COUNT}; actual={}",
                EMBEDDED_REFERENCE_BYTES.len()
            ),
        ));
    }
    require_sha256(
        "embedded reference",
        EMBEDDED_REFERENCE_BYTES,
        EMBEDDED_REFERENCE_SHA256,
        "HSR-REF-EMBEDDED-HASH",
    )?;

    let document = parse_embedded_document(EMBEDDED_REFERENCE_BYTES)?;
    validate_embedded_document(&document)?.with_packet_references(
        serde_json::from_str(include_str!("../assets/packet_affixes.json"))
            .map_err(|error| embedded_error("HSR-REF-FIXTURE-PACKET", error.to_string()))?,
    )
}

/// Deterministically normalize the exact audited GIlore bundle into the
/// compact embedded document. This is build tooling: released applications
/// only call [`load_embedded_gilore_reference`].
pub fn generate_embedded_gilore_reference(bundle_root: impl AsRef<Path>) -> HsrResult<Vec<u8>> {
    let bundle_root = bundle_root.as_ref();
    let manifest_path = bundle_root.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        HsrError::new(
            "HSR-REF-EMBEDDED-SOURCE-READ",
            hints::READ_FAILED,
            format!("path={}; cause={error}", manifest_path.display()),
        )
    })?;
    require_sha256(
        "source bundle manifest",
        &manifest_bytes,
        EMBEDDED_GILORE_MANIFEST_SHA256,
        "HSR-REF-EMBEDDED-SOURCE-HASH",
    )?;

    let snapshot = GiloreBundleReferenceProvider::new(bundle_root).load()?;
    let document = EmbeddedReferenceDocument {
        format_version: EMBEDDED_REFERENCE_FORMAT_VERSION,
        provenance: EmbeddedReferenceProvenance {
            provider: EMBEDDED_REFERENCE_PROVIDER.to_string(),
            gilore_commit: EMBEDDED_GILORE_COMMIT.to_string(),
            source_revision: EMBEDDED_GILORE_SOURCE_REVISION.to_string(),
            source_bundle_manifest_sha256: EMBEDDED_GILORE_MANIFEST_SHA256.to_string(),
        },
        snapshot,
    };
    validate_embedded_document(&document)?;
    serialize_embedded_document(&document)
}

fn parse_embedded_document(bytes: &[u8]) -> HsrResult<EmbeddedReferenceDocument> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        embedded_error(
            "HSR-REF-EMBEDDED-JSON",
            format!("embedded reference JSON is invalid; cause={error}"),
        )
    })?;
    reject_sensitive_fields(&value)?;
    serde_json::from_value(value).map_err(|error| {
        embedded_error(
            "HSR-REF-EMBEDDED-JSON",
            format!("embedded reference document is invalid; cause={error}"),
        )
    })
}

fn serialize_embedded_document(document: &EmbeddedReferenceDocument) -> HsrResult<Vec<u8>> {
    let value = serde_json::to_value(document).map_err(|error| {
        embedded_error(
            "HSR-REF-EMBEDDED-SERIALIZE",
            format!("could not normalize embedded reference document; cause={error}"),
        )
    })?;
    reject_sensitive_fields(&value)?;
    serde_json::to_vec(&value).map_err(|error| {
        embedded_error(
            "HSR-REF-EMBEDDED-SERIALIZE",
            format!("could not serialize embedded reference document; cause={error}"),
        )
    })
}

fn validate_embedded_document(document: &EmbeddedReferenceDocument) -> HsrResult<ReferenceCache> {
    let provenance = &document.provenance;
    if document.format_version != EMBEDDED_REFERENCE_FORMAT_VERSION
        || provenance.provider != EMBEDDED_REFERENCE_PROVIDER
        || provenance.gilore_commit != EMBEDDED_GILORE_COMMIT
        || provenance.source_revision != EMBEDDED_GILORE_SOURCE_REVISION
        || provenance.source_bundle_manifest_sha256 != EMBEDDED_GILORE_MANIFEST_SHA256
    {
        return Err(embedded_error(
            "HSR-REF-EMBEDDED-PROVENANCE",
            "embedded reference provenance does not match the audited GIlore boundary",
        ));
    }
    if document.snapshot.provider != provenance.provider
        || document.snapshot.revision != provenance.source_revision
    {
        return Err(embedded_error(
            "HSR-REF-EMBEDDED-COHERENCE",
            "normalized snapshot disagrees with embedded provenance",
        ));
    }

    let cache = ReferenceCache::from_snapshot(document.snapshot.clone())?;
    cache.validate_live_complete_profile()?;
    if cache.achievement_count() != EMBEDDED_REFERENCE_ACHIEVEMENT_COUNT
        || !cache.has_achievement(EMBEDDED_REFERENCE_SENTINEL_ACHIEVEMENT_ID)
    {
        return Err(embedded_error(
            "HSR-REF-EMBEDDED-ACHIEVEMENTS",
            format!(
                "achievement reference drift; expectedCount={EMBEDDED_REFERENCE_ACHIEVEMENT_COUNT}; actualCount={}; requiredId={EMBEDDED_REFERENCE_SENTINEL_ACHIEVEMENT_ID}",
                cache.achievement_count()
            ),
        ));
    }
    Ok(cache)
}

fn require_sha256(label: &str, bytes: &[u8], expected: &str, code: &'static str) -> HsrResult<()> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected {
        return Err(embedded_error(
            code,
            format!("{label} SHA-256 mismatch; expected={expected}; actual={actual}"),
        ));
    }
    Ok(())
}

fn embedded_error(code: &'static str, detail: impl Into<String>) -> HsrError {
    HsrError::new(code, hints::REFERENCE_INVALID, detail)
}

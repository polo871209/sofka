//! Decoding of Helm release storage `Secret`s.
//!
//! Helm (the package manager, not Flux's `HelmRelease` CRD) stores each
//! release revision as a `Secret` named `sh.helm.release.v1.<release>.v<rev>`
//! with `type: helm.sh/release.v1` and labels `owner=helm`, `name=<release>`,
//! `version=<revision>`, `status=<status>`. `data.release` is the release
//! JSON, base64-encoded and gzip-compressed by Helm itself, then base64
//! re-encoded by Kubernetes' own wire JSON for `Secret.data` bytes — so
//! decoding needs base64 twice, then gunzip, then JSON
//! (see helm/helm `pkg/storage/driver/util.go`).
//!
//! Only the Secret storage driver is supported (Helm's default; the
//! ConfigMap driver is out of scope).

use crate::json::Pointer as _;
use std::io::Read;

use base64::Engine;
use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use serde::Deserialize;
use serde_json::Value;

/// The base64 engine for release payloads.
///
/// Helm double-encodes the payload, so one decode runs base64 twice over the
/// whole body — the only place base64 is on a hot path here rather than reading
/// a short annotation. The SIMD engine detects the instruction set once, on
/// first use, and produces byte-identical output to the scalar one for the same
/// alphabet; it exists only for the architectures sofka ships binaries for, so
/// everything else keeps the scalar engine.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
static BASE64: std::sync::LazyLock<base64::engine::Simd> =
    std::sync::LazyLock::new(|| base64::engine::Simd::standard(Default::default()));
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
static BASE64: std::sync::LazyLock<base64::engine::general_purpose::GeneralPurpose> =
    std::sync::LazyLock::new(|| base64::engine::general_purpose::STANDARD);

/// A decoded Helm release revision. The k8s namespace is deliberately not
/// carried here — callers already have the storage Secret's own namespace,
/// which is the trustworthy source (not whatever the embedded JSON claims).
pub struct Release {
    pub name: String,
    pub revision: i64,
    pub status: String,
    pub chart_name: String,
    pub chart_version: String,
    pub app_version: String,
    pub description: String,
    /// `info.last_deployed` as a unix timestamp, when parseable.
    pub last_deployed_secs: Option<i64>,
    pub notes: String,
    /// User-supplied value overrides (`helm install/upgrade -f`/`--set`) —
    /// the "values" k9s' History-view Enter shows. The chart's own default
    /// `values.yaml` ("all values") isn't surfaced; see the plan's explicit
    /// scope note.
    pub config: Value,
    pub manifest: String,
}

/// The fields the release *list* renders. Deliberately excludes `manifest`
/// and `config`: those are the bulk of a release payload (a rendered manifest
/// runs to hundreds of KB) and no column reads them, so the list's parse walks
/// past them instead of allocating a `String` and a `Value` DOM per row.
pub struct Summary {
    pub status: String,
    pub chart_name: String,
    pub chart_version: String,
    pub app_version: String,
    pub description: String,
    /// `info.last_deployed` as a unix timestamp, when parseable.
    pub last_deployed_secs: Option<i64>,
}

#[derive(Deserialize, Default)]
struct RawRelease {
    #[serde(default)]
    name: String,
    #[serde(default)]
    info: RawInfo,
    #[serde(default)]
    chart: RawChart,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    manifest: String,
    #[serde(default)]
    version: i64,
}

#[derive(Deserialize, Default)]
struct RawInfo {
    #[serde(default)]
    last_deployed: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    notes: String,
}

#[derive(Deserialize, Default)]
struct RawChart {
    #[serde(default)]
    metadata: RawMetadata,
}

#[derive(Deserialize, Default)]
struct RawMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "appVersion")]
    app_version: String,
}

/// [`RawRelease`] minus the two heavy payloads, and minus `info.notes`.
#[derive(Deserialize, Default)]
struct RawSummary {
    #[serde(default)]
    info: RawInfoSummary,
    #[serde(default)]
    chart: RawChart,
}

#[derive(Deserialize, Default)]
struct RawInfoSummary {
    #[serde(default)]
    last_deployed: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
}

/// The serde step of [`decode`] in isolation, so a benchmark can price the
/// JSON parse against the base64 + gunzip in front of it. Deserializes into
/// the same typed struct the real path uses — a `Value` DOM parse would
/// overstate the parse share.
#[cfg(feature = "bench")]
pub fn parse_release_json(json: &[u8]) -> bool {
    serde_json::from_slice::<RawRelease>(json).is_ok()
}

/// The release payload as JSON: `data.release` is base64 twice over, then
/// gzipped. Shared by [`decode`] and [`decode_summary`].
fn release_json(secret: &DynamicObject) -> Option<Vec<u8>> {
    #[cfg(test)]
    RELEASE_DECODES.with(|count| count.set(count.get() + 1));
    let wire = secret.data.at("/data/release")?.as_str()?;
    let helm_encoded = BASE64.decode(wire).ok()?;
    let gzipped = BASE64.decode(helm_encoded).ok()?;
    let mut gz = flate2::read::GzDecoder::new(&gzipped[..]);
    let mut json = Vec::with_capacity(inflated_hint(&gzipped));
    gz.read_to_end(&mut json).ok()?;
    Some(json)
}

#[cfg(test)]
thread_local! {
    static RELEASE_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn release_decode_count() -> usize {
    RELEASE_DECODES.with(std::cell::Cell::get)
}

/// Capacity to give the gunzip buffer, from gzip's ISIZE trailer — the
/// uncompressed length mod 2^32 in the last four bytes. A release manifest
/// inflates several times over, so growing from empty re-allocates and copies
/// the whole payload repeatedly.
///
/// ISIZE comes off the cluster and is not trusted: it is a hint a corrupt or
/// hostile Secret could set to 4 GiB. Clamped to what this input could
/// plausibly inflate to, so a wrong value costs a resize, never a reservation
/// the payload cannot fill. Deflate's ceiling is about 1032:1; the absolute cap
/// is well above any real release and keeps the product in range.
fn inflated_hint(gzipped: &[u8]) -> usize {
    /// Refuse to reserve more than this up front, however large ISIZE claims.
    const MAX_HINT: usize = 64 << 20;

    let Some(trailer) = gzipped.last_chunk::<4>() else {
        return 0;
    };
    let isize_field = u32::from_le_bytes(*trailer) as usize;
    let plausible = gzipped.len().saturating_mul(1032);
    isize_field.min(plausible).min(MAX_HINT)
}

fn last_deployed_secs(raw: Option<&str>) -> Option<i64> {
    raw.and_then(|s| s.parse::<Timestamp>().ok())
        .map(|ts| ts.as_second())
}

/// Like [`decode`], but only the columns the release list shows. Half of a
/// decode is the JSON parse, and most of that is the `manifest` string and the
/// `config` DOM — neither of which the list reads. Use [`decode`] wherever
/// they are.
pub fn decode_summary(secret: &DynamicObject) -> Option<Summary> {
    let json = release_json(secret)?;
    let raw: RawSummary = serde_json::from_slice(&json).ok()?;
    Some(Summary {
        status: raw.info.status,
        chart_name: raw.chart.metadata.name,
        chart_version: raw.chart.metadata.version,
        app_version: raw.chart.metadata.app_version,
        description: raw.info.description,
        last_deployed_secs: last_deployed_secs(raw.info.last_deployed.as_deref()),
    })
}

/// Decode a release Secret into its `Release`. `None` if the secret doesn't
/// carry a `data.release` payload or it can't be decoded (corrupt, unknown
/// format, or not a Helm release secret at all) — callers treat that as
/// "unrenderable", not a crash.
pub fn decode(secret: &DynamicObject) -> Option<Release> {
    let json = release_json(secret)?;
    let raw: RawRelease = serde_json::from_slice(&json).ok()?;
    let last_deployed_secs = last_deployed_secs(raw.info.last_deployed.as_deref());

    Some(Release {
        name: raw.name,
        revision: raw.version,
        status: raw.info.status,
        chart_name: raw.chart.metadata.name,
        chart_version: raw.chart.metadata.version,
        app_version: raw.chart.metadata.app_version,
        description: raw.info.description,
        last_deployed_secs,
        notes: raw.info.notes,
        config: raw.config,
        manifest: raw.manifest,
    })
}

/// The release name from the secret's `name` label — cheap, no decode needed.
pub fn release_name(secret: &DynamicObject) -> Option<&str> {
    secret
        .metadata
        .labels
        .as_ref()?
        .get("name")
        .map(String::as_str)
}

/// Where helm-controller stores a Flux `HelmRelease`'s underlying Helm
/// release: `(release name, storage namespace)`. The release name is
/// `spec.releaseName`, defaulting to helm-controller's own composition
/// `[<targetNamespace>-]<name>`; storage is `spec.storageNamespace`,
/// defaulting to the object's namespace.
pub fn helmrelease_storage(obj: &DynamicObject) -> (String, String) {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let field = |p: &str| obj.data.at(p).and_then(Value::as_str);
    let release = match field("/spec/releaseName") {
        Some(r) => r.to_string(),
        None => match field("/spec/targetNamespace") {
            Some(t) => format!("{t}-{name}"),
            None => name,
        },
    };
    let ns = field("/spec/storageNamespace")
        .map(str::to_string)
        .unwrap_or_else(|| obj.metadata.namespace.clone().unwrap_or_default());
    (release, ns)
}

/// The revision number from the secret's `version` label — cheap, no decode
/// needed. Used to pick the latest revision per release without decoding
/// every revision's payload.
pub fn revision(secret: &DynamicObject) -> Option<i64> {
    secret
        .metadata
        .labels
        .as_ref()?
        .get("version")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn fixture_secret(release_json: &str) -> DynamicObject {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(release_json.as_bytes()).unwrap();
        let gzipped = gz.finish().unwrap();
        let helm_b64 = BASE64.encode(gzipped);
        let wire_b64 = BASE64.encode(helm_b64);

        let data: Value = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": "sh.helm.release.v1.myrelease.v2",
                "namespace": "default",
                "labels": {
                    "owner": "helm",
                    "name": "myrelease",
                    "version": "2",
                    "status": "deployed",
                },
            },
            "type": "helm.sh/release.v1",
            "data": { "release": wire_b64 },
        }))
        .unwrap();

        serde_json::from_value(data).unwrap()
    }

    /// The SIMD engine must decode byte-for-byte what the scalar one did,
    /// including the padding and rejection behaviour of a malformed payload.
    #[test]
    fn simd_and_scalar_base64_agree() {
        let scalar = base64::engine::general_purpose::STANDARD;
        for len in [0usize, 1, 2, 3, 15, 16, 17, 63, 64, 65, 588, 4096] {
            let raw: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let encoded = scalar.encode(&raw);
            assert_eq!(BASE64.encode(&raw), encoded, "encode len {len}");
            assert_eq!(BASE64.decode(&encoded).unwrap(), raw, "decode len {len}");
        }
        // Both reject the same malformed input.
        for bad in ["!!!!", "aGVsbG8", "a", "====", "aGVs bG8="] {
            assert_eq!(
                BASE64.decode(bad).is_err(),
                scalar.decode(bad).is_err(),
                "malformed {bad:?}"
            );
        }
    }

    /// Escapes and multibyte text have to survive the decode intact.
    #[test]
    fn escaped_and_multibyte_payloads_decode_intact() {
        let secret = fixture_secret(
            r#"{
                "name": "myrelease",
                "version": 1,
                "info": {
                    "status": "deployed",
                    "description": "quote \" backslash \\ tab \t",
                    "notes": "caf\u00e9 \u2014 done\nsecond line"
                },
                "chart": {"metadata": {"name": "c", "version": "1.0.0"}}
            }"#,
        );
        let release = decode(&secret).unwrap();
        assert_eq!(release.description, "quote \" backslash \\ tab \t");
        assert_eq!(release.notes, "café — done\nsecond line");
    }

    /// A payload that is not JSON at all is unrenderable, not a panic.
    #[test]
    fn a_corrupt_payload_decodes_to_none() {
        let secret = fixture_secret("{not json at all");
        assert!(decode(&secret).is_none());
        assert!(decode_summary(&secret).is_none());
    }

    /// `decode` and `decode_summary` read the same payload and must agree.
    #[test]
    fn decode_and_summary_agree_on_the_same_secret() {
        let secret = fixture_secret(
            r#"{
                "name": "myrelease",
                "version": 2,
                "info": {"status": "deployed", "description": "Upgrade complete",
                         "last_deployed": "2026-07-01T10:00:00Z"},
                "chart": {"metadata": {"name": "nginx", "version": "1.2.3",
                                       "appVersion": "1.25"}}
            }"#,
        );
        let full = decode(&secret).unwrap();
        let summary = decode_summary(&secret).unwrap();
        assert_eq!(full.status, summary.status);
        assert_eq!(full.chart_name, summary.chart_name);
        assert_eq!(full.chart_version, summary.chart_version);
        assert_eq!(full.app_version, summary.app_version);
        assert_eq!(full.description, summary.description);
        assert_eq!(full.last_deployed_secs, summary.last_deployed_secs);
        // Re-decoding the same secret is stable.
        assert_eq!(decode(&secret).unwrap().notes, full.notes);
    }

    #[test]
    fn inflated_hint_reads_the_gzip_isize_trailer() {
        let payload = "x".repeat(5000);
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(payload.as_bytes()).unwrap();
        let gzipped = gz.finish().unwrap();

        assert_eq!(inflated_hint(&gzipped), payload.len());
    }

    #[test]
    fn inflated_hint_clamps_a_lying_trailer_to_what_deflate_could_produce() {
        // A hostile Secret claiming 4 GiB - 1 from a handful of bytes.
        let mut gzipped = vec![0u8; 16];
        gzipped.extend_from_slice(&u32::MAX.to_le_bytes());

        assert_eq!(inflated_hint(&gzipped), gzipped.len() * 1032);
    }

    #[test]
    fn inflated_hint_survives_a_truncated_stream() {
        assert_eq!(inflated_hint(&[]), 0);
        assert_eq!(inflated_hint(&[0x1f, 0x8b]), 0);
    }

    #[test]
    fn a_release_that_inflates_past_the_hint_still_decodes() {
        // Highly compressible: the true inflated size far exceeds any hint
        // derived from the compressed bytes, so `read_to_end` must still grow.
        let notes = "n".repeat(400_000);
        let secret = fixture_secret(&format!(
            r#"{{"name":"big","version":1,"info":{{"status":"deployed","notes":"{notes}"}}}}"#
        ));

        let release = decode(&secret).unwrap();
        assert_eq!(release.notes.len(), notes.len());
    }

    #[test]
    fn decodes_a_release_secret() {
        let secret = fixture_secret(
            r#"{
                "name": "myrelease",
                "namespace": "default",
                "version": 2,
                "info": {
                    "status": "deployed",
                    "description": "Upgrade complete",
                    "last_deployed": "2024-01-15T10:30:00Z",
                    "notes": "Thanks for installing!"
                },
                "chart": {
                    "metadata": { "name": "mychart", "version": "1.2.3", "appVersion": "4.5.6" },
                    "values": { "replicaCount": 1 }
                },
                "config": { "replicaCount": 3 },
                "manifest": "apiVersion: v1\nkind: ConfigMap\n"
            }"#,
        );

        let rel = decode(&secret).expect("should decode");
        assert_eq!(rel.name, "myrelease");
        assert_eq!(rel.revision, 2);
        assert_eq!(rel.status, "deployed");
        assert_eq!(rel.chart_name, "mychart");
        assert_eq!(rel.chart_version, "1.2.3");
        assert_eq!(rel.app_version, "4.5.6");
        assert_eq!(rel.description, "Upgrade complete");
        assert_eq!(rel.notes, "Thanks for installing!");
        assert!(rel.manifest.contains("ConfigMap"));
        assert_eq!(
            rel.config.get("replicaCount").and_then(Value::as_i64),
            Some(3)
        );
        assert!(rel.last_deployed_secs.is_some());

        assert_eq!(release_name(&secret), Some("myrelease"));
        assert_eq!(revision(&secret), Some(2));
    }

    /// `decode_summary` skips `manifest`/`config`/`notes` for speed, so the
    /// fields it does return must still agree with the full decode — a
    /// divergence here would show up as wrong cells in the release list.
    #[test]
    fn summary_decode_agrees_with_the_full_decode() {
        let secret = fixture_secret(
            r#"{
                "name": "myrelease",
                "version": 2,
                "info": {
                    "status": "deployed",
                    "description": "Upgrade complete",
                    "last_deployed": "2024-01-15T10:30:00Z",
                    "notes": "Thanks for installing!"
                },
                "chart": {
                    "metadata": { "name": "mychart", "version": "1.2.3", "appVersion": "4.5.6" }
                },
                "config": { "replicaCount": 3 },
                "manifest": "apiVersion: v1\nkind: ConfigMap\n"
            }"#,
        );

        let full = decode(&secret).expect("should decode");
        let brief = decode_summary(&secret).expect("should decode");
        assert_eq!(brief.status, full.status);
        assert_eq!(brief.chart_name, full.chart_name);
        assert_eq!(brief.chart_version, full.chart_version);
        assert_eq!(brief.app_version, full.app_version);
        assert_eq!(brief.description, full.description);
        assert_eq!(brief.last_deployed_secs, full.last_deployed_secs);
    }

    #[test]
    fn summary_decode_returns_none_for_non_release_secret() {
        let data: Value = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "not-a-release", "namespace": "default" },
            "data": {},
        });
        let secret: DynamicObject = serde_json::from_value(data).unwrap();
        assert!(decode_summary(&secret).is_none());
    }

    #[test]
    fn decode_returns_none_for_non_release_secret() {
        let data: Value = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "some-secret", "namespace": "default" },
            "data": { "password": "aHVudGVyMg==" },
        });
        let secret: DynamicObject = serde_json::from_value(data).unwrap();
        assert!(decode(&secret).is_none());
    }
}

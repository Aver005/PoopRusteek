//! `manifest.json` релиза и сравнение версий. Формат пишет CI
//! (`scripts/ci/make-manifest.sh`) — меняются только вместе.

use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Имя ассета с манифестом в каждом релизе (stable и dev).
pub const MANIFEST_ASSET: &str = "manifest.json";

/// Поддерживаемая версия формата; новую схему старый клиент не трогает.
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// Версия из `Cargo.toml` на момент сборки.
    pub version: String,
    /// Тег релиза, откуда качать ассеты (`v0.2.0` или `dev`).
    pub tag: String,
    /// Коммит сборки — отличает dev-сборки с одной версией.
    #[serde(default)]
    pub commit: String,
    /// Имя ассета → SHA-256 несжатого бинарника.
    pub assets: BTreeMap<String, String>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|e| format!("release manifest is malformed: {e}"))?;
        if manifest.schema != SCHEMA {
            return Err(format!(
                "release manifest schema {} isn't supported by this build — update manually",
                manifest.schema
            ));
        }
        if !is_safe_tag(&manifest.tag) {
            return Err(format!(
                "release manifest has an invalid tag {:?}",
                manifest.tag
            ));
        }
        Ok(manifest)
    }

    /// Хэш ассета в нижнем регистре; ошибка различает «нет сборки» и «битый хэш».
    pub fn sha256_for(&self, asset: &str) -> Result<String, String> {
        let hash = self
            .assets
            .get(asset)
            .ok_or_else(|| format!("no build for {asset}"))?;
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(hash.to_ascii_lowercase())
        } else {
            Err(format!("a malformed checksum for {asset}"))
        }
    }
}

/// Тег идёт в URL — только безопасные символы и начало с буквы/цифры (не `..`).
fn is_safe_tag(tag: &str) -> bool {
    tag.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// SemVer `MAJOR.MINOR.PATCH[-pre][+build]`; build-метаданные не влияют на порядок.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Option<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let text = text.split_once('+').map_or(text, |(head, _)| head);
        let (core_text, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut parts = core_text.split('.');
        let mut core = [0u64; 3];
        for slot in &mut core {
            *slot = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self { core, pre })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Пререлиз младше релиза с тем же ядром (SemVer §11).
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_prerelease(a, b),
            })
    }
}

/// Части через точку: числа — численно и младше текстовых, короткий префикс младше.
fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        let order = match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            },
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(schema: u32, tag: &str, hash: &str) -> String {
        format!(
            r#"{{"schema":{schema},"version":"0.2.0","tag":"{tag}","commit":"abc",
               "assets":{{"pooprusteek-linux-x86_64":"{hash}"}}}}"#
        )
    }

    #[test]
    fn manifest_parses_and_finds_assets() {
        let hash = "A".repeat(64);
        let manifest = Manifest::parse(manifest_json(1, "v0.2.0", &hash).as_bytes()).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.commit, "abc");
        assert_eq!(
            manifest.sha256_for("pooprusteek-linux-x86_64"),
            Ok("a".repeat(64))
        );
        let missing = manifest.sha256_for("pooprusteek-macos-arm64").unwrap_err();
        assert!(missing.contains("no build"), "{missing}");
    }

    #[test]
    fn manifest_rejects_bad_hash_schema_and_tag() {
        let short = Manifest::parse(manifest_json(1, "dev", "deadbeef").as_bytes()).unwrap();
        let bad = short.sha256_for("pooprusteek-linux-x86_64").unwrap_err();
        assert!(bad.contains("malformed checksum"), "{bad}");

        let hash = "b".repeat(64);
        assert!(Manifest::parse(manifest_json(2, "dev", &hash).as_bytes()).is_err());
        for tag in ["../evil", "", "..", ".", "-x"] {
            assert!(
                Manifest::parse(manifest_json(1, tag, &hash).as_bytes()).is_err(),
                "tag {tag:?} must be rejected"
            );
        }
        assert!(Manifest::parse(b"not json").is_err());
    }

    #[test]
    fn version_orders_like_semver() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.10.0") > v("0.9.0"), "numeric, not lexicographic");
        assert!(v("0.2.0") > v("0.2.0-rc.1"));
        assert!(v("0.2.0-rc.2") > v("0.2.0-rc.1"));
        assert!(
            v("0.2.0-rc.10") > v("0.2.0-rc.2"),
            "numeric identifiers compare as numbers"
        );
        assert!(
            v("0.2.0-alpha.1") > v("0.2.0-alpha"),
            "longer set wins on equal prefix"
        );
        assert!(v("0.2.0-alpha") > v("0.2.0-1"), "text ranks above numbers");
        assert!(v("0.2.0-beta") > v("0.2.0-alpha.9"));
        assert_eq!(v("v0.2.0"), v("0.2.0"));
        assert_eq!(v("0.2.0+abc"), v("0.2.0"));
    }

    #[test]
    fn version_rejects_junk() {
        for bad in ["", "1", "1.2", "1.2.3.4", "1.x.3", "1.2.3-", "latest"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }
}

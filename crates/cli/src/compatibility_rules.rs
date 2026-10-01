//! Versioned, reviewable compatibility data; extensions cannot weaken built-ins.
use crate::project::error;
use dependency_check_updates_core::{DcuError, pad_to_three_segments};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgpRule {
    pub min_gradle: String,
    pub min_jdk: u64,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct KotlinRule {
    pub from: String,
    pub to: String,
    pub min_gradle: String,
    pub max_gradle: String,
    pub min_agp: String,
    pub max_agp: String,
    pub bytecode_min_agp: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Rules {
    schema_version: u32,
    verified_at: String,
    sources: Vec<String>,
    #[serde(default)]
    pub agp: BTreeMap<String, AgpRule>,
    #[serde(default)]
    pub sdk: BTreeMap<u64, String>,
    #[serde(default)]
    pub jdk: BTreeMap<u64, String>,
    #[serde(default)]
    pub kotlin: Vec<KotlinRule>,
    #[serde(skip)]
    pub extensions: bool,
    #[serde(skip)]
    pub provenance: Vec<Provenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Provenance {
    pub verified_at: String,
    pub sources: Vec<String>,
    pub user_supplied: bool,
    pub age_days: i64,
    pub stale: bool,
    pub future: bool,
}

// Gregorian validation and days since 0001-01-01. No local timezone dependence.
fn date_days(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
    {
        return None;
    }
    let year = text[..4].parse::<i64>().ok()?;
    let month = text[5..7].parse::<usize>().ok()?;
    let day = text[8..].parse::<i64>().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if year == 0 || !(1..=12).contains(&month) || day < 1 || day > months[month - 1] {
        return None;
    }
    let prior = year - 1;
    Some(
        365 * prior + prior / 4 - prior / 100
            + prior / 400
            + months[..month - 1].iter().sum::<i64>()
            + day
            - 1,
    )
}

#[cfg(not(test))]
fn today_days() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86400;
    719_162 + i64::try_from(elapsed).unwrap_or(i64::MAX - 719_162)
}

#[cfg(test)]
fn today_days() -> i64 {
    // Keep fixed-response unit tests independent of the wall clock. Production
    // binaries (including CLI process tests) use the real UTC date above.
    date_days("2026-10-01").expect("fixture clock")
}

impl Provenance {
    fn new(date: &str, sources: Vec<String>, user_supplied: bool, today: i64) -> Self {
        let age_days = today - date_days(date).expect("validated Gregorian date");
        Self {
            verified_at: date.into(),
            sources,
            user_supplied,
            age_days,
            stale: age_days > 180,
            future: age_days < 0,
        }
    }
}

pub(crate) fn version(value: &str) -> Option<semver::Version> {
    semver::Version::parse(&pad_to_three_segments(value))
        .ok()
        .filter(|v| v.pre.is_empty() && v.build.is_empty())
}

impl Rules {
    pub fn builtin() -> Self {
        Self::parse(include_str!("../data/compatibility-v1.json"))
            .expect("checked built-in compatibility data")
    }

    pub fn load(path: Option<&Path>) -> Result<Self, DcuError> {
        let mut rules = Self::builtin();
        if let Some(path) = path {
            if std::fs::metadata(path)
                .map_err(|source| DcuError::Io {
                    path: path.to_owned(),
                    source,
                })?
                .len()
                > 1024 * 1024
            {
                return Err(error("compatibility rules", "file exceeds size limit"));
            }
            let text = std::fs::read_to_string(path).map_err(|source| DcuError::Io {
                path: path.to_owned(),
                source,
            })?;
            let extra = Self::parse(&text)?;
            rules
                .provenance
                .extend(extra.provenance.into_iter().map(|mut p| {
                    p.user_supplied = true;
                    p
                }));
            rules.extensions = true;
            merge_map(&mut rules.agp, extra.agp)?;
            merge_map(&mut rules.sdk, extra.sdk)?;
            merge_map(&mut rules.jdk, extra.jdk)?;
            for rule in extra.kotlin {
                if rules.kotlin.iter().any(|r| overlap(r, &rule) && r != &rule) {
                    return Err(error(
                        "compatibility rules",
                        "overlapping Kotlin rule cannot replace built-in conditions",
                    ));
                }
                if !rules.kotlin.contains(&rule) {
                    rules.kotlin.push(rule);
                }
            }
        }
        Ok(rules)
    }

    fn parse(text: &str) -> Result<Self, DcuError> {
        let mut rules: Self = serde_json::from_str(text)
            .map_err(|_| error("compatibility rules", "invalid rule document"))?;
        if rules.schema_version != 1
            || date_days(&rules.verified_at).is_none()
            || rules.sources.is_empty()
            || rules.sources.iter().any(|s| {
                reqwest::Url::parse(s).ok().is_none_or(|u| {
                    u.scheme() != "https"
                        || !u.username().is_empty()
                        || u.password().is_some()
                        || !matches!(
                            u.host_str(),
                            Some("developer.android.com" | "kotlinlang.org" | "docs.gradle.org")
                        )
                })
            })
        {
            return Err(error(
                "compatibility rules",
                "schema 1, verification date and official source URLs required",
            ));
        }
        let valid = |v: &str| version(v).is_some();
        if rules.agp.iter().any(|(key, r)| {
            key.split('.').count() != 2
                || !valid(key)
                || !valid(&r.min_gradle)
                || !(8..=100).contains(&r.min_jdk)
        }) || rules.sdk.iter().any(|(n, v)| *n == 0 || !valid(v))
            || rules
                .jdk
                .iter()
                .any(|(n, v)| !(8..=100).contains(n) || !valid(v))
            || rules.kotlin.iter().any(|r| {
                ![
                    &r.from,
                    &r.to,
                    &r.min_gradle,
                    &r.max_gradle,
                    &r.min_agp,
                    &r.max_agp,
                    &r.bytecode_min_agp,
                ]
                .iter()
                .all(|v| valid(v))
                    || version(&r.from) > version(&r.to)
                    || version(&r.min_gradle) > version(&r.max_gradle)
                    || version(&r.min_agp) > version(&r.max_agp)
            })
        {
            return Err(error(
                "compatibility rules",
                "invalid versions or inverted compatibility bounds",
            ));
        }
        for (i, r) in rules.kotlin.iter().enumerate() {
            if rules
                .kotlin
                .iter()
                .skip(i + 1)
                .any(|other| overlap(r, other))
            {
                return Err(error("compatibility rules", "overlapping Kotlin ranges"));
            }
        }
        rules.provenance.push(Provenance::new(
            &rules.verified_at,
            rules.sources.clone(),
            false,
            today_days(),
        ));
        Ok(rules)
    }
}

fn overlap(a: &KotlinRule, b: &KotlinRule) -> bool {
    version(&a.from) <= version(&b.to) && version(&b.from) <= version(&a.to)
}

fn merge_map<K: Ord, V: PartialEq>(
    base: &mut BTreeMap<K, V>,
    extra: BTreeMap<K, V>,
) -> Result<(), DcuError> {
    for (key, value) in extra {
        if base.get(&key).is_some_and(|existing| existing != &value) {
            return Err(error(
                "compatibility rules",
                "extension cannot replace built-in conditions",
            ));
        }
        base.insert(key, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gregorian_dates_and_freshness_are_checked_deterministically() {
        for invalid in [
            "2026-99-99",
            "2026-02-29",
            "1900-02-29",
            "0000-01-01",
            "2026-04-31",
            "2026-10-1",
        ] {
            assert!(date_days(invalid).is_none(), "{invalid}");
        }
        assert!(date_days("2000-02-29").is_some());
        assert_eq!(date_days("1970-01-01"), Some(719_162));
        let today = date_days("2026-10-01").unwrap();
        let stale = Provenance::new("2026-01-01", Vec::new(), false, today);
        assert!(stale.stale);
        assert!(!stale.future);
        let future = Provenance::new("2026-10-02", Vec::new(), true, today);
        assert!(future.future);
        assert!(!future.stale);
        assert_eq!(
            Provenance::new("2026-10-01", Vec::new(), false, today).age_days,
            0
        );
        let text =
            include_str!("../data/compatibility-v1.json").replace("2026-10-01", "2026-99-99");
        assert!(Rules::parse(&text).is_err());
    }
    #[test]
    fn builtin_and_extensions_are_validated_and_cannot_weaken_rules() {
        let builtin = Rules::builtin();
        assert_eq!(builtin.jdk[&27], "9.8.0");
        assert!(Rules::parse(r#"{"schemaVersion":1,"verifiedAt":"2026-10-01","sources":["https://private.invalid"],"agp":{}}"#).is_err());
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rules.json");
        std::fs::write(&path, r#"{"schemaVersion":1,"verifiedAt":"2026-10-01","sources":["https://developer.android.com/build/releases/about-agp"],"agp":{"10.0":{"minGradle":"10.1","minJdk":21}}}"#).unwrap();
        assert_eq!(Rules::load(Some(&path)).unwrap().agp["10.0"].min_jdk, 21);
        std::fs::write(&path, r#"{"schemaVersion":1,"verifiedAt":"2026-10-01","sources":["https://developer.android.com/build/releases/about-agp"],"agp":{"8.5":{"minGradle":"1.0","minJdk":8}}}"#).unwrap();
        assert!(Rules::load(Some(&path)).is_err());
    }
}

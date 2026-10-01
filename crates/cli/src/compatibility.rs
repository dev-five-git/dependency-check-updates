//! Deliberately bounded official compatibility checks, not a dependency solver.
use crate::compatibility_rules::Rules;
use crate::project::Document;
use dependency_check_updates_core::{DependencySpec, PlannedUpdate, pad_to_three_segments};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) type Plans = HashMap<PathBuf, Vec<PlannedUpdate>>;

pub(crate) fn related(name: &str) -> bool {
    matches!(
        name,
        "com.android.tools.build:gradle"
            | "org.jetbrains.kotlin:kotlin-gradle-plugin"
            | "gradle"
            | "jdk"
            | "android.compileSdk"
            | "android.targetSdk"
    ) || matches!(name, "com.android.application" | "com.android.library")
        || name.starts_with("org.jetbrains.kotlin.")
}
fn agp(name: &str) -> bool {
    name == "com.android.tools.build:gradle"
        || matches!(name, "com.android.application" | "com.android.library")
}
fn kotlin(name: &str) -> bool {
    name == "org.jetbrains.kotlin:kotlin-gradle-plugin" || name.starts_with("org.jetbrains.kotlin.")
}
fn ver(v: &str) -> Option<semver::Version> {
    semver::Version::parse(&pad_to_three_segments(v.trim_start_matches('v'))).ok()
}

pub(crate) fn scope(path: &Path, documents: &HashMap<PathBuf, Document>) -> PathBuf {
    for parent in path.ancestors().skip(1) {
        if documents.contains_key(&parent.join("gradle/wrapper/gradle-wrapper.properties"))
            || documents.contains_key(&parent.join("settings.gradle"))
            || documents.contains_key(&parent.join("settings.gradle.kts"))
        {
            return parent.to_owned();
        }
    }
    path.parent().unwrap_or(path).to_owned()
}

fn proposed<'a>(plans: &'a Plans, path: &Path, dep: &'a DependencySpec) -> &'a str {
    plans
        .get(path)
        .and_then(|us| {
            us.iter().find(|u| {
                u.name == dep.name && u.from == dep.current_req && u.section == dep.section
            })
        })
        .map_or(&dep.current_req, |u| &u.to)
}

/// Return per-build status and block all coupled changes on a definite conflict.
/// A second pass using retained current versions avoids inconsistent fallbacks.
pub(crate) fn guard(
    documents: &HashMap<PathBuf, Document>,
    plans: &mut Plans,
    strict: bool,
    rules: &Rules,
) -> HashMap<PathBuf, String> {
    let mut statuses = HashMap::new();
    let mut scopes: Vec<_> = documents.keys().map(|p| scope(p, documents)).collect();
    scopes.sort();
    scopes.dedup();
    for root in scopes {
        let members: Vec<_> = documents
            .values()
            .filter(|d| scope(&d.path, documents) == root)
            .collect();
        let mut values = Vec::new();
        for d in &members {
            for issue in &d.context_issues {
                values.push(("unsupported-build-connection", issue.as_str()));
            }
            for plugin in &d.applied_plugins {
                values.push(("applied-kotlin-android", plugin.as_str()));
            }
            for e in &d.entries {
                if related(&e.dep.name) {
                    values.push((e.dep.name.as_str(), proposed(plans, &d.path, &e.dep)));
                }
            }
            for (source, dep) in &d.resolved_uses {
                if related(&dep.name) {
                    values.push((dep.name.as_str(), proposed(plans, source, dep)));
                }
            }
        }
        // Tool pins declared at an ancestor project root apply to nested builds.
        for d in documents.values().filter(|d| {
            root.starts_with(d.path.parent().unwrap_or(&d.path))
                && !members.iter().any(|m| m.path == d.path)
        }) {
            for e in &d.entries {
                if e.dep.name == "jdk" {
                    values.push(("jdk", proposed(plans, &d.path, &e.dep)));
                }
            }
        }
        if !values.iter().any(|(n, _)| *n == "gradle") {
            for parent in root.ancestors() {
                if let Some(wrapper) =
                    documents.get(&parent.join("gradle/wrapper/gradle-wrapper.properties"))
                {
                    for e in &wrapper.entries {
                        if e.dep.name == "gradle" {
                            values.push(("gradle", proposed(plans, &wrapper.path, &e.dep)));
                        }
                    }
                    break;
                }
            }
        }
        let coupled = values.iter().any(|(n, _)| {
            agp(n)
                || kotlin(n)
                || matches!(*n, "gradle" | "android.compileSdk" | "android.targetSdk")
        });
        if !coupled {
            continue;
        }
        let mut status = check_with_rules(&values, rules);
        if strict && coupled && status.starts_with("unverified:") {
            status = format!(
                "unverified: strict compatibility blocks this combination; {}",
                status.trim_start_matches("unverified: ")
            );
        }
        if status.starts_with("conflict:")
            || (strict && coupled && status.starts_with("unverified:"))
        {
            for path in block_coupled(documents, plans, &root, &members) {
                statuses.insert(path, status.clone());
            }
        }
        for d in members {
            if !statuses.get(&d.path).is_some_and(|s: &String| {
                s.starts_with("conflict:") || s.contains("strict compatibility blocks")
            }) {
                statuses.insert(d.path.clone(), status.clone());
            }
        }
    }
    statuses
}

fn block_coupled(
    documents: &HashMap<PathBuf, Document>,
    plans: &mut Plans,
    root: &Path,
    members: &[&Document],
) -> Vec<PathBuf> {
    let before: HashMap<_, _> = plans.iter().map(|(p, u)| (p.clone(), u.len())).collect();
    for d in members {
        if let Some(updates) = plans.get_mut(&d.path) {
            updates.retain(|u| !related(&u.name));
        }
        for (source, dep) in &d.resolved_uses {
            if related(&dep.name)
                && let Some(updates) = plans.get_mut(source)
            {
                updates.retain(|u| {
                    u.name != dep.name || u.from != dep.current_req || u.section != dep.section
                });
            }
        }
    }
    // Ancestor JDK/wrapper pins apply to this build too.
    for (path, updates) in plans.iter_mut() {
        if root.starts_with(path.parent().unwrap_or(path)) {
            updates.retain(|u| u.name != "jdk");
        }
        let wrapper_root = path.parent().and_then(Path::parent).and_then(Path::parent);
        if documents
            .get(path)
            .is_some_and(|d| d.distribution.is_some())
            && wrapper_root.is_some_and(|p| root.starts_with(p))
        {
            updates.retain(|u| u.name != "gradle");
        }
    }
    plans
        .iter()
        .filter(|(path, u)| before.get(*path).is_some_and(|n| *n != u.len()))
        .map(|(path, _)| path.clone())
        .collect()
}

#[cfg(test)]
fn check(values: &[(&str, &str)]) -> String {
    check_with_rules(values, &Rules::builtin())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn check_with_rules(values: &[(&str, &str)], rules: &Rules) -> String {
    let agps: Vec<_> = values
        .iter()
        .filter(|(n, _)| agp(n))
        .filter_map(|(_, v)| ver(v))
        .collect();
    let gradles: Vec<_> = values
        .iter()
        .filter(|(n, _)| *n == "gradle")
        .filter_map(|(_, v)| ver(v))
        .collect();
    let kotlins: Vec<_> = values
        .iter()
        .filter(|(n, _)| kotlin(n))
        .filter_map(|(_, v)| ver(v))
        .collect();
    let jdks: Vec<_> = values
        .iter()
        .filter(|(n, _)| *n == "jdk")
        .filter_map(|(_, v)| ver(v))
        .collect();
    let android = !agps.is_empty()
        || values.iter().any(|(n, _)| {
            matches!(
                *n,
                "applied-kotlin-android" | "android.compileSdk" | "android.targetSdk"
            )
        });
    let mut unknown = (android && agps.is_empty())
        || values.iter().any(|(n, v)| related(n) && ver(v).is_none())
        || gradles.is_empty()
        || jdks.is_empty()
        || values
            .iter()
            .any(|(n, _)| *n == "unsupported-build-connection");
    for a in &agps {
        if let Some(rule) = rules.agp.get(&format!("{}.{}", a.major, a.minor)) {
            let min = rule.min_gradle.as_str();
            if gradles.iter().any(|g| g < &ver(min).unwrap()) {
                return format!("conflict: AGP {a} requires Gradle >= {min}");
            }
            if jdks.iter().any(|j| j.major < rule.min_jdk) {
                return format!("conflict: AGP {a} requires JDK >= {}", rule.min_jdk);
            }
        } else {
            unknown = true;
        }
        for (name, value) in values
            .iter()
            .filter(|(n, _)| matches!(*n, "android.compileSdk" | "android.targetSdk"))
        {
            let minimum = match value.parse::<u64>() {
                Ok(1..=33) => None,
                Ok(api) if rules.sdk.contains_key(&api) => Some(rules.sdk[&api].as_str()),
                _ => {
                    unknown = true;
                    None
                }
            };
            if let Some(min) = minimum
                && a < &ver(min).unwrap()
            {
                return format!("conflict: {name} {value} requires AGP >= {min}");
            }
        }
        if a.major >= 9 && values.iter().any(|(n, _)| *n == "applied-kotlin-android") {
            return "conflict: AGP 9 built-in Kotlin requires an explicit migration of external Kotlin Android plugins; automatic migration is unsupported".into();
        }
    }
    for k in &kotlins {
        let bounds = rules
            .kotlin
            .iter()
            .find(|r| k >= &ver(&r.from).unwrap() && k <= &ver(&r.to).unwrap());
        let min_agp = bounds
            .or_else(|| {
                rules.kotlin.iter().find(|r| {
                    let from = ver(&r.from).unwrap();
                    from.major == k.major && from.minor == k.minor
                })
            })
            .map(|r| r.bytecode_min_agp.as_str());
        if let Some(min) = min_agp {
            if agps.iter().any(|a| a < &ver(min).unwrap()) {
                return format!("conflict: Kotlin {k} bytecode requires AGP >= {min}");
            }
        } else {
            unknown = true;
        }
        if let Some(rule) = bounds {
            let (gmin, gmax, amin, amax) = (
                rule.min_gradle.as_str(),
                rule.max_gradle.as_str(),
                rule.min_agp.as_str(),
                rule.max_agp.as_str(),
            );
            if gradles.iter().any(|g| g < &ver(gmin).unwrap())
                || agps.iter().any(|a| a < &ver(amin).unwrap())
            {
                return format!("conflict: Kotlin {k} requires Gradle >= {gmin}, AGP >= {amin}");
            }
            if gradles.iter().any(|g| g > &ver(gmax).unwrap())
                || agps.iter().any(|a| a > &ver(amax).unwrap())
            {
                unknown = true;
            }
        } else {
            unknown = true;
        }
    }
    for g in &gradles {
        for j in &jdks {
            if g.major >= 9 && j.major < 17 {
                return "conflict: Gradle 9 requires JDK >= 17".into();
            }
            let min = rules.jdk.get(&j.major).map(String::as_str);
            if let Some(min) = min {
                if g < &ver(min).unwrap() {
                    return format!("conflict: JDK {} requires Gradle >= {min}", j.major);
                }
            } else {
                unknown = true;
            }
        }
    }
    if rules.provenance.iter().any(|p| p.stale || p.future) {
        "unverified: compatibility rule verification date is stale or in the future".into()
    } else if unknown {
        "unverified: missing tool pins or combination outside documented compatibility coverage"
            .into()
    } else if rules.extensions {
        "verified: supported conditions checked against built-in and explicitly supplied rule data (extensions are user-reviewed, not independently authenticated)".into()
    } else {
        "verified: supported AGP/Gradle/Kotlin/JDK/SDK conditions checked".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_conflicts_and_unknown() {
        assert!(
            check(&[
                ("com.android.tools.build:gradle", "8.7.3"),
                ("gradle", "8.8"),
                ("jdk", "17")
            ])
            .starts_with("conflict:")
        );
        assert!(
            check(&[
                ("com.android.tools.build:gradle", "8.7.2"),
                ("gradle", "8.9"),
                ("jdk", "17"),
                ("org.jetbrains.kotlin:kotlin-gradle-plugin", "2.1.21")
            ])
            .starts_with("verified:")
        );
        assert!(
            check(&[
                ("com.android.tools.build:gradle", "42.0"),
                ("gradle", "40.0")
            ])
            .starts_with("unverified:")
        );
        assert!(check(&[("gradle", "8.9"), ("jdk", "25")]).starts_with("conflict:"));
        assert!(
            check(&[
                ("com.android.tools.build:gradle", "8.5.1"),
                ("org.jetbrains.kotlin:kotlin-gradle-plugin", "2.2.0")
            ])
            .starts_with("conflict:")
        );
    }
}

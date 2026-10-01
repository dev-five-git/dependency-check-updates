//! Official metadata clients for Maven, Gradle distributions and project tools.
use crate::project::{Document, Entry, error};
use dependency_check_updates_core::{
    DcuError, DependencySection as Section, MetadataCache, Patch, PlannedUpdate, ResolvedVersion,
    TargetLevel, highest_stable, pad_to_three_segments, select_version,
};
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use std::fmt::Write;
use std::sync::Arc;

pub(crate) struct ToolRegistry {
    pub cache: MetadataCache,
    pub endpoints: Endpoints,
    npm: dependency_check_updates_node::NpmRegistry,
    pub private_repositories: std::collections::HashMap<String, reqwest::header::HeaderMap>,
    pub private_cache: Option<MetadataCache>,
}

#[derive(Clone)]
pub(crate) struct Endpoints {
    pub gradle: String,
    pub distributions: String,
    pub node: String,
    pub rust: String,
    pub github: String,
    pub android: String,
    pub jdk: String,
    pub npm: String,
    pub yarn: String,
    pub yarn_downloads: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            gradle: "https://services.gradle.org/versions/all".into(),
            distributions: "https://services.gradle.org/distributions".into(),
            node: "https://nodejs.org/dist/index.json".into(),
            rust: "https://static.rust-lang.org/dist/channel-rust-stable.toml".into(),
            github: "https://api.github.com".into(),
            android: "https://dl.google.com/android/repository/repository2-1.xml".into(),
            jdk: "https://api.adoptium.net/v3/info/release_versions?image_type=jdk&release_type=ga&page_size=50&sort_method=DATE&sort_order=DESC".into(),
            npm: "https://registry.npmjs.org".into(),
            yarn: "https://repo.yarnpkg.com/tags".into(),
            yarn_downloads: "https://repo.yarnpkg.com".into(),
        }
    }
}

impl ToolRegistry {
    pub fn fresh_execution(&self) -> Self {
        let mut result = Self::with_endpoints(self.endpoints.clone());
        result
            .private_repositories
            .clone_from(&self.private_repositories);
        result.private_cache = self
            .private_cache
            .as_ref()
            .map(|_| MetadataCache::without_redirects());
        result
    }
    pub fn new() -> Self {
        Self::with_endpoints(Endpoints::default())
    }
    pub fn with_endpoints(endpoints: Endpoints) -> Self {
        let cache = MetadataCache::new();
        Self {
            npm: dependency_check_updates_node::NpmRegistry::with_cache(
                &endpoints.npm,
                cache.clone(),
            ),
            cache,
            endpoints,
            private_repositories: std::collections::HashMap::new(),
            private_cache: None,
        }
    }

    async fn text(&self, url: &str, name: &str) -> Result<String, DcuError> {
        let bytes = self.bytes(url, name, MetadataCache::METADATA_LIMIT).await?;
        String::from_utf8(bytes.to_vec()).map_err(|e| error(name, e.to_string()))
    }
    async fn bytes(&self, url: &str, name: &str, limit: usize) -> Result<Arc<[u8]>, DcuError> {
        self.cache
            .get(url, reqwest::header::HeaderMap::new(), limit, name)
            .await
    }
    async fn json(&self, url: &str, name: &str) -> Result<Value, DcuError> {
        serde_json::from_str(&self.text(url, name).await?).map_err(|e| error(name, e.to_string()))
    }

    #[allow(clippy::too_many_lines)]
    pub async fn resolve(
        &self,
        entry: &Entry,
        target: TargetLevel,
    ) -> Result<ResolvedVersion, DcuError> {
        let dep = &entry.dep;
        if entry
            .reason
            .as_ref()
            .is_some_and(|r| !r.starts_with("channel preserved"))
        {
            return Err(error(&dep.name, entry.reason.clone().unwrap()));
        }
        if matches!(dep.section, Section::Maven | Section::GradlePlugin) {
            return self.maven(entry, target).await;
        }
        if dep.name == "yarn"
            && dep
                .current_req
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|v| v >= 2)
        {
            let json = self.json(&self.endpoints.yarn, "yarn").await?;
            let tags = &json["tags"];
            let versions = if let Some(tags) = tags.as_array() {
                tags.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            } else if let Some(tags) = tags.as_object() {
                tags.keys().cloned().collect()
            } else {
                return Err(error("yarn", "invalid official Yarn tags"));
            };
            return choose(&versions, &dep.current_req, target, None);
        }
        let (versions, newest) = match dep.name.as_str() {
            "npm" | "pnpm" | "yarn" => {
                let spec = dep.clone();
                let result = self.npm.resolve_batch(&[spec], target).await;
                return result
                    .into_iter()
                    .next()
                    .ok_or_else(|| error(&dep.name, "empty npm response"))?
                    .1;
            }
            "gradle" => {
                let json = self.json(&self.endpoints.gradle, "gradle").await?;
                let rows = json
                    .as_array()
                    .ok_or_else(|| error("gradle", "invalid distribution metadata"))?;
                let all: Vec<_> = rows
                    .iter()
                    .filter(|r| r.get("snapshot").and_then(Value::as_bool) != Some(true))
                    .filter_map(|r| r.get("version").and_then(Value::as_str).map(str::to_owned))
                    .collect();
                let newest = rows
                    .iter()
                    .filter(|r| r.get("snapshot").and_then(Value::as_bool) != Some(true))
                    .max_by_key(|r| r.get("buildTime").and_then(Value::as_str).unwrap_or(""))
                    .and_then(|r| r.get("version"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                (all, newest)
            }
            "node" => {
                let json = self.json(&self.endpoints.node, "node").await?;
                let rows = json
                    .as_array()
                    .ok_or_else(|| error("node", "invalid release index"))?;
                let lts = dep.current_req.starts_with("lts") || dep.current_req == "--lts";
                let all = rows
                    .iter()
                    .filter(|r| !lts || r.get("lts").is_some_and(|v| v != &Value::Bool(false)))
                    .filter(|r| {
                        dep.current_req
                            .strip_prefix("lts/")
                            .filter(|s| *s != "*")
                            .is_none_or(|channel| {
                                r.get("lts")
                                    .and_then(Value::as_str)
                                    .is_some_and(|name| name.eq_ignore_ascii_case(channel))
                            })
                    })
                    .filter_map(|r| {
                        r.get("version")
                            .and_then(Value::as_str)
                            .map(|v| v.trim_start_matches('v').to_owned())
                    })
                    .collect();
                let newest = rows
                    .iter()
                    .max_by_key(|r| r.get("date").and_then(Value::as_str).unwrap_or(""))
                    .and_then(|r| r.get("version"))
                    .and_then(Value::as_str)
                    .map(|v| v.trim_start_matches('v').to_owned());
                (all, newest)
            }
            "rust" => {
                let channel = if matches!(dep.current_req.as_str(), "beta" | "nightly") {
                    dep.current_req.as_str()
                } else {
                    "stable"
                };
                let url = self
                    .endpoints
                    .rust
                    .replace("channel-rust-stable", &format!("channel-rust-{channel}"));
                let text = self.text(&url, "rust").await?;
                let doc = text
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|e| error("rust", e.to_string()))?;
                let version = doc["pkg"]["rust"]["version"]
                    .as_str()
                    .and_then(|v| v.split_whitespace().next())
                    .ok_or_else(|| error("rust", "missing stable Rust version"))?;
                if entry.reason.is_some() {
                    return Ok(ResolvedVersion {
                        latest: Some(version.to_owned()),
                        selected: None,
                    });
                }
                if matches!(target, TargetLevel::Minor | TargetLevel::Patch) {
                    let json = self
                        .json(
                            &format!(
                                "{}/repos/rust-lang/rust/releases?per_page=100",
                                self.endpoints.github
                            ),
                            "rust",
                        )
                        .await?;
                    (release_tags(&json, ""), None)
                } else {
                    (vec![version.to_owned()], Some(version.to_owned()))
                }
            }
            "bun" => {
                let json = self
                    .json(
                        &format!(
                            "{}/repos/oven-sh/bun/releases?per_page=100",
                            self.endpoints.github
                        ),
                        "bun",
                    )
                    .await?;
                let newest = json
                    .as_array()
                    .and_then(|rows| {
                        rows.iter()
                            .filter(|r| r.get("draft") != Some(&Value::Bool(true)))
                            .max_by_key(|r| {
                                r.get("published_at").and_then(Value::as_str).unwrap_or("")
                            })
                    })
                    .and_then(|r| r.get("tag_name"))
                    .and_then(Value::as_str)
                    .map(|v| v.trim_start_matches("bun-v").to_owned());
                (release_tags(&json, "bun-v"), newest)
            }
            "android.compileSdk" | "android.targetSdk" => {
                let text = self.text(&self.endpoints.android, "android-sdk").await?;
                validate_xml(&text, "android-sdk")?;
                // SDK repository platform packages; preview packages carry a suffix and do not match.
                let regex=regex::Regex::new(r#"<remotePackage\b[^>]*path="platforms;android-([0-9]+)"[^>]*>([\s\S]*?)</remotePackage>"#).unwrap();
                let all = regex
                    .captures_iter(&text)
                    .filter(|c| {
                        !c[2].contains("<preview>") && !c[2].contains("<obsolete>true</obsolete>")
                    })
                    .map(|c| c[1].to_owned())
                    .collect();
                (all, None)
            }
            "jdk" => {
                let json = self.json(&self.endpoints.jdk, "jdk").await?;
                let all = json
                    .get("versions")
                    .and_then(Value::as_array)
                    .ok_or_else(|| error("jdk", "missing release index"))?
                    .iter()
                    .filter_map(|v| v.get("semver").and_then(Value::as_str).map(str::to_owned))
                    .collect();
                (all, None)
            }
            name => return Err(error(name, "unsupported development tool")),
        };
        let mut result = choose(&versions, &dep.current_req, target, newest.as_deref())?;
        if entry.reason.is_some() {
            result.selected = None;
        }
        // Short tool pins intentionally track a release line. Keep their precision.
        if dep.section == Section::Toolchain && dep.name != "gradle" {
            let precision = dep.current_req.trim_start_matches('v').split('.').count();
            if let Some(selected) = &mut result.selected {
                *selected = selected
                    .split('.')
                    .take(precision)
                    .collect::<Vec<_>>()
                    .join(".");
            }
        }
        Ok(result)
    }

    async fn maven(&self, entry: &Entry, target: TargetLevel) -> Result<ResolvedVersion, DcuError> {
        choose(
            &self.maven_versions(entry).await?,
            &entry.dep.current_req,
            target,
            None,
        )
    }

    async fn maven_versions(&self, entry: &Entry) -> Result<Vec<String>, DcuError> {
        let name = &entry.dep.name;
        let coordinate = if entry.dep.section == Section::GradlePlugin {
            match name.as_str() {
                "com.android.application" | "com.android.library" => {
                    "com.android.tools.build:gradle".to_owned()
                }
                "org.jetbrains.kotlin.android"
                | "org.jetbrains.kotlin.jvm"
                | "org.jetbrains.kotlin.multiplatform" => {
                    "org.jetbrains.kotlin:kotlin-gradle-plugin".to_owned()
                }
                _ => format!("{name}:{name}.gradle.plugin"),
            }
        } else {
            name.clone()
        };
        let (group, artifact) = coordinate
            .split_once(':')
            .ok_or_else(|| error(name, "invalid Maven coordinate"))?;
        if !group
            .bytes()
            .chain(artifact.bytes())
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(error(name, "unsupported Maven coordinate"));
        }
        if entry.repositories.is_empty() {
            return Err(error(name, "no statically declared supported repository"));
        }
        let mut failures = Vec::new();
        let mut not_found = Vec::new();
        let mut versions = Vec::new();
        for repo in &entry.repositories {
            let normalized = crate::maven_access::normalize(repo).ok();
            let configured = normalized
                .as_ref()
                .and_then(|r| self.private_repositories.get(r));
            if !public_repository(repo) && configured.is_none() {
                failures.push("private or unsupported repository: authorize a literal endpoint with --maven-config".into());
                continue;
            }
            let url = format!(
                "{}/{}/{}/maven-metadata.xml",
                repo.trim_end_matches('/'),
                group.replace('.', "/"),
                artifact
            );
            let text = if let Some(headers) = configured {
                let bytes = self
                    .private_cache
                    .as_ref()
                    .expect("configured private cache")
                    .get(&url, headers.clone(), MetadataCache::METADATA_LIMIT, name)
                    .await;
                bytes.and_then(|b| {
                    String::from_utf8(b.to_vec())
                        .map_err(|_| error(name, "non-UTF-8 Maven metadata"))
                })
            } else {
                self.text(&url, name).await
            };
            match text.and_then(|text| xml_versions(&text, name)) {
                Ok(v) => versions.extend(v),
                Err(e) if matches!(&e,DcuError::RegistryLookup {detail,..} if detail.starts_with("HTTP 404 ") || detail=="HTTP 404") =>
                {
                    not_found.push(e.to_string());
                }
                Err(e) => failures.push(e.to_string()),
            }
        }
        if versions.is_empty() {
            failures.extend(not_found);
            return Err(error(name, failures.join("; ")));
        }
        // An inaccessible declared repository could contain a newer release. Do
        // not call a partial result "latest" or auto-apply it. 404 is normal
        // when searching multiple public repositories for one artifact.
        if !failures.is_empty() {
            return Err(error(name, failures.join("; ")));
        }
        Ok(versions)
    }

    /// Published candidates for the bounded coupled-tool search. Cache reuses
    /// the same authenticated metadata as normal selection; never invent pins.
    pub async fn candidates(
        &self,
        entry: &Entry,
        target: TargetLevel,
        selected: &str,
    ) -> Result<Vec<String>, DcuError> {
        let versions = if matches!(entry.dep.section, Section::Maven | Section::GradlePlugin) {
            self.maven_versions(entry).await?
        } else {
            match entry.dep.name.as_str() {
                "gradle" => self
                    .json(&self.endpoints.gradle, "gradle")
                    .await?
                    .as_array()
                    .ok_or_else(|| error("gradle", "invalid distribution metadata"))?
                    .iter()
                    .filter(|r| r.get("snapshot") != Some(&Value::Bool(true)))
                    .filter_map(|r| r["version"].as_str().map(str::to_owned))
                    .collect(),
                "jdk" => self.json(&self.endpoints.jdk, "jdk").await?["versions"]
                    .as_array()
                    .ok_or_else(|| error("jdk", "invalid release index"))?
                    .iter()
                    .filter_map(|r| r["semver"].as_str().map(str::to_owned))
                    .collect(),
                "android.compileSdk" | "android.targetSdk" => {
                    let text = self.text(&self.endpoints.android, "android SDK").await?;
                    let regex = regex::Regex::new(r#"<remotePackage\b[^>]*path="platforms;android-([0-9]+)"[^>]*>([\s\S]*?)</remotePackage>"#).unwrap();
                    regex
                        .captures_iter(&text)
                        .filter(|c| {
                            !c[2].contains("<preview>")
                                && !c[2].contains("<obsolete>true</obsolete>")
                        })
                        .map(|c| c[1].to_owned())
                        .collect()
                }
                _ => return Err(error(&entry.dep.name, "not a coupled tool")),
            }
        };
        let parse = |v: &str| {
            semver::Version::parse(&pad_to_three_segments(v.trim_start_matches('v'))).ok()
        };
        let current = parse(&entry.dep.current_req)
            .ok_or_else(|| error(&entry.dep.name, "non-numeric pin"))?;
        let upper =
            parse(selected).ok_or_else(|| error(&entry.dep.name, "non-numeric selection"))?;
        let mut versions: Vec<_> = versions
            .into_iter()
            .filter_map(|v| {
                let parsed = parse(&v)?;
                let written =
                    if entry.dep.section == Section::Toolchain && entry.dep.name != "gradle" {
                        parse(
                            &v.split('.')
                                .take(
                                    entry
                                        .dep
                                        .current_req
                                        .trim_start_matches('v')
                                        .split('.')
                                        .count(),
                                )
                                .collect::<Vec<_>>()
                                .join("."),
                        )?
                    } else {
                        parsed.clone()
                    };
                (parsed.pre.is_empty()
                    && parsed >= current
                    && written <= upper
                    && match target {
                        TargetLevel::Minor => parsed.major == current.major,
                        TargetLevel::Patch => {
                            parsed.major == current.major && parsed.minor == current.minor
                        }
                        _ => true,
                    })
                .then_some((parsed, v))
            })
            .collect();
        versions.push((
            current,
            entry.dep.current_req.trim_start_matches('v').into(),
        ));
        versions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        versions.dedup_by(|a, b| a.0 == b.0);
        Ok(versions.into_iter().map(|(_, v)| v).collect())
    }

    /// Fetch all sidecar data before any project files are written.
    #[allow(clippy::too_many_lines)]
    pub async fn sidecars(
        &self,
        doc: &Document,
        updates: &[PlannedUpdate],
    ) -> Result<Vec<Patch>, DcuError> {
        let mut extra = Vec::new();
        if let (Some(span), Some(kind), Some(update)) = (
            &doc.checksum,
            &doc.distribution,
            updates
                .iter()
                .find(|u| u.name == "gradle" && u.section == Section::Toolchain),
        ) {
            let url = format!(
                "{}/gradle-{}-{kind}.zip.sha256",
                self.endpoints.distributions, update.to
            );
            let digest = self.text(&url, "gradle checksum").await?;
            let digest = digest.trim();
            if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(error("gradle", "invalid official SHA-256 response"));
            }
            extra.push(Patch {
                start: span.start,
                end: span.end,
                new_value: digest.to_owned(),
            });
        }
        for entry in &doc.entries {
            let Some((span, algo)) = &entry.integrity else {
                continue;
            };
            let Some(update) = updates.iter().find(|u| {
                u.name == entry.dep.name
                    && u.from == entry.dep.current_req
                    && u.section == entry.dep.section
            }) else {
                continue;
            };
            if !matches!(
                algo.as_str(),
                "sha1" | "sha224" | "sha256" | "sha384" | "sha512"
            ) {
                return Err(error(
                    &update.name,
                    "unsupported packageManager hash algorithm",
                ));
            }
            let modern_yarn = update.name == "yarn"
                && update
                    .to
                    .split('.')
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_some_and(|v| v >= 2);
            let tarball = if modern_yarn {
                format!(
                    "{}/{}/packages/yarnpkg-cli/bin/yarn.js",
                    self.endpoints.yarn_downloads, update.to
                )
            } else {
                let json = self
                    .json(
                        &format!("{}/{}/{}", self.endpoints.npm, update.name, update.to),
                        &update.name,
                    )
                    .await?;
                json["dist"]["tarball"]
                    .as_str()
                    .ok_or_else(|| error(&update.name, "missing npm tarball for integrity"))?
                    .to_owned()
            };
            let trusted = tarball.starts_with("https://registry.npmjs.org/")
                || (modern_yarn && tarball.starts_with("https://repo.yarnpkg.com/"))
                || (cfg!(test)
                    && (tarball.starts_with(&self.endpoints.npm)
                        || tarball.starts_with(&self.endpoints.yarn_downloads)));
            if !trusted {
                return Err(error(&update.name, "untrusted package manager tarball URL"));
            }
            let bytes = self
                .bytes(&tarball, &update.name, MetadataCache::INTEGRITY_LIMIT)
                .await?;
            let digest = match algo.as_str() {
                "sha1" => Sha1::digest(&bytes).to_vec(),
                "sha224" => Sha224::digest(&bytes).to_vec(),
                "sha256" => Sha256::digest(&bytes).to_vec(),
                "sha384" => Sha384::digest(&bytes).to_vec(),
                _ => Sha512::digest(&bytes).to_vec(),
            };
            let hex = digest.iter().fold(String::new(), |mut s, b| {
                write!(s, "{b:02x}").expect("write to String");
                s
            });
            extra.push(Patch {
                start: span.start,
                end: span.end,
                new_value: format!("{algo}.{hex}"),
            });
        }
        Ok(extra)
    }
}

fn public_repository(url: &str) -> bool {
    [
        "https://dl.google.com/dl/android/maven2",
        "https://maven.google.com",
        "https://repo.maven.apache.org/maven2",
        "https://repo1.maven.org/maven2",
        "https://plugins.gradle.org/m2",
    ]
    .iter()
    .any(|base| url.trim_end_matches('/') == *base)
        || (cfg!(test)
            && (url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")))
}

fn release_tags(json: &Value, prefix: &str) -> Vec<String> {
    json.as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| r.get("draft") != Some(&Value::Bool(true)))
                .filter_map(|r| {
                    r.get("tag_name")
                        .and_then(Value::as_str)
                        .map(|v| v.trim_start_matches(prefix).to_owned())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn choose(
    versions: &[String],
    current: &str,
    target: TargetLevel,
    newest: Option<&str>,
) -> Result<ResolvedVersion, DcuError> {
    let mut parsed: Vec<_> = versions
        .iter()
        .filter_map(|v| semver::Version::parse(&pad_to_three_segments(v)).ok())
        .collect();
    parsed.sort_unstable();
    parsed.dedup();
    let latest = highest_stable(&parsed);
    if parsed.is_empty() {
        return Err(error("metadata", "no supported published versions"));
    }
    let cur = semver::Version::parse(&pad_to_three_segments(current.trim_start_matches('v'))).ok();
    let selected = if target == TargetLevel::Newest && newest.is_some() {
        newest.map(str::to_owned)
    } else {
        select_version(cur.as_ref(), &parsed, target, latest.as_deref(), None)
    };
    // Keep real release strings (Gradle 8.9, SDK 36), not padded inventions.
    let restore = |value: Option<String>| {
        value.map(|v| {
            versions
                .iter()
                .find(|original| {
                    semver::Version::parse(&pad_to_three_segments(original)).ok()
                        == semver::Version::parse(&v).ok()
                })
                .cloned()
                .unwrap_or(v)
        })
    };
    Ok(ResolvedVersion {
        latest: restore(latest),
        selected: restore(selected),
    })
}

fn validate_xml(text: &str, name: &str) -> Result<(), DcuError> {
    let mut reader = quick_xml::Reader::from_str(text);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => return Err(error(name, e.to_string())),
            _ => {}
        }
    }
    Ok(())
}

fn xml_versions(text: &str, name: &str) -> Result<Vec<String>, DcuError> {
    validate_xml(text, name)?;
    Ok(regex::Regex::new(r"<version>\s*([^<\s]+)\s*</version>")
        .unwrap()
        .captures_iter(text)
        .map(|c| c[1].to_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    fn entry(name: &str, version: &str, section: Section) -> Entry {
        Entry {
            requested: true,
            dep: dependency_check_updates_core::DependencySpec {
                name: name.into(),
                current_req: version.into(),
                section,
                path_version: None,
            },
            span: None,
            reason: None,
            repositories: Vec::new(),
            integrity: None,
        }
    }

    #[tokio::test]
    async fn yarn_object_tags_invalid_metadata_and_rust_patch_index_are_fixed() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        for (url, body) in [
            ("/yarn", r#"{"tags":{"2.0.0":{},"2.1.0":{}}}"#),
            ("/invalid-yarn", r#"{"tags":null}"#),
            ("/rust", "[pkg.rust]\nversion = '1.88.2 (fixed)'\n"),
            (
                "/repos/rust-lang/rust/releases",
                r#"[{"tag_name":"1.88.1"},{"tag_name":"1.88.2"}]"#,
            ),
        ] {
            Mock::given(path(url))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        let registry = ToolRegistry::with_endpoints(Endpoints {
            yarn: format!("{}/yarn", server.uri()),
            rust: format!("{}/rust", server.uri()),
            github: server.uri(),
            ..Endpoints::default()
        });
        assert_eq!(
            registry
                .resolve(
                    &entry("yarn", "2.0.0", Section::Toolchain),
                    TargetLevel::Latest
                )
                .await
                .unwrap()
                .selected
                .as_deref(),
            Some("2.1.0")
        );
        assert_eq!(
            registry
                .resolve(
                    &entry("rust", "1.88.1", Section::Toolchain),
                    TargetLevel::Patch
                )
                .await
                .unwrap()
                .selected
                .as_deref(),
            Some("1.88.2")
        );
        let invalid = ToolRegistry::with_endpoints(Endpoints {
            yarn: format!("{}/invalid-yarn", server.uri()),
            ..Endpoints::default()
        });
        assert!(
            invalid
                .resolve(
                    &entry("yarn", "2.0", Section::Toolchain),
                    TargetLevel::Latest
                )
                .await
                .is_err()
        );
        assert!(
            registry
                .resolve(
                    &entry("unknown", "1.0", Section::Toolchain),
                    TargetLevel::Latest
                )
                .await
                .is_err()
        );
        assert!(
            registry
                .candidates(
                    &entry("unknown", "1.0", Section::Toolchain),
                    TargetLevel::Latest,
                    "1.0"
                )
                .await
                .is_err()
        );
        assert!(choose(&[], "1.0", TargetLevel::Latest, None).is_err());
        assert!(validate_xml("<root></wrong>", "xml").is_err());
    }

    #[tokio::test]
    async fn maven_coordinate_missing_repository_and_404_are_not_current() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        let registry = ToolRegistry::new();
        let mut dep = entry("group:bad/artifact", "1.0", Section::Maven);
        assert!(
            registry
                .maven(&dep, TargetLevel::Latest)
                .await
                .unwrap_err()
                .to_string()
                .contains("coordinate")
        );
        dep.dep.name = "group:artifact".into();
        assert!(
            registry
                .maven(&dep, TargetLevel::Latest)
                .await
                .unwrap_err()
                .to_string()
                .contains("repository")
        );
        dep.repositories = vec![server.uri()];
        assert!(
            registry
                .maven(&dep, TargetLevel::Latest)
                .await
                .unwrap_err()
                .to_string()
                .contains("404")
        );
    }

    #[tokio::test]
    async fn malformed_checksum_hash_algorithm_and_untrusted_tarball_block_sidecars() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/gradle-9.0-bin.zip.sha256"))
            .respond_with(ResponseTemplate::new(200).set_body_string("invalid"))
            .mount(&server)
            .await;
        Mock::given(path("/pnpm/2.0.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"dist":{"tarball":"https://untrusted.example/pnpm.tgz"}}),
            ))
            .mount(&server)
            .await;
        let registry = ToolRegistry::with_endpoints(Endpoints {
            distributions: server.uri(),
            npm: server.uri(),
            ..Endpoints::default()
        });
        let doc = crate::project::parse("distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\ndistributionSha256Sum=old\n", std::path::Path::new("gradle/wrapper/gradle-wrapper.properties")).unwrap();
        let update = PlannedUpdate {
            name: "gradle".into(),
            from: "8.9".into(),
            to: "9.0".into(),
            section: Section::Toolchain,
        };
        assert!(
            registry
                .sidecars(&doc, &[update])
                .await
                .unwrap_err()
                .to_string()
                .contains("SHA-256")
        );
        for algo in ["unsupported", "sha256"] {
            let text = format!(r#"{{"packageManager":"pnpm@1.0.0+{algo}.old"}}"#);
            let doc = crate::project::parse(&text, std::path::Path::new("package.json")).unwrap();
            let update = PlannedUpdate {
                name: "pnpm".into(),
                from: "1.0.0".into(),
                to: "2.0.0".into(),
                section: Section::Toolchain,
            };
            let error = registry
                .sidecars(&doc, &[update])
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(if algo == "unsupported" {
                "hash algorithm"
            } else {
                "untrusted"
            }));
        }
    }
}

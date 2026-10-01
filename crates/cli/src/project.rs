//! Bounded static Gradle/tool parsing. No Gradle code is executed.
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};

use dependency_check_updates_core::{
    DcuError, DependencySection as Section, DependencySpec, ManifestHandler, ManifestKind,
    ManifestRef, ParsedManifest, Patch, PlannedUpdate, apply_byte_patches,
};
use regex::Regex;

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub requested: bool,
    pub dep: DependencySpec,
    pub span: Option<Range<usize>>,
    pub reason: Option<String>,
    pub repositories: Vec<String>,
    pub integrity: Option<(Range<usize>, String)>,
}

#[derive(Clone, Debug)]
struct Definition {
    key: String,
    value: String,
    span: Range<usize>,
}

#[derive(Clone, Debug)]
struct Reference {
    name: String,
    key: String,
    section: Section,
}

#[derive(Clone, Debug)]
struct CatalogAlias {
    accessor: String,
    name: String,
    section: Section,
    span: Option<Range<usize>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Document {
    pub path: PathBuf,
    pub text: String,
    pub entries: Vec<Entry>,
    pub checksum: Option<Range<usize>>,
    pub distribution: Option<String>,
    pub applied_plugins: Vec<String>,
    pub context_issues: Vec<String>,
    pub resolved_uses: Vec<(PathBuf, DependencySpec)>,
    property_bindings: Vec<(String, String)>,
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    repositories: Vec<String>,
    catalog_aliases: Vec<CatalogAlias>,
    catalog_uses: Vec<(String, Section, bool)>,
}

fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static parser regex")
}

/// Find the end of a quoted region without interpreting Gradle expressions.
/// Triple-quoted prose is opaque, including its embedded quotes and comments.
fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let triple = bytes.get(start..start + 3) == Some(&[quote; 3]);
    let width = if triple { 3 } else { 1 };
    let mut i = start + width;
    while i < bytes.len() {
        if bytes
            .get(i..i + width)
            .is_some_and(|s| s.iter().all(|b| *b == quote))
        {
            return i + width;
        }
        i = if !triple && bytes[i] == b'\\' {
            (i + 2).min(bytes.len())
        } else {
            i + 1
        };
    }
    bytes.len()
}

pub(crate) fn error(name: &str, detail: impl Into<String>) -> DcuError {
    DcuError::RegistryLookup {
        package: name.to_owned(),
        detail: detail.into(),
    }
}

/// Mask comments without changing byte positions or touching quoted strings.
fn uncomment(text: &str, slash: bool) -> String {
    let mut b = text.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if matches!(b[i], b'\'' | b'"') {
            i = quoted_end(&b, i);
        } else if slash && i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            b[i] = b' ';
            b[i + 1] = b' ';
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                    depth += 1;
                    b[i] = b' ';
                    b[i + 1] = b' ';
                    i += 2;
                } else if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                    depth -= 1;
                    b[i] = b' ';
                    b[i + 1] = b' ';
                    i += 2;
                } else {
                    if !matches!(b[i], b'\r' | b'\n') {
                        b[i] = b' ';
                    }
                    i += 1;
                }
            }
        } else if (!slash && matches!(b[i], b'#' | b'!'))
            || (slash && i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/')
        {
            while i < b.len() && b[i] != b'\n' {
                if b[i] != b'\r' {
                    b[i] = b' ';
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(b).expect("comment masking preserves UTF-8")
}

fn numeric(value: &str) -> bool {
    let v = value.strip_prefix('v').unwrap_or(value);
    semver::Version::parse(&dependency_check_updates_core::pad_to_three_segments(v)).is_ok()
}

impl Document {
    fn entry(&mut self, name: &str, value: &str, span: Option<Range<usize>>, section: Section) {
        let (value, span) = if name == "jdk" && value.starts_with("temurin-") {
            let offset = "temurin-".len();
            (&value[offset..], span.map(|s| s.start + offset..s.end))
        } else {
            (value, span)
        };
        self.entries.push(Entry {
            requested: true,
            dep: DependencySpec {
                name: name.to_owned(),
                current_req: value.to_owned(),
                section,
                path_version: None,
            },
            reason: (!numeric(value) || span.is_none()).then(|| {
                if matches!(
                    value,
                    "stable" | "lts" | "lts/*" | "--lts" | "node" | "latest" | "beta" | "nightly"
                ) || value.starts_with("lts/")
                {
                    "channel preserved; moving channel is not a project pin".to_owned()
                } else {
                    "unsupported dynamic expression or support range; preserved".to_owned()
                }
            }),
            span,
            repositories: Vec::new(),
            integrity: None,
        });
    }

    pub fn dependencies(&self) -> Vec<DependencySpec> {
        self.entries
            .iter()
            .filter(|e| e.requested)
            .map(|e| e.dep.clone())
            .collect()
    }

    pub fn apply(
        &self,
        text: &str,
        updates: &[PlannedUpdate],
        extra: Vec<Patch>,
    ) -> Result<String, DcuError> {
        let mut patches: BTreeMap<(usize, usize), String> = BTreeMap::new();
        for entry in &self.entries {
            for u in updates.iter().filter(|u| {
                u.name == entry.dep.name
                    && u.section == entry.dep.section
                    && u.from == entry.dep.current_req
            }) {
                if entry.reason.is_some() {
                    continue;
                }
                if let Some(span) = &entry.span {
                    let key = (span.start, span.end);
                    if let Some(previous) = patches.insert(key, u.to.clone())
                        && previous != u.to
                    {
                        return Err(error(
                            &u.name,
                            "shared version reference has conflicting updates",
                        ));
                    }
                }
            }
        }
        for p in extra {
            patches.insert((p.start, p.end), p.new_value);
        }
        let patches: Vec<_> = patches
            .into_iter()
            .map(|((start, end), new_value)| Patch {
                start,
                end,
                new_value,
            })
            .collect();
        apply_byte_patches(text, &patches).map_err(|e| error("patch", e.to_string()))
    }
}

pub(crate) struct ProjectHandler(pub Document);
impl ManifestHandler for ProjectHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        let document = if self.0.text == text && self.0.path == path {
            self.0.clone()
        } else {
            parse(text, path)?
        };
        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_owned(),
                kind: ManifestKind::from_path(path)
                    .ok_or_else(|| error("manifest", "unknown file"))?,
            },
            dependencies: document.dependencies(),
        })
    }
    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        self.0.apply(text, updates, Vec::new())
    }
}

pub(crate) fn parse(text: &str, path: &Path) -> Result<Document, DcuError> {
    let mut doc = Document {
        path: path.to_owned(),
        text: text.to_owned(),
        entries: Vec::new(),
        definitions: Vec::new(),
        references: Vec::new(),
        repositories: Vec::new(),
        checksum: None,
        distribution: None,
        applied_plugins: Vec::new(),
        context_issues: Vec::new(),
        resolved_uses: Vec::new(),
        property_bindings: Vec::new(),
        catalog_aliases: Vec::new(),
        catalog_uses: Vec::new(),
    };
    match ManifestKind::from_path(path) {
        Some(ManifestKind::Gradle) => parse_gradle(&mut doc),
        Some(ManifestKind::GradleCatalog) => parse_catalog(&mut doc)?,
        Some(ManifestKind::GradleProperties) => parse_properties(&mut doc),
        Some(ManifestKind::GradleWrapper) => parse_wrapper(&mut doc),
        Some(ManifestKind::ToolVersions | ManifestKind::PackageJson) => parse_tools(&mut doc)?,
        _ => {}
    }
    Ok(doc)
}

fn outside_strings(text: &str) -> Vec<bool> {
    let bytes = text.as_bytes();
    let mut outside = vec![true; bytes.len()];
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'\'' | b'"') {
            let start = i;
            i = quoted_end(bytes, i);
            outside[start..i].fill(false);
        } else {
            i += 1;
        }
    }
    outside
}

fn json_depth(text: &str) -> Vec<usize> {
    let outside = outside_strings(text);
    let mut depth = 0usize;
    text.bytes()
        .enumerate()
        .map(|(i, b)| {
            let at = depth;
            if outside[i] {
                match b {
                    b'{' => depth += 1,
                    b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            at
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn parse_gradle(doc: &mut Document) {
    let clean = uncomment(&doc.text, true);
    let outside = outside_strings(&clean);
    if rx(r"\bincludeBuild\s*\(|\bapply\s*(?:\(\s*from\s*=|from\s*:)")
        .find_iter(&clean)
        .any(|m| outside[m.start()])
    {
        let reason = "included builds and imported convention scripts are not statically resolved";
        doc.context_issues.push(reason.into());
        doc.entry("gradle.build-connection", reason, None, Section::Maven);
    }
    if rx(r"\b(?:exclusiveContent|includeGroup|excludeGroup|includeModule|excludeModule)\b")
        .find_iter(&clean)
        .any(|m| outside[m.start()])
    {
        doc.repositories
            .push("unsupported Gradle repository content filters".into());
    }
    for c in rx(r"\bmaven\s*\{([^}]*)\}").captures_iter(&clean) {
        if !outside[c.get(0).unwrap().start()] {
            continue;
        }
        if !rx(r#"\burl\s*(?:=|\()?\s*(?:uri\s*\()?\s*["']([^"']+)["']"#).is_match(&c[1]) {
            doc.repositories
                .push("unsupported dynamic Gradle Maven repository URL".into());
        }
    }
    for c in rx(r"\bmaven\s*\(\s*([A-Za-z_]\w*)").captures_iter(&clean) {
        if outside[c.get(0).unwrap().start()] {
            doc.repositories
                .push("unsupported dynamic Gradle Maven repository URL".into());
        }
    }
    for c in rx(r#"\b(?:id\s*(?:\(\s*)?|apply\s*\(\s*plugin\s*=\s*|apply\s+plugin\s*:\s*)["'](org\.jetbrains\.kotlin\.android|org\.jetbrains\.kotlin\.multiplatform|kotlin-android)["']([^\r\n]*)"#).captures_iter(&clean) {
        if outside[c.get(0).unwrap().start()] && !c[2].contains("apply false") {doc.applied_plugins.push(c[1].to_owned());}
    }
    for (method, url) in [
        ("google", "https://dl.google.com/dl/android/maven2"),
        ("mavenCentral", "https://repo.maven.apache.org/maven2"),
        ("gradlePluginPortal", "https://plugins.gradle.org/m2"),
    ] {
        if rx(&format!(r"\b{method}\s*\(\s*\)"))
            .find_iter(&clean)
            .any(|m| outside[m.start()])
        {
            doc.repositories.push(url.to_owned());
        }
    }
    for c in rx(r#"\b(?:url\s*(?:=|\()?\s*(?:uri\s*\()?|maven\s*\()\s*["']([^"']+)["']"#)
        .captures_iter(&clean)
    {
        if !outside[c.get(0).unwrap().start()] {
            continue;
        }
        doc.repositories.push(c[1].to_owned());
    }
    for c in
        rx(r#"(?m)^\s*(?:(?:val|var|def)\s+|ext\.)?([A-Za-z_]\w*)\s*(?::\s*String)?\s*=\s*["']([^"']+)["']\s*;?\s*$"#)
            .captures_iter(&clean)
    {
        if !outside[c.get(0).unwrap().start()] {
            continue;
        }
        let m = c.get(2).unwrap();
        doc.definitions.push(Definition {
            key: c[1].to_owned(),
            value: m.as_str().to_owned(),
            span: m.range(),
        });
    }
    for c in rx(r#"(?m)^\s*val\s+([A-Za-z_]\w*)\s*(?::\s*String)?\s+by\s+extra\s*\(\s*["']([^"']+)["']\s*\)\s*;?\s*$"#).captures_iter(&clean) {
        if outside[c.get(0).unwrap().start()] {
            let m = c.get(2).unwrap();
            doc.definitions.push(Definition { key: c[1].to_owned(), value: m.as_str().to_owned(), span: m.range() });
        }
    }
    // Only literal project-property bindings. No environment access, function
    // evaluation, defaults or provider transformations are interpreted.
    for c in rx(r#"(?m)^\s*(?:val|def)\s+([A-Za-z_]\w*)\s*(?::\s*(?:String|Int))?\s*=\s*(?:providers\.gradleProperty\(\s*["']([^"']+)["']\s*\)\.get\(\)|(?:project\.)?(?:property|findProperty)\(\s*["']([^"']+)["']\s*\))\s*(?:\.toInt\(\)|as\s+String)?\s*;?\s*$"#).captures_iter(&clean) {
        if outside[c.get(0).unwrap().start()] {
            let key = c.get(2).or_else(|| c.get(3)).unwrap().as_str();
            doc.property_bindings.push((c[1].to_owned(), key.to_owned()));
        }
    }
    for c in rx(r#"\b(?:extra|ext)\s*\[\s*["']([^"']+)["']\s*\]\s*=\s*["']([^"']+)["']"#)
        .captures_iter(&clean)
    {
        if !outside[c.get(0).unwrap().start()] {
            continue;
        }
        let m = c.get(2).unwrap();
        doc.definitions.push(Definition {
            key: c[1].to_owned(),
            value: m.as_str().to_owned(),
            span: m.range(),
        });
    }
    for c in rx(r"(?m)^\s*(?:val|var|def)\s+([A-Za-z_]\w*)\s*(?::\s*Int)?\s*=\s*([0-9]+)\s*;?\s*$")
        .captures_iter(&clean)
    {
        if !outside[c.get(0).unwrap().start()] {
            continue;
        }
        let m = c.get(2).unwrap();
        doc.definitions.push(Definition {
            key: c[1].to_owned(),
            value: m.as_str().to_owned(),
            span: m.range(),
        });
    }
    // Literal Maven notation in dependency calls, not arbitrary quoted prose.
    let mut recognized = Vec::new();
    for c in rx(r"(?m)\b(?:classpath|\w*[Ii]mplementation|implementation|api|compileOnly|runtimeOnly|annotationProcessor|kapt|ksp)\s*(?:\(\s*)?libs\.([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s*\)?\s*;?\s*$").captures_iter(&clean) {
        if outside[c.get(0).unwrap().start()] {
            recognized.push(c.get(0).unwrap().start());
            doc.catalog_uses.push((c[1].to_owned(), Section::Maven, false));
        }
    }
    for c in rx(r"\balias\s*\(\s*libs\.plugins\.([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s*\)([^\r\n]*)")
        .captures_iter(&clean)
    {
        if outside[c.get(0).unwrap().start()] {
            doc.catalog_uses.push((
                c[1].to_owned(),
                Section::GradlePlugin,
                !c[2].contains("apply false"),
            ));
        }
    }
    for c in rx(r#"\b(?:classpath|\w*[Ii]mplementation|implementation|api|\w*[Cc]ompileOnly|compileOnly|\w*[Rr]untimeOnly|runtimeOnly|annotationProcessor|kapt|ksp)\s*(?:\(\s*)?["']([^"'\r\n]+)["']"#).captures_iter(&clean) {
        if !outside[c.get(0).unwrap().start()] { continue; }
        recognized.push(c.get(0).unwrap().start());
        let m=c.get(1).unwrap();
        let parts:Vec<_>=m.as_str().split(':').collect();
        if parts.len()!=3 { doc.entry(m.as_str(),m.as_str(),None,Section::Maven); continue; }
        let name=format!("{}:{}",parts[0],parts[1]); let version=parts[2];
        let tail=clean[c.get(0).unwrap().end()..].trim_start();
        if tail.starts_with('+') || tail.starts_with(".to") { doc.entry(&name,"dynamic concatenation",None,Section::Maven); continue; }
        if let Some(key)=reference_key(version) { doc.references.push(Reference{name,key,section:Section::Maven}); }
        else { doc.entry(&name,version,Some(m.end()-version.len()..m.end()),Section::Maven); }
    }
    for c in rx(r"\b(classpath|\w*[Ii]mplementation|implementation|api|compileOnly|runtimeOnly|annotationProcessor|kapt|ksp)\s*\(\s*([^\r\n]+)").captures_iter(&clean) {
        if !outside[c.get(0).unwrap().start()] || recognized.contains(&c.get(0).unwrap().start()) {continue;}
        doc.entry(&format!("gradle.{}",&c[1]),c[2].trim(),None,Section::Maven);
    }
    for c in rx(r#"\bid\s*(?:\(\s*)?["']([^"']+)["']\s*\)?\s*version\s*(?:\(\s*)?(?:["']([^"']+)["']|([A-Za-z_]\w*))"#).captures_iter(&clean) {
        if !outside[c.get(0).unwrap().start()] { continue; }
        let tail=clean[c.get(0).unwrap().end()..].trim_start();
        if tail.starts_with('+') {doc.entry(&c[1],"dynamic plugin version",None,Section::GradlePlugin);continue;}
        if let Some(m)=c.get(2) {
            if let Some(key)=reference_key(m.as_str()) {doc.references.push(Reference{name:c[1].to_owned(),key,section:Section::GradlePlugin});}
            else {doc.entry(&c[1],m.as_str(),Some(m.range()),Section::GradlePlugin);}
        } else {doc.references.push(Reference{name:c[1].to_owned(),key:c[3].to_owned(),section:Section::GradlePlugin});}
    }
    for c in rx(r"\b(compileSdkVersion|targetSdkVersion|compileSdk|targetSdk)\b\s*(?:=|\()?\s*([A-Za-z_][\w.]*|[0-9]+)\b([^\r\n]*)").captures_iter(&clean) {
        if !outside[c.get(0).unwrap().start()] { continue; }
        let m=c.get(2).unwrap(); let name=if c[1].starts_with("compile") {"android.compileSdk"} else {"android.targetSdk"};
        if c[3].trim_start().starts_with(['+', '-', '*', '/','.']) {doc.entry(name,"dynamic SDK expression",None,Section::AndroidSdk);}
        else if m.as_str().bytes().all(|b| b.is_ascii_digit()) {doc.entry(name,m.as_str(),Some(m.range()),Section::AndroidSdk);}
        else {doc.references.push(Reference{name:name.to_owned(),key:m.as_str().to_owned(),section:Section::AndroidSdk});}
    }
    // Track a shared minSdk source as a protected consumer. It has no lookup
    // or update target, so shared-source validation preserves the device range.
    for c in
        rx(r"\b(?:minSdkVersion|minSdk)\b\s*(?:=|\()?\s*([A-Za-z_]\w*)\b").captures_iter(&clean)
    {
        if outside[c.get(0).unwrap().start()] {
            doc.references.push(Reference {
                name: "android.minSdk".into(),
                key: c[1].to_owned(),
                section: Section::AndroidSdk,
            });
        }
    }
}

fn reference_key(value: &str) -> Option<String> {
    let key = value
        .strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))?;
    rx(r"^[A-Za-z_]\w*$").is_match(key).then(|| key.to_owned())
}

fn parse_properties(doc: &mut Document) {
    let clean = uncomment(&doc.text, false);
    for c in rx(r"(?m)^\s*([\w.]+)\s*[=:]\s*([^\s]+)\s*$").captures_iter(&clean) {
        let m = c.get(2).unwrap();
        doc.definitions.push(Definition {
            key: c[1].to_owned(),
            value: m.as_str().to_owned(),
            span: m.range(),
        });
    }
}

fn parse_wrapper(doc: &mut Document) {
    let clean = uncomment(&doc.text, false);
    if let Some(c)=rx(r"(?m)^\s*distributionUrl\s*=\s*(https(?:\\)?:\/\/(?:services|downloads)\.gradle\.org/distributions/gradle-([^\s/]+)-(bin|all)\.zip)\s*$").captures(&clean) {
        let m=c.get(2).unwrap();doc.entry("gradle",m.as_str(),Some(m.range()),Section::Toolchain);doc.distribution=Some(c[3].to_owned());
    } else if clean.contains("distributionUrl") {doc.entry("gradle","custom distribution URL",None,Section::Toolchain);}
    if let Some(c) = rx(r"(?m)^\s*distributionSha256Sum\s*=\s*([^\s]+)").captures(&clean) {
        doc.checksum = Some(c.get(1).unwrap().range());
    }
}

fn toml_string_span(text: &str, span: Range<usize>, value: &str) -> Option<Range<usize>> {
    let raw = text.get(span.clone())?;
    let quotes = if raw.starts_with("\"\"\"") || raw.starts_with("'''") {
        3
    } else {
        1
    };
    let content = span.start + quotes..span.end.checked_sub(quotes)?;
    (text.get(content.clone())? == value).then_some(content)
}

#[allow(clippy::too_many_lines)]
fn parse_catalog(doc: &mut Document) -> Result<(), DcuError> {
    let parsed = toml_edit::Document::parse(doc.text.clone())
        .map_err(|e| error("version catalog", e.to_string()))?;
    if let Some(versions) = parsed
        .get("versions")
        .and_then(toml_edit::Item::as_table_like)
    {
        for (key, item) in versions.iter() {
            if let (Some(value), Some(span)) = (item.as_str(), item.span()) {
                let Some(span) = toml_string_span(&doc.text, span, value) else {
                    continue;
                };
                doc.definitions.push(Definition {
                    key: format!("catalog:{key}"),
                    value: value.to_owned(),
                    span,
                });
            }
        }
    }
    for (table, section) in [
        ("libraries", Section::Maven),
        ("plugins", Section::GradlePlugin),
    ] {
        if let Some(items) = parsed.get(table).and_then(toml_edit::Item::as_table_like) {
            for (alias, item) in items.iter() {
                if let Some(literal) = item.as_str() {
                    if let Some((name, version)) = literal.rsplit_once(':')
                        && let Some(span) = item.span()
                    {
                        let version_span = toml_string_span(&doc.text, span, literal)
                            .map(|s| s.end - version.len()..s.end);
                        doc.catalog_aliases.push(CatalogAlias {
                            accessor: alias.replace(['-', '_'], "."),
                            name: name.to_owned(),
                            section,
                            span: version_span.clone(),
                        });
                        doc.entry(name, version, version_span, section);
                    }
                    continue;
                }
                let name = if section == Section::GradlePlugin {
                    item.get("id")
                        .and_then(toml_edit::Item::as_str)
                        .map(str::to_owned)
                } else {
                    item.get("module")
                        .and_then(toml_edit::Item::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            Some(format!(
                                "{}:{}",
                                item.get("group")?.as_str()?,
                                item.get("name")?.as_str()?
                            ))
                        })
                };
                let Some(name) = name else {
                    doc.entry(alias, "unsupported catalog notation", None, section);
                    continue;
                };
                let version_span = item.get("version").and_then(|version| {
                    if let Some(value) = version.as_str() {
                        toml_string_span(&doc.text, version.span()?, value)
                    } else {
                        let key = format!("catalog:{}", version.get("ref")?.as_str()?);
                        doc.definitions
                            .iter()
                            .find(|d| d.key == key)
                            .map(|d| d.span.clone())
                    }
                });
                doc.catalog_aliases.push(CatalogAlias {
                    accessor: alias.replace(['-', '_'], "."),
                    name: name.clone(),
                    section,
                    span: version_span,
                });
                if let Some(version) = item.get("version") {
                    if let Some(value) = version.as_str() {
                        if let Some(span) = version.span() {
                            doc.entry(
                                &name,
                                value,
                                toml_string_span(&doc.text, span, value),
                                section,
                            );
                        }
                    } else if let Some(key) = version.get("ref").and_then(toml_edit::Item::as_str) {
                        doc.references.push(Reference {
                            name,
                            key: format!("catalog:{key}"),
                            section,
                        });
                    } else {
                        doc.entry(&name, "rich version constraint", None, section);
                    }
                } else {
                    doc.entry(&name, "no static version", None, section);
                }
            }
        }
    }
    Ok(())
}

fn tool_name(name: &str) -> Option<&'static str> {
    match name {
        "node" | "nodejs" => Some("node"),
        "rust" => Some("rust"),
        "npm" => Some("npm"),
        "pnpm" => Some("pnpm"),
        "yarn" => Some("yarn"),
        "bun" => Some("bun"),
        "java" | "jdk" => Some("jdk"),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn parse_tools(doc: &mut Document) -> Result<(), DcuError> {
    let filename = doc.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let clean = uncomment(&doc.text, false);
    match filename {
        "package.json" => {
            let json: serde_json::Value = serde_json::from_str(&doc.text)
                .map_err(|e| error("package.json", e.to_string()))?;
            if let Some(manager) = json
                .get("packageManager")
                .and_then(serde_json::Value::as_str)
                && let Some((name, value)) = manager.split_once('@')
            {
                if matches!(name, "npm" | "pnpm" | "yarn" | "bun") {
                    // Exact JSON key and value; never an unrelated string containing packageManager.
                    let depth = json_depth(&clean);
                    let pattern = rx(r#""packageManager"\s*:\s*"([^"\\]*)""#);
                    let captures: Vec<_> = pattern
                        .captures_iter(&clean)
                        .filter(|c| depth[c.get(0).unwrap().start()] == 1 && &c[1] == manager)
                        .collect();
                    if captures.len() == 1 {
                        let c = &captures[0];
                        let m = c.get(1).unwrap();
                        let (version, hash) = value
                            .split_once('+')
                            .map_or((value, None), |(v, h)| (v, Some(h)));
                        let start = m.start() + name.len() + 1;
                        doc.entry(
                            name,
                            version,
                            Some(start..start + version.len()),
                            Section::Toolchain,
                        );
                        if let Some(hash) = hash {
                            if let Some((algo, _)) = hash.split_once('.') {
                                doc.entries.last_mut().unwrap().integrity =
                                    Some((start + version.len() + 1..m.end(), algo.to_owned()));
                                if name == "bun" {
                                    doc.entries.last_mut().unwrap().reason=Some("unsupported Bun packageManager integrity semantics; preserved".to_owned());
                                }
                            } else {
                                doc.entries.last_mut().unwrap().reason =
                                    Some("unsupported packageManager integrity format".to_owned());
                            }
                        }
                    } else {
                        doc.entry(name, value, None, Section::Toolchain);
                        doc.entries.last_mut().unwrap().reason = Some(
                            "escaped or ambiguous packageManager JSON declaration; preserved"
                                .to_owned(),
                        );
                    }
                } else {
                    doc.entry(name, value, None, Section::Toolchain);
                }
            }
        }
        ".nvmrc" | ".node-version" | "rust-toolchain" => {
            if let Some(c) = rx(r"(?m)^\s*([^\s]+)\s*$").captures(&clean) {
                let m = c.get(1).unwrap();
                doc.entry(
                    if filename == "rust-toolchain" {
                        "rust"
                    } else {
                        "node"
                    },
                    m.as_str(),
                    Some(m.range()),
                    Section::Toolchain,
                );
            }
        }
        ".tool-versions" => {
            for c in rx(r"(?m)^\s*([\w-]+)\s+([^\r\n]+?)\s*$").captures_iter(&clean) {
                if let Some(name) = tool_name(&c[1]) {
                    let m = c.get(2).unwrap();
                    doc.entry(name, m.as_str(), Some(m.range()), Section::Toolchain);
                }
            }
        }
        "rust-toolchain.toml" | "mise.toml" | ".mise.toml" => {
            let parsed = toml_edit::Document::parse(doc.text.clone())
                .map_err(|e| error(filename, e.to_string()))?;
            let table = if filename == "rust-toolchain.toml" {
                "toolchain"
            } else {
                "tools"
            };
            if let Some(items) = parsed.get(table).and_then(toml_edit::Item::as_table_like) {
                for (key, item) in items.iter() {
                    let name = if table == "toolchain" {
                        (key == "channel").then_some("rust")
                    } else {
                        tool_name(key)
                    };
                    if let Some(name) = name {
                        if let (Some(value), Some(span)) = (item.as_str(), item.span()) {
                            doc.entry(
                                name,
                                value,
                                toml_string_span(&doc.text, span, value),
                                Section::Toolchain,
                            );
                        } else {
                            doc.entry(
                                name,
                                "multiple versions or complex tool configuration",
                                None,
                                Section::Toolchain,
                            );
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Load Gradle context without expanding the update allowlist. Other consumers
/// participate in compatibility/shared-source checks, not registry requests.
#[cfg(test)]
pub(crate) fn load(
    manifests: &[ManifestRef],
    root: &Path,
) -> Result<HashMap<PathBuf, Document>, DcuError> {
    load_with_discovery(manifests, root, false)
}

fn gradle_kind(path: &Path) -> bool {
    matches!(
        ManifestKind::from_path(path),
        Some(
            ManifestKind::Gradle
                | ManifestKind::GradleCatalog
                | ManifestKind::GradleProperties
                | ManifestKind::GradleWrapper
        )
    )
}

fn build_root(path: &Path, root: &Path) -> PathBuf {
    path.ancestors()
        .skip(1)
        .take_while(|p| p.starts_with(root))
        .find(|p| {
            p.join("settings.gradle").is_file()
                || p.join("settings.gradle.kts").is_file()
                || p.join("gradle/wrapper/gradle-wrapper.properties").is_file()
        })
        .unwrap_or(root)
        .to_owned()
}

fn read_document(path: &Path, selected: &[PathBuf]) -> Result<Document, DcuError> {
    let text = std::fs::read_to_string(path).map_err(|source| DcuError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut doc = parse(&text, path)?;
    if !selected.iter().any(|p| p == path) {
        for entry in &mut doc.entries {
            entry.requested = false;
        }
    }
    Ok(doc)
}

fn reference_source(
    documents: &HashMap<PathBuf, Document>,
    path: &Path,
    root: &Path,
    key: &str,
) -> Option<(PathBuf, Definition)> {
    let doc = documents.get(path)?;
    let bindings: Vec<_> = doc
        .property_bindings
        .iter()
        .filter(|(local, _)| local == key)
        .collect();
    if !bindings.is_empty() {
        let clean = uncomment(&doc.text, true);
        let outside = outside_strings(&clean);
        let assignments = rx(&format!(
            r"\b{}\s*(?::\s*\w+)?\s*(?:=|by\b)",
            regex::escape(key)
        ))
        .find_iter(&clean)
        .filter(|m| outside[m.start()])
        .count();
        if bindings.len() != 1 || assignments != 1 {
            return None;
        }
        // A Gradle property binding reads properties, never an unrelated script
        // variable with the same name. The nearest declaration wins.
        for parent in path.ancestors().skip(1).take_while(|p| p.starts_with(root)) {
            let candidate = parent.join("gradle.properties");
            if let Some(properties) = documents.get(&candidate) {
                let defs: Vec<_> = properties
                    .definitions
                    .iter()
                    .filter(|d| d.key == bindings[0].1)
                    .collect();
                if defs.len() == 1 {
                    return Some((candidate, defs[0].clone()));
                }
                if !defs.is_empty() {
                    return None;
                }
            }
        }
        return None;
    }
    let candidates = std::iter::once(path.to_owned()).chain(
        path.ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(root))
            .flat_map(|p| {
                [
                    p.join("gradle.properties"),
                    p.join("build.gradle.kts"),
                    p.join("build.gradle"),
                ]
            }),
    );
    for candidate in candidates {
        let Some(doc) = documents.get(&candidate) else {
            continue;
        };
        let defs: Vec<_> = doc.definitions.iter().filter(|d| d.key == key).collect();
        let clean = uncomment(&doc.text, true);
        let outside = outside_strings(&clean);
        let assignments = rx(&format!(
            r"\b{}\s*(?::\s*\w+)?\s*(?:=|by\b)",
            regex::escape(key)
        ))
        .find_iter(&clean)
        .filter(|m| outside[m.start()])
        .count();
        if defs.len() == 1 && assignments <= 1 {
            return Some((candidate, defs[0].clone()));
        }
        // A local dynamic/ambiguous assignment shadows an ancestor pin too.
        if !defs.is_empty() || assignments != 0 {
            return None;
        }
    }
    None
}

/// Reuse a complete -d discovery; other invocations walk only build scopes and
/// exact ancestor context paths. Expand shared-source owners only when needed.
#[allow(clippy::too_many_lines)]
pub(crate) fn load_with_discovery(
    manifests: &[ManifestRef],
    root: &Path,
    reuse_deep: bool,
) -> Result<HashMap<PathBuf, Document>, DcuError> {
    let mut documents = HashMap::new();
    let mut paths: Vec<_> = manifests
        .iter()
        .filter(|m| {
            matches!(
                m.kind,
                ManifestKind::Gradle
                    | ManifestKind::GradleCatalog
                    | ManifestKind::GradleProperties
                    | ManifestKind::GradleWrapper
                    | ManifestKind::ToolVersions
                    | ManifestKind::PackageJson
            )
        })
        .map(|m| m.path.clone())
        .collect();
    let selected = paths.clone();
    let mut roots: Vec<_> = selected
        .iter()
        .filter(|p| gradle_kind(p))
        .map(|p| build_root(p, root))
        .collect();
    roots.sort();
    roots.dedup();
    let mut scopes = roots.clone();
    for path in selected.iter().filter(|p| gradle_kind(p)) {
        for parent in path.ancestors().skip(1).take_while(|p| p.starts_with(root)) {
            for name in [
                "build.gradle",
                "build.gradle.kts",
                "settings.gradle",
                "settings.gradle.kts",
                "gradle.properties",
                "gradle/libs.versions.toml",
                "gradle/wrapper/gradle-wrapper.properties",
                ".tool-versions",
                "mise.toml",
                ".mise.toml",
            ] {
                scopes.push(parent.join(name));
            }
        }
    }
    scopes.sort();
    scopes.dedup();
    let mut allowed: std::collections::HashSet<_> = if roots.is_empty() {
        std::collections::HashSet::new()
    } else if reuse_deep {
        manifests.iter().map(|m| m.path.clone()).collect()
    } else {
        dependency_check_updates_core::Scanner::scan_scoped_checked(root, &scopes)?
            .into_iter()
            .map(|m| m.path)
            .collect()
    };
    if !roots.is_empty() {
        for path in &allowed {
            // Catalogs are parsed only when selected, in the same build, or
            // explicitly inherited by selected consumers. Other builds' bad
            // catalogs cannot interrupt a targeted query.
            let same_build = roots.contains(&build_root(path, root));
            let ancestor_probe = scopes.iter().any(|p| p == path);
            if ((gradle_kind(path)
                && (ManifestKind::from_path(path) != Some(ManifestKind::GradleCatalog)
                    || same_build
                    || ancestor_probe))
                || (ManifestKind::from_path(path) == Some(ManifestKind::ToolVersions)
                    && ancestor_probe))
                && !paths.contains(path)
            {
                paths.push(path.clone());
            }
        }
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        let doc = read_document(&path, &selected)?;
        documents.insert(path, doc);
    }
    let mut shared_roots = Vec::new();
    for path in &selected {
        let doc = &documents[path];
        let own = build_root(path, root);
        for reference in &doc.references {
            if let Some((source, _)) = reference_source(&documents, path, root, &reference.key) {
                let owner = source.parent().unwrap_or(root);
                if !owner.starts_with(&own) {
                    shared_roots.push(owner.to_owned());
                }
            }
        }
        if !doc.catalog_uses.is_empty()
            && let Some(catalog) = path
                .ancestors()
                .skip(1)
                .take_while(|p| p.starts_with(root))
                .map(|p| p.join("gradle/libs.versions.toml"))
                .find(|p| allowed.contains(p))
        {
            let owner = catalog.parent().and_then(Path::parent).unwrap_or(root);
            if !owner.starts_with(&own) {
                shared_roots.push(owner.to_owned());
            }
        }
    }
    shared_roots.sort();
    shared_roots.dedup();
    if !shared_roots.is_empty() {
        if !reuse_deep {
            allowed.extend(
                dependency_check_updates_core::Scanner::scan_scoped_checked(root, &shared_roots)?
                    .into_iter()
                    .map(|m| m.path),
            );
        }
        for path in &allowed {
            if gradle_kind(path)
                && ManifestKind::from_path(path) != Some(ManifestKind::GradleCatalog)
                && !documents.contains_key(path)
            {
                documents.insert(path.clone(), read_document(path, &selected)?);
            }
        }
    }
    // Read a descendant catalog only when a script actually uses it, e.g.
    // Kotlin aliases in a nested build affected by an ancestor wrapper.
    let mut catalogs = Vec::new();
    for doc in documents.values().filter(|d| !d.catalog_uses.is_empty()) {
        if let Some(path) = doc
            .path
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(root))
            .map(|p| p.join("gradle/libs.versions.toml"))
            .find(|p| allowed.contains(p))
            && !documents.contains_key(&path)
        {
            catalogs.push(path);
        }
    }
    catalogs.sort();
    catalogs.dedup();
    for path in catalogs {
        documents.insert(path.clone(), read_document(&path, &selected)?);
    }
    let snapshot = documents.clone();
    let mut context_paths: Vec<_> = snapshot.keys().cloned().collect();
    context_paths.sort();
    for path in context_paths {
        let original = &snapshot[&path];
        let mut repos = Vec::new();
        let boundary = build_root(&path, root);
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&boundary))
        {
            for name in [
                "build.gradle",
                "build.gradle.kts",
                "settings.gradle",
                "settings.gradle.kts",
            ] {
                if let Some(d) = snapshot.get(&parent.join(name)) {
                    for r in &d.repositories {
                        if !repos.contains(r) {
                            repos.push(r.clone());
                        }
                    }
                }
            }
        }
        // References already added by another consumer retain that consumer's
        // repository context, not the source property's own (possibly empty).
        for e in documents
            .get_mut(&path)
            .unwrap()
            .entries
            .iter_mut()
            .take(original.entries.len())
        {
            e.repositories.clone_from(&repos);
        }
        documents
            .get_mut(&path)
            .unwrap()
            .repositories
            .clone_from(&repos);
        for reference in &original.references {
            let source = reference_source(&snapshot, &path, root, &reference.key);
            if let Some((source_path, definition)) = source {
                let d = documents.get_mut(&source_path).unwrap();
                d.entry(
                    &reference.name,
                    &definition.value,
                    Some(definition.span),
                    reference.section,
                );
                d.entries.last_mut().unwrap().requested =
                    selected.contains(&path) || selected.contains(&source_path);
                if reference.name == "android.minSdk"
                    || matches!(definition.key.as_str(), "minSdk" | "minSdkVersion")
                {
                    d.entries.last_mut().unwrap().reason =
                        Some("minSdk source is a supported-device range; preserved".to_owned());
                }
                d.entries
                    .last_mut()
                    .unwrap()
                    .repositories
                    .clone_from(&repos);
                let dep = d.entries.last().unwrap().dep.clone();
                documents
                    .get_mut(&path)
                    .unwrap()
                    .resolved_uses
                    .push((source_path, dep));
            } else {
                let d = documents.get_mut(&path).unwrap();
                d.entry(&reference.name, &reference.key, None, reference.section);
                d.entries.last_mut().unwrap().reason =
                    Some(format!("unresolved version reference: {}", reference.key));
                d.entries.last_mut().unwrap().requested = selected.contains(&path);
            }
        }
    }
    // Default `libs` accessors resolve to the nearest ancestor's catalog. Keep
    // inline/version.ref source identity so another alias of the same artifact
    // does not accidentally enter an explicit --manifest update allowlist.
    let snapshot = documents.clone();
    let mut alias_paths: Vec<_> = snapshot.keys().collect();
    alias_paths.sort();
    let mut activated = std::collections::HashSet::new();
    for path in alias_paths {
        let original = &snapshot[path];
        for (accessor, section, applied) in &original.catalog_uses {
            let catalog = path
                .ancestors()
                .skip(1)
                .take_while(|p| p.starts_with(root))
                .find_map(|p| snapshot.get(&p.join("gradle/libs.versions.toml")));
            let alias = catalog.and_then(|d| {
                d.catalog_aliases
                    .iter()
                    .find(|a| a.accessor == *accessor && a.section == *section)
            });
            if let (Some(catalog), Some(alias)) = (catalog, alias) {
                let source_selected = selected.contains(&catalog.path);
                for (index, base) in catalog.entries.iter().enumerate().filter(|(_, e)| {
                    e.dep.name == alias.name
                        && e.dep.section == alias.section
                        && e.span == alias.span
                }) {
                    documents
                        .get_mut(path)
                        .unwrap()
                        .resolved_uses
                        .push((catalog.path.clone(), base.dep.clone()));
                    let requested = selected.contains(path) || source_selected;
                    let doc = documents.get_mut(&catalog.path).unwrap();
                    if requested && activated.insert((catalog.path.clone(), index)) {
                        doc.entries[index].requested = true;
                        doc.entries[index]
                            .repositories
                            .clone_from(&original.repositories);
                    } else {
                        // Keep every consumer, including non-selected uses of
                        // the same alias. Its build's repository context matters.
                        let mut consumer = base.clone();
                        consumer.requested = requested;
                        consumer.repositories.clone_from(&original.repositories);
                        doc.entries.push(consumer);
                    }
                }
                if *applied
                    && matches!(
                        alias.name.as_str(),
                        "org.jetbrains.kotlin.android" | "org.jetbrains.kotlin.multiplatform"
                    )
                {
                    documents
                        .get_mut(path)
                        .unwrap()
                        .applied_plugins
                        .push(alias.name.clone());
                }
            } else if selected.contains(path) {
                let d = documents.get_mut(path).unwrap();
                d.entry(
                    &format!("libs.{accessor}"),
                    "unresolved catalog alias",
                    None,
                    *section,
                );
            }
        }
    }
    for path in &selected {
        if ManifestKind::from_path(path) != Some(ManifestKind::GradleProperties) {
            continue;
        }
        let doc = documents.get_mut(path).expect("selected properties");
        for definition in doc.definitions.clone() {
            let key = definition.key.to_ascii_lowercase();
            if key.contains("version")
                && !key.starts_with("minsdk")
                && numeric(&definition.value)
                && !doc
                    .entries
                    .iter()
                    .any(|e| e.span.as_ref() == Some(&definition.span))
            {
                doc.entry(
                    &format!("gradle.property.{}", definition.key),
                    &definition.value,
                    None,
                    Section::Maven,
                );
                doc.entries.last_mut().unwrap().reason = Some(
                    "version property has no statically identified artifact consumer; preserved"
                        .into(),
                );
            }
        }
    }
    Ok(documents)
}

pub(crate) fn guard_shared_versions(
    documents: &HashMap<PathBuf, Document>,
    plans: &mut crate::compatibility::Plans,
) -> HashMap<PathBuf, String> {
    type Consumers = Vec<(String, Option<String>)>;
    let mut errors = HashMap::new();
    for (path, doc) in documents {
        let Some(updates) = plans.get_mut(path) else {
            continue;
        };
        let mut spans: HashMap<(usize, usize), Consumers> = HashMap::new();
        for e in &doc.entries {
            if let Some(span) = &e.span {
                let targets: Vec<_> = updates
                    .iter()
                    .filter(|u| {
                        e.requested
                            && u.name == e.dep.name
                            && u.from == e.dep.current_req
                            && u.section == e.dep.section
                    })
                    .map(|u| u.to.clone())
                    .collect();
                let consumers = spans.entry((span.start, span.end)).or_default();
                if targets.is_empty() {
                    consumers.push((e.dep.name.clone(), None));
                } else {
                    consumers.extend(targets.into_iter().map(|to| (e.dep.name.clone(), Some(to))));
                }
            }
        }
        for entries in spans.values() {
            let targets: Vec<_> = entries.iter().filter_map(|(_, v)| v.as_ref()).collect();
            // A rejected/filtered consumer sharing the value must also be preserved.
            if !targets.is_empty()
                && (targets.iter().any(|v| *v != targets[0])
                    || entries.iter().any(|(_, v)| v.is_none()))
            {
                updates.retain(|u| !entries.iter().any(|(name, _)| name == &u.name));
                errors.insert(path.clone(),"shared version source has conflicting targets or filtered consumers; preserved".to_owned());
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoted_repository_and_definition_text_is_not_executable_gradle() {
        let text = "val documentation = \"\"\"\nmaven { url = uri(\"https://fake.example\") }\nval fakeVersion = \"1.0\"\n\"\"\"\n";
        let doc = parse(text, Path::new("build.gradle.kts")).unwrap();
        assert!(doc.repositories.is_empty());
        assert!(doc.definitions.iter().all(|d| d.key != "fakeVersion"));
    }

    #[test]
    fn absent_and_ambiguous_gradle_property_sources_are_never_guessed() {
        let root = Path::new("project");
        let path = root.join("app/build.gradle.kts");
        let doc = parse("val pin = providers.gradleProperty(\"sharedVersion\").get()\nimplementation(\"g:a:$pin\")\n", &path).unwrap();
        assert_eq!(doc.property_bindings.len(), 1);
        let mut docs = HashMap::from([(path.clone(), doc)]);
        assert!(reference_source(&docs, &path, root, "pin").is_none());
        assert!(reference_source(&docs, &path, root, "unknown").is_none());
        let props = root.join("app/gradle.properties");
        docs.insert(
            props.clone(),
            parse("sharedVersion=1.0\nsharedVersion=2.0\n", &props).unwrap(),
        );
        assert!(reference_source(&docs, &path, root, "pin").is_none());
    }

    #[test]
    fn shared_source_discovery_loads_a_consumed_catalog_in_another_nested_build() {
        use dependency_check_updates_core::ManifestRef;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (file, text) in [
            ("build.gradle.kts", "val sharedVersion = \"1.0\"\n"),
            ("a/settings.gradle.kts", "rootProject.name = \"a\"\n"),
            (
                "a/build.gradle.kts",
                "implementation(\"g:a:$sharedVersion\")\n",
            ),
            ("b/settings.gradle.kts", "rootProject.name = \"b\"\n"),
            ("b/build.gradle.kts", "implementation(libs.shared)\n"),
            (
                "b/gradle/libs.versions.toml",
                "[libraries]\nshared = { module = \"g:a\", version = \"1.0\" }\n",
            ),
        ] {
            let p = root.join(file);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let selected = ManifestRef {
            path: root.join("a/build.gradle.kts"),
            kind: ManifestKind::Gradle,
        };
        let docs = load_with_discovery(&[selected], root, false).unwrap();
        assert!(docs.contains_key(&root.join("b/gradle/libs.versions.toml")));
        assert_eq!(
            docs[&root.join("b/build.gradle.kts")].resolved_uses[0]
                .1
                .name,
            "g:a"
        );
        assert!(!docs[&root.join("b/gradle/libs.versions.toml")].entries[0].requested);
    }
    use super::*;

    #[test]
    fn nested_comments_and_quoted_fake_declarations_are_not_dependencies() {
        let text = r#"/* outer /* implementation("fake:inside:1.0") */ end */
val text = "escaped \" quote"
val fake = '''
ext['ver'] = '1.0'
val sdk = 35
id('fake.plugin') version '1.0'
compileSdk = 35
maven { url = 'https://fake.invalid' }
'''
"#;
        let doc = parse(text, Path::new("build.gradle.kts")).unwrap();
        assert!(doc.entries.is_empty());
        assert!(doc.definitions.is_empty());
        assert!(doc.repositories.is_empty());
        assert!(
            parse("irrelevant", Path::new("unknown.txt"))
                .unwrap()
                .entries
                .is_empty()
        );
        let mut doc = parse("", Path::new("unknown.txt")).unwrap();
        parse_tools(&mut doc).unwrap();
    }

    #[test]
    fn gradle_repository_and_plugin_forms_have_explicit_static_boundaries() {
        let text = r#"repositories {
    google()
    mavenCentral()
    gradlePluginPortal()
    maven { url = repositoryUrl }
    maven(repositoryUrl)
    exclusiveContent {}
}
extra['v'] = '1.0'
id('literal.plugin') version '1.0' + suffix
id('interpolated.plugin') version "$v"
id('variable.plugin') version v
implementation('bad:notation')
"#;
        let doc = parse(text, Path::new("build.gradle.kts")).unwrap();
        assert_eq!(doc.references.len(), 2);
        assert_eq!(doc.definitions.len(), 1);
        assert_eq!(doc.entries.len(), 2);
        assert!(doc.entries.iter().all(|e| e.reason.is_some()));
        assert!(
            doc.repositories
                .iter()
                .any(|r| r.contains("content filters"))
        );
        assert_eq!(
            doc.repositories
                .iter()
                .filter(|r| r.contains("dynamic"))
                .count(),
            2
        );
        assert_eq!(
            doc.repositories
                .iter()
                .filter(|r| r.starts_with("https://"))
                .count(),
            3
        );
    }

    #[test]
    fn catalog_literal_rich_missing_and_escaped_versions_keep_their_meaning() {
        let text = r#"[versions]
escaped = "1.\u0030"
[libraries]
literal = "group:artifact:1.0"
rich = { module = "group:rich", version = { strictly = "1.0" } }
missing = { module = "group:missing" }
unknown = { unsupported = "1.0" }
"#;
        let doc = parse(text, Path::new("gradle/libs.versions.toml")).unwrap();
        assert_eq!(doc.entries.len(), 4);
        assert!(doc.definitions.is_empty());
        assert!(doc.entries[0].span.is_some());
        assert!(doc.entries[1..].iter().all(|e| e.reason.is_some()));
        assert_eq!(toml_string_span("'''1.0'''", 0..9, "1.0"), Some(3..6));
        assert!(toml_string_span("x", 0..20, "x").is_none());
    }

    #[test]
    fn package_manager_hashes_escaping_unknown_tools_and_complex_mise_are_preserved() {
        for text in [
            r#"{"packageManager":"bun@1.0.0+sha256.hash"}"#,
            r#"{"packageManager":"pnpm@1.0.0+invalidhash"}"#,
            r#"{"packageManager":"pnpm@1.\u0030.0"}"#,
            r#"{"packageManager":"unsupported@1.0.0"}"#,
        ] {
            let doc = parse(text, Path::new("package.json")).unwrap();
            assert_eq!(doc.entries.len(), 1);
            assert!(doc.entries[0].reason.is_some());
        }
        let doc = parse(
            "[tools]\nnode = ['20','22']\nunsupported = '1.0'\n",
            Path::new("mise.toml"),
        )
        .unwrap();
        assert_eq!(doc.entries.len(), 1);
        assert!(doc.entries[0].reason.is_some());
        let doc = parse(
            "distributionUrl=https://mirror.example/custom.zip\n",
            Path::new("gradle/wrapper/gradle-wrapper.properties"),
        )
        .unwrap();
        assert!(doc.entries[0].reason.is_some());
    }

    #[test]
    fn patcher_rejects_conflicting_source_updates_and_skips_unsupported_declarations() {
        let path = Path::new("build.gradle.kts");
        let text = "implementation(\"group:artifact:1.0\")\n";
        let doc = parse(text, path).unwrap();
        let update = PlannedUpdate {
            name: "group:artifact".into(),
            from: "1.0".into(),
            to: "2.0".into(),
            section: Section::Maven,
        };
        let handler = ProjectHandler(doc.clone());
        assert!(
            handler
                .apply_updates(text, std::slice::from_ref(&update))
                .unwrap()
                .contains(":2.0")
        );
        let other = PlannedUpdate {
            to: "3.0".into(),
            ..update.clone()
        };
        assert!(
            doc.apply(text, &[update.clone(), other], Vec::new())
                .is_err()
        );
        let mut unsupported = doc;
        unsupported.entries[0].reason = Some("dynamic".into());
        assert_eq!(
            unsupported.apply(text, &[update], Vec::new()).unwrap(),
            text
        );
    }
}

//! Deterministic regression tests through the real scan/resolve/patch pipeline.
use crate::{
    Cli, project,
    run::{ManifestJob, execute_at, report_rows, run_at},
    tool_registry::{Endpoints, ToolRegistry},
};
use clap::Parser;
use dependency_check_updates_core::{DependencySection, ManifestKind, Scanner, TargetLevel};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex},
};

const ANDROID: &str = "apps/app/src-tauri/gen/android";
const FIXTURES: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!("../tests/fixtures/tauri/package.json"),
    ),
    (".nvmrc", include_str!("../tests/fixtures/tauri/.nvmrc")),
    (
        ".node-version",
        include_str!("../tests/fixtures/tauri/.node-version"),
    ),
    (
        ".tool-versions",
        include_str!("../tests/fixtures/tauri/.tool-versions"),
    ),
    (
        ".mise.toml",
        include_str!("../tests/fixtures/tauri/.mise.toml"),
    ),
    (
        "mise.toml",
        include_str!("../tests/fixtures/tauri/mise.toml"),
    ),
    (
        "rust-toolchain.toml",
        include_str!("../tests/fixtures/tauri/rust-toolchain.toml"),
    ),
    (
        "gradle/wrapper/gradle-wrapper.properties",
        include_str!("../tests/fixtures/tauri/gradle/wrapper/gradle-wrapper.properties"),
    ),
    (
        "gradle/libs.versions.toml",
        include_str!("../tests/fixtures/tauri/gradle/libs.versions.toml"),
    ),
    (
        "settings.gradle.kts",
        include_str!("../tests/fixtures/tauri/settings.gradle.kts"),
    ),
    (
        "apps/app/src-tauri/gen/android/settings.gradle.kts",
        include_str!("../tests/fixtures/tauri/apps/app/src-tauri/gen/android/settings.gradle.kts"),
    ),
    (
        "apps/app/src-tauri/gen/android/build.gradle.kts",
        include_str!("../tests/fixtures/tauri/apps/app/src-tauri/gen/android/build.gradle.kts"),
    ),
    (
        "apps/app/src-tauri/gen/android/buildSrc/build.gradle.kts",
        include_str!(
            "../tests/fixtures/tauri/apps/app/src-tauri/gen/android/buildSrc/build.gradle.kts"
        ),
    ),
    (
        "apps/app/src-tauri/gen/android/gradle.properties",
        include_str!("../tests/fixtures/tauri/apps/app/src-tauri/gen/android/gradle.properties"),
    ),
    (
        "apps/app/src-tauri/gen/android/app/build.gradle.kts",
        include_str!("../tests/fixtures/tauri/apps/app/src-tauri/gen/android/app/build.gradle.kts"),
    ),
    (
        "Cargo.toml",
        include_str!("../tests/fixtures/tauri/Cargo.toml.fixture"),
    ),
    (
        "pyproject.toml",
        include_str!("../tests/fixtures/tauri/pyproject.toml"),
    ),
    (
        "Dockerfile",
        include_str!("../tests/fixtures/tauri/Dockerfile"),
    ),
    (
        ".github/workflows/CI.yml",
        include_str!("../tests/fixtures/tauri/.github/workflows/CI.yml"),
    ),
];

fn write(root: &Path, name: &str, text: &str) {
    let p = root.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}
fn read(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).unwrap()
}
fn fixture(server: &MockServer) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for (name, text) in FIXTURES {
        write(
            tmp.path(),
            name,
            &text
                .replace("google()", &format!("maven(\"{}\")", server.uri()))
                .replace("mavenCentral()", "")
                .replace("gradlePluginPortal()", "")
                .replace("\r\n", "\n")
                .replace('\n', "\r\n"),
        );
    }
    tmp
}
async fn metadata(server: &MockServer, p: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}
async fn registry() -> (MockServer, ToolRegistry) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = MockServer::start().await;
    for (p, versions) in [
        (
            "/com/android/tools/build/gradle/maven-metadata.xml",
            &["8.5.1", "8.5.2", "8.10.1"][..],
        ),
        (
            "/org/jetbrains/kotlin/kotlin-gradle-plugin/maven-metadata.xml",
            &["1.9.25", "2.2.10"][..],
        ),
        (
            "/androidx/webkit/webkit/maven-metadata.xml",
            &["1.6.1", "1.6.2", "1.12.1"][..],
        ),
        (
            "/androidx/appcompat/appcompat/maven-metadata.xml",
            &["1.6.1", "1.7.0"][..],
        ),
        (
            "/com/google/android/material/material/maven-metadata.xml",
            &["1.8.0", "1.12.0"][..],
        ),
        ("/junit/junit/maven-metadata.xml", &["4.13.2", "4.13.3"][..]),
        (
            "/androidx/test/ext/junit/maven-metadata.xml",
            &["1.1.4", "1.2.1"][..],
        ),
        (
            "/androidx/test/espresso/espresso-core/maven-metadata.xml",
            &["3.5.0", "3.6.1"][..],
        ),
    ] {
        let body = format!(
            "<metadata><versioning><versions>{}</versions></versioning></metadata>",
            versions.iter().fold(String::new(), |mut s, v| {
                write!(s, "<version>{v}</version>").unwrap();
                s
            })
        );
        metadata(&server, p, &body).await;
    }
    metadata(&server,"/gradle",r#"[{"version":"8.9","snapshot":false,"buildTime":"20240101"},{"version":"8.13","snapshot":false,"buildTime":"20250101"}]"#).await;
    metadata(
        &server,
        "/distributions/gradle-8.13-bin.zip.sha256",
        &"b".repeat(64),
    )
    .await;
    metadata(
        &server,
        "/distributions/gradle-8.13-all.zip.sha256",
        &"c".repeat(64),
    )
    .await;
    metadata(&server,"/node",r#"[{"version":"v20.1.0","date":"2023-01-01","lts":false},{"version":"v22.15.1","date":"2025-01-01","lts":"Jod"}]"#).await;
    metadata(
        &server,
        "/rust",
        "[pkg.rust]\nversion = \"1.86.0 (hash date)\"\n",
    )
    .await;
    metadata(&server, "/jdk", r#"{"versions":[{"semver":"17.0.0+1"}]}"#).await;
    metadata(
        &server,
        "/repos/oven-sh/bun/releases",
        r#"[{"tag_name":"bun-v1.2.7","draft":false,"published_at":"2025-01-01"}]"#,
    )
    .await;
    metadata(&server,"/android",r#"<repository><remotePackage path="platforms;android-35"><revision><major>1</major></revision></remotePackage><remotePackage path="platforms;android-36"><revision><major>1</major></revision></remotePackage><remotePackage path="platforms;android-37-preview"><preview>1</preview></remotePackage></repository>"#).await;
    for (name, current, latest) in [
        ("pnpm", "10.12.1", "10.13.1"),
        ("yarn", "1.22.20", "1.22.22"),
        ("npm", "10.0.0", "11.0.0"),
    ] {
        metadata(&server,&format!("/{name}"),&format!(r#"{{"dist-tags":{{"latest":"{latest}"}},"versions":{{"{current}":{{}},"{latest}":{{}}}}}}"#)).await;
    }
    let uri = server.uri();
    let registry = ToolRegistry::with_endpoints(Endpoints {
        gradle: format!("{uri}/gradle"),
        distributions: format!("{uri}/distributions"),
        node: format!("{uri}/node"),
        rust: format!("{uri}/rust"),
        github: uri.clone(),
        android: format!("{uri}/android"),
        jdk: format!("{uri}/jdk"),
        npm: uri,
        yarn: format!("{}/yarn-tags", server.uri()),
        yarn_downloads: server.uri(),
        ..Endpoints::default()
    });
    (server, registry)
}

#[tokio::test]
async fn integrity_failure_blocks_only_its_own_declaration_and_strict_mode_aborts() {
    let (server, registry) = registry().await;
    Mock::given(path("/pnpm/10.13.1"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let original = r#"{"packageManager":"pnpm@10.12.1+sha512.old","dependencies":{"npm":"^10.0.0","pnpm":"10.12.1","yarn":"1.22.22"}}"#;
    for strict in [false, true] {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "package.json", original);
        let mut cli = Cli::parse_from(["dcu", "-u"]);
        cli.fail_on_incomplete = strict;
        let report = execute_at(&cli, tmp.path(), &registry, false)
            .await
            .unwrap();
        let rows: Vec<_> = report
            .items
            .iter()
            .filter_map(|r| {
                if let crate::report::Item::Project(r) = r {
                    Some(r)
                } else {
                    None
                }
            })
            .collect();
        let tool = rows
            .iter()
            .find(|r| r.name == "pnpm" && r.section == "toolchain")
            .unwrap();
        assert_eq!(tool.status, crate::report::Status::Blocked);
        assert!(tool.latest.is_some());
        assert_eq!(
            rows.iter().find(|r| r.name == "yarn").unwrap().status,
            crate::report::Status::Current
        );
        assert_eq!(
            rows.iter()
                .find(|r| r.name == "pnpm" && r.section == "dependencies")
                .unwrap()
                .status,
            crate::report::Status::Update
        );
        let library = rows.iter().find(|r| r.name == "npm").unwrap();
        assert_eq!(library.status, crate::report::Status::Update);
        assert_eq!(library.updated, !strict);
        let updated = read(tmp.path(), "package.json");
        assert!(updated.contains("pnpm@10.12.1+sha512.old"));
        if strict {
            assert_eq!(updated, original);
            assert_eq!(report.exit_code(&cli), 2);
        } else {
            assert!(updated.contains("^11.0.0"));
            assert_eq!(report.exit_code(&cli), 0);
        }
    }
}

#[tokio::test]
async fn compatible_suggestions_preserve_latest_and_apply_only_when_selected() {
    let (server, registry) = registry().await;
    // Latest AGP 9 requires an explicit plugin migration; the supported 8.10.0
    // alternative works with Kotlin 2.2.10 and Gradle 8.13.
    Mock::given(path("/com/android/tools/build/gradle/maven-metadata.xml")).respond_with(ResponseTemplate::new(200).set_body_string("<metadata><versioning><versions><version>8.5.1</version><version>8.10.0</version><version>9.0.0</version></versions></versioning></metadata>")).with_priority(1).mount(&server).await;
    let tmp = TempDir::new().unwrap();
    let build = format!(
        "repositories {{ maven {{ url = uri(\"{}\") }} }}\nplugins {{ id(\"com.android.application\") version \"8.5.1\"; id(\"org.jetbrains.kotlin.android\") version \"1.9.25\" }}\n",
        server.uri()
    );
    write(tmp.path(), "build.gradle.kts", &build);
    write(
        tmp.path(),
        "settings.gradle.kts",
        "rootProject.name = \"compatible\"\n",
    );
    write(tmp.path(), ".tool-versions", "java 17\n");
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\ndistributionSha256Sum=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    let query = execute_at(
        &Cli::parse_from(["dcu", "-d"]),
        tmp.path(),
        &registry,
        false,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), "build.gradle.kts"), build);
    let json = query.json(crate::cli::OutputFormat::JsonReport).unwrap();
    let agp = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "com.android.application")
        .unwrap();
    assert_eq!(agp["latest"], "9.0.0");
    assert_eq!(agp["selected"], "9.0.0");
    assert_eq!(agp["compatible"], "8.10.0");
    assert!(agp["to"].is_null());
    let cli = Cli::parse_from(["dcu", "-d", "--compatible", "-u", "--fail-on-incomplete"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(
        report.exit_code(&cli),
        0,
        "{}",
        report.json(crate::cli::OutputFormat::JsonReport).unwrap()
    );
    let applied = read(tmp.path(), "build.gradle.kts");
    assert!(applied.contains("8.10.0"));
    assert!(applied.contains("2.2.10"));
    let wrapper = read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties");
    assert!(wrapper.contains("gradle-8.13-bin.zip"));
    assert!(wrapper.contains(&"b".repeat(64)));
    assert_eq!(read(tmp.path(), ".tool-versions"), "java 17\n");
}

#[tokio::test]
async fn compatible_tools_keep_short_pins_filters_targets_and_unknown_states() {
    let (server, registry) = registry().await;
    Mock::given(path("/gradle")).respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"version":"8.9","snapshot":false},{"version":"8.13","snapshot":false},{"version":"9.8","snapshot":false}]"#)).with_priority(1).mount(&server).await;
    Mock::given(path("/jdk"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"versions":[{"semver":"17.0.15+6"},{"semver":"27.0.1+4"}]}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    for args in [
        vec!["dcu", "-d", "-u", "--compatible"],
        vec!["dcu", "-d", "-u", "--compatible", "--target", "minor"],
        vec!["dcu", "-d", "-u", "--compatible", "--reject", "jdk"],
        vec!["dcu", "-d", "-u", "--compatible", "--target", "patch"],
    ] {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "settings.gradle.kts",
            "rootProject.name = \"plain\"\n",
        );
        write(
            tmp.path(),
            ".tool-versions",
            "java 17 # keep line precision\r\n",
        );
        write(
            tmp.path(),
            "gradle/wrapper/gradle-wrapper.properties",
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\r\n",
        );
        let cli = Cli::parse_from(args.clone());
        let report = execute_at(&cli, tmp.path(), &registry, false)
            .await
            .unwrap();
        let expected =
            if args.contains(&"minor") || args.contains(&"patch") || args.contains(&"--reject") {
                "17"
            } else {
                "27"
            };
        assert_eq!(
            read(tmp.path(), ".tool-versions"),
            format!("java {expected} # keep line precision\r\n")
        );
        let gradle = if args.contains(&"patch") {
            "8.9"
        } else if args.contains(&"minor") {
            "8.13"
        } else {
            "9.8"
        };
        assert!(
            read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties")
                .contains(&format!("gradle-{gradle}-bin.zip")),
            "{}",
            report.json(crate::cli::OutputFormat::JsonReport).unwrap()
        );
    }
    let tmp = TempDir::new().unwrap();
    let wrapper =
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\n";
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        wrapper,
    );
    let report = execute_at(
        &Cli::parse_from(["dcu", "-d", "-u", "--compatible"]),
        tmp.path(),
        &registry,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties"),
        wrapper
    );
    let row = report
        .items
        .iter()
        .find_map(|r| {
            if let crate::report::Item::Project(r) = r {
                Some(r)
            } else {
                None
            }
        })
        .unwrap();
    assert!(row.compatible.is_none());
    assert_eq!(row.status, crate::report::Status::Unverified);
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        ".tool-versions",
        "java 17\nnode lts/*\nrust stable\n",
    );
    let cli = Cli::parse_from(["dcu", "-u", "--compatible", "--fail-on-incomplete"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert_eq!(
        read(tmp.path(), ".tool-versions"),
        "java 27\nnode lts/*\nrust stable\n"
    );
    assert!(report.compatibility_rules.is_empty());
}

#[tokio::test]
async fn compatible_selection_isolates_unconnected_builds() {
    let (_server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    for build in ["a", "b"] {
        write(
            tmp.path(),
            &format!("apps/{build}/settings.gradle.kts"),
            "rootProject.name = \"separate\"\n",
        );
        write(
            tmp.path(),
            &format!("apps/{build}/gradle/wrapper/gradle-wrapper.properties"),
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\n",
        );
    }
    write(tmp.path(), "apps/a/.tool-versions", "java 17\n");
    let cli = Cli::parse_from(["dcu", "-d", "-u", "--compatible"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert!(
        read(
            tmp.path(),
            "apps/a/gradle/wrapper/gradle-wrapper.properties"
        )
        .contains("gradle-8.13-bin.zip")
    );
    assert!(
        read(
            tmp.path(),
            "apps/b/gradle/wrapper/gradle-wrapper.properties"
        )
        .contains("gradle-8.9-bin.zip")
    );
    assert!(report.incomplete());
}

#[tokio::test]
async fn stale_and_future_rule_sources_are_reported_and_cannot_approve_updates() {
    let (_server, registry) = registry().await;
    for date in ["1900-01-01", "9999-12-31"] {
        let tmp = TempDir::new().unwrap();
        let wrapper =
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\n";
        write(
            tmp.path(),
            "gradle/wrapper/gradle-wrapper.properties",
            wrapper,
        );
        write(tmp.path(), ".tool-versions", "java 17\n");
        write(tmp.path(), "rules.json", &serde_json::json!({"schemaVersion":1,"verifiedAt":date,"sources":["https://docs.gradle.org/current/userguide/compatibility.html"]}).to_string());
        let cli = Cli::parse_from([
            "dcu",
            "-d",
            "-u",
            "--compatible",
            "--strict-compatibility",
            "--fail-on-incomplete",
            "--compatibility-file",
            "rules.json",
        ]);
        let report = execute_at(&cli, tmp.path(), &registry, false)
            .await
            .unwrap();
        assert_eq!(report.exit_code(&cli), 2);
        assert_eq!(
            read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties"),
            wrapper
        );
        let json = report.json(crate::cli::OutputFormat::JsonReport).unwrap();
        let provenance = &json["compatibilityRules"][1];
        assert_eq!(provenance["verifiedAt"], date);
        assert_eq!(provenance["userSupplied"], true);
        assert_eq!(provenance["stale"], date == "1900-01-01");
        assert_eq!(provenance["future"], date == "9999-12-31");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "compatibility-rules-date")
        );
    }
}

#[tokio::test]
async fn tauri_scan_check_and_upgrade_all_sources() {
    let (server, registry) = registry().await;
    let tmp = fixture(&server);
    let manifests = Scanner::scan_deep(tmp.path());
    assert_eq!(manifests.len(), FIXTURES.len());
    for name in [".nvmrc", ".node-version", ".tool-versions", ".mise.toml"] {
        assert!(manifests.iter().any(|m| m.path == tmp.path().join(name)));
    }
    let before: Vec<_> = FIXTURES
        .iter()
        .map(|(p, _)| (*p, read(tmp.path(), p)))
        .collect();
    assert!(
        run_at(
            &Cli::parse_from(["dcu", "-d"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    for (path, text) in &before {
        assert_eq!(&read(tmp.path(), path), text, "read-only changed {path}");
    }
    assert!(
        run_at(
            &Cli::parse_from(["dcu", "-d", "-u", "--format", "json"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    for file in [
        format!("{ANDROID}/build.gradle.kts"),
        format!("{ANDROID}/buildSrc/build.gradle.kts"),
    ] {
        let text = read(tmp.path(), &file);
        assert!(
            text.contains("com.android.tools.build:gradle:8.10.1"),
            "{text}"
        );
        assert!(!text.contains("com.android.tools.build:gradle:8.5.1"));
    }
    let app = read(tmp.path(), &format!("{ANDROID}/app/build.gradle.kts"));
    assert!(app.contains("androidx.webkit:webkit:1.12.1"));
    assert!(app.contains("$webkitVersion"));
    assert!(app.contains("compileSdk = 36"));
    assert!(app.contains("targetSdk = 36"));
    assert!(app.contains("minSdk = 24"));
    assert!(app.contains("${lookupVersion()}"));
    assert!(
        read(tmp.path(), &format!("{ANDROID}/gradle.properties"))
            .contains("webkitVersion = 1.12.1")
    );
    let catalog = read(tmp.path(), "gradle/libs.versions.toml");
    assert!(catalog.contains("webkit = \"1.12.1\" # Shared version source"));
    assert!(catalog.contains("agp = \"8.10.1\""));
    assert!(catalog.contains("kotlin = \"2.2.10\""));
    let wrapper = read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties");
    assert!(wrapper.contains("https\\://services.gradle.org/distributions/gradle-8.13-bin.zip"));
    assert!(wrapper.contains(&format!("distributionSha256Sum={}", "b".repeat(64))));
    assert_eq!(read(tmp.path(), ".nvmrc"), "v22.15.1\r\n");
    assert!(read(tmp.path(), "package.json").contains("pnpm@10.13.1"));
    assert!(read(tmp.path(), "package.json").contains("\">=20\""));
    assert!(read(tmp.path(), "mise.toml").contains("rust = \"stable\""));
    for (path, text) in &before {
        let new = read(tmp.path(), path);
        assert_eq!(new.matches("\r\n").count(), text.matches("\r\n").count());
    }
    let build = read(tmp.path(), &format!("{ANDROID}/build.gradle.kts"));
    assert!(build.contains("fake.comment:dependency:1.0.0"));
    assert!(build.contains("fake.block:dependency:1.0.0"));
}

#[tokio::test]
async fn manifest_filter_patch_reject_and_reference_source() {
    let (server, registry) = registry().await;
    let tmp = fixture(&server);
    let app = format!("{ANDROID}/app/build.gradle.kts");
    let before = read(tmp.path(), &app);
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--manifest",
        &app,
        "webkit",
        "--target",
        "patch",
    ]);
    assert!(run_at(&cli, tmp.path(), &registry, false).await.unwrap());
    assert!(read(tmp.path(), &app).contains("androidx.webkit:webkit:1.6.2"));
    assert!(
        read(tmp.path(), &format!("{ANDROID}/gradle.properties")).contains("webkitVersion = 1.6.2")
    );
    assert!(read(tmp.path(), &app).contains("appcompat:1.6.1"));
    assert_eq!(
        read(tmp.path(), &app).matches("minSdk = 24").count(),
        before.matches("minSdk = 24").count()
    );
    let unchanged = read(tmp.path(), &app);
    assert!(
        !run_at(
            &Cli::parse_from([
                "dcu",
                "-u",
                "--manifest",
                &app,
                "webkit",
                "--reject",
                "webkit"
            ]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), &app), unchanged);
}

#[tokio::test]
async fn wrapper_all_and_failed_checksum_preserve_files() {
    let (server, registry) = registry().await;
    let wrapper = "gradle/wrapper/gradle-wrapper.properties";
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        wrapper,
        include_str!("../tests/fixtures/tauri/gradle/wrapper/gradle-wrapper.properties"),
    );
    write(
        tmp.path(),
        wrapper,
        &read(tmp.path(), wrapper).replace("-bin.zip", "-all.zip"),
    );
    assert!(
        run_at(
            &Cli::parse_from(["dcu", "-u", "--manifest", wrapper]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert!(read(tmp.path(), wrapper).contains("-8.13-all.zip"));
    assert!(read(tmp.path(), wrapper).contains(&"c".repeat(64)));
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        wrapper,
        include_str!("../tests/fixtures/tauri/gradle/wrapper/gradle-wrapper.properties"),
    );
    let before = read(tmp.path(), wrapper);
    Mock::given(path("/distributions/gradle-8.13-bin.zip.sha256"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(
        !run_at(
            &Cli::parse_from(["dcu", "-u", "--manifest", wrapper]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), wrapper), before);
}

#[tokio::test]
async fn conflicts_and_repository_failures_keep_original() {
    let (server, registry) = registry().await;
    let tmp = fixture(&server);
    metadata(
        &server,
        "/gradle-conflict",
        r#"[{"version":"8.9","snapshot":false}]"#,
    )
    .await;
    let mut registry = registry;
    registry.endpoints.gradle = format!("{}/gradle-conflict", server.uri());
    let build = format!("{ANDROID}/build.gradle.kts");
    let before = read(tmp.path(), &build);
    run_at(
        &Cli::parse_from(["dcu", "-d", "-u"]),
        tmp.path(),
        &registry,
        false,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), &build), before);
    let tmp = fixture(&server);
    let app = format!("{ANDROID}/app/build.gradle.kts");
    let before = read(tmp.path(), &app);
    Mock::given(path_regex("/androidx/webkit/.*"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(
        !run_at(
            &Cli::parse_from(["dcu", "-u", "--manifest", &app, "webkit"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), &app), before);
    let entry = project::Entry {
        requested: true,
        dep: dependency_check_updates_core::DependencySpec {
            name: "example:private".into(),
            current_req: "1.0.0".into(),
            section: DependencySection::Maven,
            path_version: None,
        },
        span: None,
        reason: None,
        repositories: vec!["https://private.example/maven".into()],
        integrity: None,
    };
    assert!(
        registry
            .resolve(&entry, TargetLevel::Latest)
            .await
            .unwrap_err()
            .to_string()
            .contains("private or unsupported repository")
    );
}

#[test]
fn parser_handles_comments_channels_and_dynamic_expressions() {
    let text = "// implementation(\"bad:fake:1.0\")\n/* implementation(\"bad:block:1.0\") */\nval prose = \"implementation('bad:string:1.0')\"\nval version = \"1.0.0\"\nimplementation(\"good:artifact:$version\")\nimplementation(\"good:artifact:1.0.0\" + suffix)\nimplementation(computeDependency())\nid(\"example.plugin\") version \"1.0.0\"\ncompileSdk = 35 + offset\nminSdk = 24\n";
    let doc = project::parse(text, Path::new("build.gradle.kts")).unwrap();
    assert!(!doc.entries.iter().any(|e| e.dep.name.starts_with("bad")));
    assert!(!doc.entries.iter().any(|e| e.dep.name.contains("minSdk")));
    assert!(doc.entries.iter().filter(|e| e.reason.is_some()).count() >= 3);
    for filename in [".nvmrc", "rust-toolchain"] {
        let doc = project::parse("stable\r\n", Path::new(filename)).unwrap();
        assert!(
            doc.entries[0]
                .reason
                .as_ref()
                .unwrap()
                .starts_with("channel")
        );
        assert_eq!(doc.apply("stable\r\n", &[], vec![]).unwrap(), "stable\r\n");
    }
}

#[test]
fn json_reports_failed_and_unsupported_without_claiming_current() {
    let document = project::parse(
        "implementation(\"g:a:${getVersion()}\")\n",
        Path::new("build.gradle.kts"),
    )
    .unwrap();
    let job = ManifestJob {
        manifest_ref: dependency_check_updates_core::ManifestRef {
            path: PathBuf::from("build.gradle.kts"),
            kind: ManifestKind::Gradle,
        },
        display_path: "build.gradle.kts".into(),
        text: document.text.clone(),
        handler: Box::new(project::ProjectHandler(document.clone())),
        deps: document.dependencies(),
        document: Some(document),
    };
    let rows = report_rows(
        &job,
        &[(0, Err(project::error("g:a", "dynamic expression")))],
        &[],
        None,
        None,
        false,
    );
    let json = serde_json::to_string(&rows).unwrap();
    let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded[0]["status"], "unsupported");
    assert!(!decoded[0]["updated"].as_bool().unwrap());
    assert!(decoded[0]["latest"].is_null());
}

#[tokio::test]
async fn package_manager_hashes_use_the_correct_release_bytes() {
    let (server, registry) = registry().await;
    metadata(
        &server,
        "/yarn-tags",
        r#"{"tags":["4.0.0","4.1.0"],"aliases":{"stable":"4.1.0"}}"#,
    )
    .await;
    metadata(&server, "/4.1.0/packages/yarnpkg-cli/bin/yarn.js", "abc").await;
    for (name, latest) in [("pnpm", "10.13.1"), ("npm", "11.0.0"), ("yarn", "1.22.22")] {
        metadata(
            &server,
            &format!("/{name}/{latest}"),
            &format!(
                r#"{{"dist":{{"tarball":"{}/tarballs/{name}"}}}}"#,
                server.uri()
            ),
        )
        .await;
        metadata(&server, &format!("/tarballs/{name}"), "abc").await;
    }
    for (manager, old, latest, algo, digest) in [
        (
            "pnpm",
            "10.12.1",
            "10.13.1",
            "sha256",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            "npm",
            "10.0.0",
            "11.0.0",
            "sha1",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
        ),
        (
            "yarn",
            "1.22.20",
            "1.22.22",
            "sha512",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
        (
            "yarn",
            "4.0.0",
            "4.1.0",
            "sha224",
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
        ),
    ] {
        let tmp = TempDir::new().unwrap();
        let original = format!(
            "{{\r\n  \"packageManager\": \"{manager}@{old}+{algo}.oldhash\",\r\n  \"engines\": {{\"node\":\">=20\"}}\r\n}}\r\n"
        );
        write(tmp.path(), "package.json", &original);
        let cli = Cli::parse_from(["dcu", "--manifest", "package.json"]);
        assert!(run_at(&cli, tmp.path(), &registry, false).await.unwrap());
        assert_eq!(read(tmp.path(), "package.json"), original);
        let cli = Cli::parse_from(["dcu", "-u", "--manifest", "package.json"]);
        assert!(run_at(&cli, tmp.path(), &registry, false).await.unwrap());
        assert_eq!(
            read(tmp.path(), "package.json"),
            original.replace(
                &format!("{old}+{algo}.oldhash"),
                &format!("{latest}+{algo}.{digest}")
            )
        );
    }
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"packageManager":"bun@1.1.0"}"#,
    );
    assert!(
        run_at(
            &Cli::parse_from(["dcu", "-u"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert!(read(tmp.path(), "package.json").contains("bun@1.2.7"));
}

#[tokio::test]
async fn plugin_markers_groovy_and_sdk_variable_sources() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    metadata(
        &server,
        "/example/plugin/example.plugin.gradle.plugin/maven-metadata.xml",
        "<metadata><version>1.0.0</version><version>1.1.0</version></metadata>",
    )
    .await;
    write(
        tmp.path(),
        "build.gradle",
        &format!(
            "repositories {{ maven {{ url '{}' }} }}\r\nplugins {{ id 'example.plugin' version '1.0.0' }}\r\nval sdkVersion = 35\r\nandroid {{ compileSdk = sdkVersion\r\n targetSdkVersion 35\r\n minSdkVersion 24\r\n}}\r\n",
            server.uri()
        ),
    );
    assert!(
        run_at(
            &Cli::parse_from(["dcu", "-u"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    let text = read(tmp.path(), "build.gradle");
    assert!(text.contains("version '1.1.0'"));
    assert!(text.contains("val sdkVersion = 36"));
    assert!(text.contains("compileSdk = sdkVersion"));
    assert!(text.contains("targetSdkVersion 36"));
    assert!(text.contains("minSdkVersion 24"));
}

#[tokio::test]
async fn shared_catalog_versions_do_not_modify_filtered_consumers() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}", server.uri()),
    );
    let catalog = "[versions]\nshared = '1.6.1' # shared source\n[libraries]\nwebkit = { module = 'androidx.webkit:webkit', version.ref = 'shared' }\nappcompat = { module = 'androidx.appcompat:appcompat', version.ref = 'shared' }\n";
    write(tmp.path(), "gradle/libs.versions.toml", catalog);
    assert!(
        !run_at(
            &Cli::parse_from(["dcu", "-u", "webkit"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), "gradle/libs.versions.toml"), catalog);
    assert!(
        !run_at(
            &Cli::parse_from(["dcu", "-u"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), "gradle/libs.versions.toml"), catalog);
}

#[tokio::test]
async fn explicit_build_manifest_updates_catalog_alias_source_only() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!(
            "dependencyResolutionManagement {{ repositories {{ maven(\"{}\") }} }}\n",
            server.uri()
        ),
    );
    let catalog = "[versions]\nwebkit = '1.6.1' # original source\n[libraries]\nandroid-webkit = { module = 'androidx.webkit:webkit', version.ref = 'webkit' }\nother = { module = 'androidx.appcompat:appcompat', version = '1.6.1' }\n[plugins]\nkotlin-android = { id = 'org.jetbrains.kotlin.android', version = '1.9.25' }\n";
    write(tmp.path(), "gradle/libs.versions.toml", catalog);
    let build = "plugins {\n    alias(libs.plugins.kotlin.android)\n}\ndependencies {\n    implementation(libs.android.webkit)\n    // implementation(libs.other)\n}\n";
    write(tmp.path(), "app/build.gradle.kts", build);
    let manifests = [dependency_check_updates_core::ManifestRef {
        path: tmp.path().join("app/build.gradle.kts"),
        kind: ManifestKind::Gradle,
    }];
    let docs = project::load(&manifests, tmp.path()).unwrap();
    assert!(
        docs[&tmp.path().join("app/build.gradle.kts")]
            .applied_plugins
            .contains(&"org.jetbrains.kotlin.android".to_owned())
    );
    let catalog_doc = &docs[&tmp.path().join("gradle/libs.versions.toml")];
    assert!(
        catalog_doc
            .dependencies()
            .iter()
            .any(|d| d.name == "androidx.webkit:webkit")
    );
    assert!(
        !catalog_doc
            .dependencies()
            .iter()
            .any(|d| d.name == "androidx.appcompat:appcompat")
    );
    run_at(
        &Cli::parse_from([
            "dcu",
            "-u",
            "--manifest",
            "app/build.gradle.kts",
            "androidx.webkit:webkit",
        ]),
        tmp.path(),
        &registry,
        false,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), "app/build.gradle.kts"), build);
    assert_eq!(
        read(tmp.path(), "gradle/libs.versions.toml"),
        catalog.replace("webkit = '1.6.1'", "webkit = '1.12.1'")
    );

    write(
        tmp.path(),
        "app/build.gradle.kts",
        "dependencies {\n    implementation(libs.missing)\n    implementation(libs.other.get())\n}\n",
    );
    let docs = project::load(&manifests, tmp.path()).unwrap();
    let entries = &docs[&tmp.path().join("app/build.gradle.kts")].entries;
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.reason.is_some()));
}

#[tokio::test]
async fn strict_incomplete_aborts_entire_batch_and_cleanup_but_strict_compatibility_is_scoped() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "package.json", "{}");
    write(tmp.path(), "package-lock.json", "keep lock");
    write(tmp.path(), "node_modules/keep", "keep install");
    write(tmp.path(), ".nvmrc", "20.1.0\r\n");
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    let build = "classpath(\"com.android.tools.build:gradle:8.5.1\")\nimplementation(\"androidx.webkit:webkit:1.6.1\")\n";
    let wrapper =
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\n";
    write(tmp.path(), "build.gradle.kts", build);
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        wrapper,
    );
    let cli = Cli::parse_from([
        "dcu",
        "-d",
        "-u",
        "--rm",
        "--fail-on-incomplete",
        "--format",
        "json-report",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 2);
    assert!(matches!(
        report.outcome,
        crate::report::ApplyOutcome::Aborted
    ));
    assert_eq!(report.summary().updated, 0);
    assert_eq!(read(tmp.path(), "build.gradle.kts"), build);
    assert_eq!(
        read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties"),
        wrapper
    );
    assert_eq!(read(tmp.path(), ".nvmrc"), "20.1.0\r\n");
    assert_eq!(read(tmp.path(), "package-lock.json"), "keep lock");
    assert!(tmp.path().join("node_modules/keep").exists());
    let cli = Cli::parse_from([
        "dcu",
        "-d",
        "-u",
        "--strict-compatibility",
        "--format",
        "json-report",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert!(matches!(
        report.outcome,
        crate::report::ApplyOutcome::Committed
    ));
    assert_eq!(
        read(tmp.path(), "build.gradle.kts"),
        build.replace("webkit:1.6.1", "webkit:1.12.1")
    );
    assert_eq!(
        read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties"),
        wrapper
    );
    assert_eq!(read(tmp.path(), ".nvmrc"), "22.15.1\r\n");
    let rows = report.json(crate::OutputFormat::JsonReport).unwrap();
    let agp = rows["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "com.android.tools.build:gradle")
        .unwrap();
    assert_eq!(agp["status"], "unverified");
    assert_eq!(agp["updated"], false);
    assert!(
        agp["reason"]
            .as_str()
            .unwrap()
            .contains("strict compatibility")
    );
}

#[tokio::test]
async fn explicit_gradle_manifest_uses_ancestor_jdk_without_updating_the_pin() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), ".tool-versions", "java 17\n");
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.13-bin.zip\n",
    );
    write(
        tmp.path(),
        "android/settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    let build = "classpath(\"com.android.tools.build:gradle:8.5.1\")\n";
    write(tmp.path(), "android/app/build.gradle.kts", build);
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--strict-compatibility",
        "--manifest",
        "android/app/build.gradle.kts",
        "--format",
        "json-report",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 1);
    assert_eq!(report.summary().incomplete, 0);
    assert_eq!(read(tmp.path(), ".tool-versions"), "java 17\n");
    assert_eq!(
        read(tmp.path(), "android/app/build.gradle.kts"),
        build.replace("8.5.1", "8.10.1")
    );
}

#[tokio::test]
async fn targeted_context_does_not_read_unrelated_catalogs_or_inherit_their_repositories() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        "repositories { maven(\"https://private.invalid/root\") }\n",
    );
    write(
        tmp.path(),
        "apps/a/settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    write(
        tmp.path(),
        "apps/a/app/build.gradle.kts",
        "implementation(\"androidx.webkit:webkit:1.6.1\")\n",
    );
    write(
        tmp.path(),
        "apps/b/settings.gradle.kts",
        "repositories { maven(\"https://private.invalid/b\") }\n",
    );
    write(
        tmp.path(),
        "apps/b/gradle/libs.versions.toml",
        "[not valid TOML",
    );
    write(
        tmp.path(),
        "apps/b/build.gradle.kts",
        "classpath(\"com.android.tools.build:gradle:99.0.0\")\n",
    );
    let manifest = Scanner::from_path(&tmp.path().join("apps/a/app/build.gradle.kts")).unwrap();
    let docs = project::load(&[manifest], tmp.path()).unwrap();
    assert!(
        docs.keys()
            .all(|p| !p.starts_with(tmp.path().join("apps/b")))
    );
    let entry = &docs[&tmp.path().join("apps/a/app/build.gradle.kts")].entries[0];
    assert_eq!(entry.repositories, vec![server.uri()]);
    let cli = Cli::parse_from(["dcu", "-u", "--manifest", "apps/a/app/build.gradle.kts"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 1);
    assert_eq!(
        read(tmp.path(), "apps/b/gradle/libs.versions.toml"),
        "[not valid TOML"
    );
    write(tmp.path(), "package.json", "{}");
    let docs = project::load(
        &[Scanner::from_path(&tmp.path().join("package.json")).unwrap()],
        tmp.path(),
    )
    .unwrap();
    assert_eq!(docs.len(), 1); // No Gradle context traversal for a Node-only lookup.
}

#[tokio::test]
async fn shared_ancestor_source_expands_consumers_without_parsing_unrelated_catalogs() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    let properties = "sharedVersion=1.6.1\n";
    write(tmp.path(), "gradle.properties", properties);
    for name in ["a", "b"] {
        write(
            tmp.path(),
            &format!("apps/{name}/settings.gradle.kts"),
            &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
        );
    }
    write(
        tmp.path(),
        "apps/a/build.gradle.kts",
        "implementation(\"androidx.webkit:webkit:$sharedVersion\")\n",
    );
    write(
        tmp.path(),
        "apps/b/build.gradle.kts",
        "implementation(\"androidx.appcompat:appcompat:$sharedVersion\")\n",
    );
    write(
        tmp.path(),
        "apps/b/gradle/libs.versions.toml",
        "[broken catalog",
    );
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--manifest",
        "apps/a/build.gradle.kts",
        "webkit",
        "--format",
        "json-report",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 0);
    assert_eq!(read(tmp.path(), "gradle.properties"), properties);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].status(), crate::report::Status::Blocked);
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--manifest",
        "apps/a/build.gradle.kts",
        "webkit",
        "--format",
        "json-legacy",
    ]);
    assert!(
        execute_at(&cli, tmp.path(), &registry, false)
            .await
            .is_err()
    );
    assert_eq!(read(tmp.path(), "gradle.properties"), properties);
}

#[tokio::test]
async fn duplicate_metadata_is_shared_but_targets_and_executions_are_independent() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    write(
        tmp.path(),
        "build.gradle.kts",
        "classpath(\"com.android.tools.build:gradle:8.5.1\")\nclasspath(\"com.android.tools.build:gradle:8.5.2\")\n",
    );
    write(
        tmp.path(),
        "buildSrc/build.gradle.kts",
        "implementation(\"com.android.tools.build:gradle:8.5.1\")\n",
    );
    write(
        tmp.path(),
        "package.json",
        r#"{"packageManager":"pnpm@10.12.1","dependencies":{"pnpm":"10.12.1"}}"#,
    );
    write(tmp.path(), ".nvmrc", "20.1.0\n");
    write(tmp.path(), ".node-version", "20.1.0\n");
    let cli = Cli::parse_from(["dcu", "-d", "-t", "patch", "--format", "json-report"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    let json = report.json(crate::OutputFormat::Json).unwrap();
    let rows = json.as_array().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|r| r["name"] == "com.android.tools.build:gradle")
            .count(),
        3
    );
    assert!(
        rows.iter()
            .filter(|r| r["name"] == "com.android.tools.build:gradle")
            .all(|r| r["selected"] == "8.5.2")
    );
    let requests = server.received_requests().await.unwrap();
    for path in [
        "/com/android/tools/build/gradle/maven-metadata.xml",
        "/pnpm",
        "/node",
    ] {
        assert_eq!(
            requests.iter().filter(|r| r.url.path() == path).count(),
            1,
            "{path}"
        );
    }
    let cli = Cli::parse_from(["dcu", "-d", "--format", "json-report"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    let json = report.json(crate::OutputFormat::Json).unwrap();
    assert!(
        json.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["name"] == "com.android.tools.build:gradle")
            .all(|r| r["selected"] == "8.10.1")
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/com/android/tools/build/gradle/maven-metadata.xml")
            .count(),
        2
    );
}

#[test]
fn dynamic_local_assignment_shadows_ancestor_pin_and_imported_builds_are_unverified() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "gradle.properties", "webkitVersion=1.6.1\n");
    write(
        tmp.path(),
        "build.gradle.kts",
        "val webkitVersion = lookupVersion()\nimplementation(\"androidx.webkit:webkit:$webkitVersion\")\n",
    );
    let docs = project::load(
        &[Scanner::from_path(&tmp.path().join("build.gradle.kts")).unwrap()],
        tmp.path(),
    )
    .unwrap();
    let doc = &docs[&tmp.path().join("build.gradle.kts")];
    assert!(
        doc.entries[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("unresolved")
    );
    assert!(
        docs[&tmp.path().join("gradle.properties")]
            .entries
            .is_empty()
    );
    let doc = project::parse(
        "includeBuild(projectPath())\n",
        Path::new("settings.gradle.kts"),
    )
    .unwrap();
    assert!(!doc.context_issues.is_empty());
    assert!(doc.entries[0].reason.is_some());
}

#[tokio::test]
async fn same_artifact_in_nonselected_shared_property_consumers_is_preserved() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "gradle.properties", "webkitVersion=1.6.1\n");
    for name in ["a", "b"] {
        write(
            tmp.path(),
            &format!("apps/{name}/settings.gradle.kts"),
            &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
        );
        write(
            tmp.path(),
            &format!("apps/{name}/build.gradle.kts"),
            "implementation(\"androidx.webkit:webkit:$webkitVersion\")\n",
        );
    }
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--manifest",
        "apps/a/build.gradle.kts",
        "--format",
        "json-report",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 0);
    assert_eq!(report.items[0].status(), crate::report::Status::Blocked);
    assert_eq!(
        read(tmp.path(), "gradle.properties"),
        "webkitVersion=1.6.1\n"
    );
}

#[tokio::test]
async fn catalog_alias_uses_consumer_repositories_and_keeps_nonselected_uses() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        "repositories { maven(\"https://private.invalid/root\") }\n",
    );
    write(
        tmp.path(),
        "android/settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    let catalog = "[versions]\nwebkit = '1.6.1'\n[libraries]\nwebkit = { module = 'androidx.webkit:webkit', version.ref = 'webkit' }\n";
    write(tmp.path(), "gradle/libs.versions.toml", catalog);
    write(
        tmp.path(),
        "android/a/build.gradle.kts",
        "implementation(libs.webkit)\n",
    );
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--manifest",
        "android/a/build.gradle.kts",
        "--format",
        "json-report",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 1);
    assert_eq!(
        read(tmp.path(), "gradle/libs.versions.toml"),
        catalog.replace("1.6.1", "1.12.1")
    );
    write(tmp.path(), "gradle/libs.versions.toml", catalog);
    write(
        tmp.path(),
        "android/b/build.gradle.kts",
        "implementation(libs.webkit)\n",
    );
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 0);
    assert_eq!(report.items[0].status(), crate::report::Status::Blocked);
    assert_eq!(read(tmp.path(), "gradle/libs.versions.toml"), catalog);
}

#[tokio::test]
async fn shared_alias_lookup_failure_blocks_successful_consumers_of_the_same_source() {
    let (server, registry) = registry().await;
    let failed = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&failed)
        .await;
    let tmp = TempDir::new().unwrap();
    let catalog =
        "[libraries]\nwebkit = { module = 'androidx.webkit:webkit', version = '1.6.1' }\n";
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    write(tmp.path(), "gradle/libs.versions.toml", catalog);
    for (name, url) in [("a", server.uri()), ("b", failed.uri())] {
        write(
            tmp.path(),
            &format!("apps/{name}/settings.gradle.kts"),
            &format!("repositories {{ maven(\"{url}\") }}\n"),
        );
        write(
            tmp.path(),
            &format!("apps/{name}/build.gradle.kts"),
            "implementation(libs.webkit)\n",
        );
    }
    let cli = Cli::parse_from(["dcu", "-d", "-u", "--format", "json-report"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.summary().updated, 0);
    assert_eq!(read(tmp.path(), "gradle/libs.versions.toml"), catalog);
    assert!(
        report
            .items
            .iter()
            .any(|r| r.status() == crate::report::Status::Blocked)
    );
    let rows = report.json(crate::OutputFormat::Json).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["reason"].as_str().is_some_and(|s| s.contains("503")))
    );
}

#[test]
fn ordinary_npm_tool_names_do_not_gain_tool_compatibility() {
    use dependency_check_updates_core::{ManifestHandler, ResolvedVersion};
    let handler = dependency_check_updates_node::NodeHandler;
    let text = r#"{"dependencies":{"jdk":"1.0.0","gradle":"1.0.0"}}"#;
    let path = PathBuf::from("package.json");
    let deps = handler.parse(text, &path).unwrap().dependencies;
    let job = ManifestJob {
        manifest_ref: dependency_check_updates_core::ManifestRef {
            path,
            kind: ManifestKind::PackageJson,
        },
        display_path: "package.json".into(),
        text: text.into(),
        handler: Box::new(handler),
        deps,
        document: None,
    };
    let resolved = vec![
        (
            0,
            Ok(ResolvedVersion {
                latest: Some("1.0.0".into()),
                selected: Some("1.0.0".into()),
            }),
        ),
        (
            1,
            Ok(ResolvedVersion {
                latest: Some("1.0.0".into()),
                selected: Some("1.0.0".into()),
            }),
        ),
    ];
    let rows = report_rows(
        &job,
        &resolved,
        &[],
        Some("unverified: missing JDK"),
        None,
        false,
    );
    assert!(
        rows.iter()
            .all(|r| r.status == crate::report::Status::Current
                && r.compatibility.is_none()
                && r.selection_policy.is_none())
    );
}

#[tokio::test]
async fn failed_checksum_revalidates_agp_before_any_write() {
    let (server, registry) = registry().await;
    let tmp = fixture(&server);
    Mock::given(path("/distributions/gradle-8.13-bin.zip.sha256"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&server)
        .await;
    let build = format!("{ANDROID}/build.gradle.kts");
    let before = read(tmp.path(), &build);
    run_at(
        &Cli::parse_from(["dcu", "-d", "-u"]),
        tmp.path(),
        &registry,
        false,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), &build), before);
    assert!(
        read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties").contains("gradle-8.9-bin.zip")
    );
}

#[tokio::test]
async fn agp9_requires_explicit_kotlin_android_migration() {
    let (server, registry) = registry().await;
    let tmp = fixture(&server);
    Mock::given(path("/com/android/tools/build/gradle/maven-metadata.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<metadata><version>9.0.1</version></metadata>"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    metadata(
        &server,
        "/gradle9",
        r#"[{"version":"9.1.0","snapshot":false}]"#,
    )
    .await;
    let mut registry = registry;
    registry.endpoints.gradle = format!("{}/gradle9", server.uri());
    let app = format!("{ANDROID}/app/build.gradle.kts");
    write(
        tmp.path(),
        &app,
        &format!(
            "plugins {{ id(\"org.jetbrains.kotlin.android\") }}\r\n{}",
            read(tmp.path(), &app)
        ),
    );
    let build = format!("{ANDROID}/build.gradle.kts");
    let before = read(tmp.path(), &build);
    run_at(
        &Cli::parse_from(["dcu", "-d", "-u"]),
        tmp.path(),
        &registry,
        false,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), &build), before);
}

#[test]
fn scanner_preserves_ignore_and_build_directory_rules() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    write(
        tmp.path(),
        ".gitignore",
        "ignored/\nignored-project/.nvmrc\n",
    );
    for p in [
        ".nvmrc",
        "apps/app/.node-version",
        "apps/app/.mise.toml",
        "ignored/build.gradle.kts",
        "ignored-project/.nvmrc",
        "node_modules/tool/.nvmrc",
        "target/build.gradle.kts",
        "build/build.gradle.kts",
        ".secret/.nvmrc",
    ] {
        write(tmp.path(), p, "20\n");
    }
    let found = Scanner::scan_deep(tmp.path());
    assert_eq!(
        found.len(),
        3,
        "{:?}",
        found.iter().map(|m| &m.path).collect::<Vec<_>>()
    );
}

#[test]
fn tool_syntax_preserves_channels_ranges_and_vendor_prefixes() {
    let text = "[tools]\nnode = 'lts/jod'\nrust = 'stable'\npnpm = '>=10 <11'\njava = 'temurin-17.0.1+9'\n";
    let doc = project::parse(text, Path::new("mise.toml")).unwrap();
    assert_eq!(doc.entries.len(), 4);
    assert_eq!(doc.entries[0].dep.current_req, "lts/jod");
    assert!(doc.entries[2].reason.is_some());
    let update = dependency_check_updates_core::PlannedUpdate {
        name: "jdk".into(),
        section: DependencySection::Toolchain,
        from: "17.0.1+9".into(),
        to: "21.0.2+13".into(),
    };
    assert_eq!(
        doc.apply(text, &[update], vec![]).unwrap(),
        text.replace("temurin-17.0.1+9", "temurin-21.0.2+13")
    );
    let package = r#"{"nested":{"packageManager":"pnpm@10.12.1"},"packageManager":"pnpm@10.12.1"}"#;
    let doc = project::parse(package, Path::new("package.json")).unwrap();
    let update = dependency_check_updates_core::PlannedUpdate {
        name: "pnpm".into(),
        section: DependencySection::Toolchain,
        from: "10.12.1".into(),
        to: "10.13.1".into(),
    };
    assert_eq!(
        doc.apply(package, &[update], vec![]).unwrap(),
        r#"{"nested":{"packageManager":"pnpm@10.12.1"},"packageManager":"pnpm@10.13.1"}"#
    );
}

#[tokio::test]
async fn shared_sdk_source_cannot_raise_min_sdk_and_reassignments_are_unsupported() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    let script =
        "val sdk = 24\r\nandroid {\r\ncompileSdk = sdk\r\ntargetSdk = sdk\r\nminSdk = sdk\r\n}\r\n";
    write(tmp.path(), "build.gradle.kts", script);
    assert!(
        !run_at(
            &Cli::parse_from(["dcu", "-u"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), "build.gradle.kts"), script);
    let script = format!(
        "repositories {{ maven(\"{}\") }}\nvar version = \"1.6.1\"\nversion = calculateVersion()\nimplementation(\"androidx.webkit:webkit:$version\")\n",
        server.uri()
    );
    write(tmp.path(), "build.gradle.kts", &script);
    assert!(
        !run_at(
            &Cli::parse_from(["dcu", "-u"]),
            tmp.path(),
            &registry,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(read(tmp.path(), "build.gradle.kts"), script);
}

#[test]
fn toml_literal_quotes_and_escaped_values_remain_safe() {
    let text =
        "[tools]\nnode = '''20.1.0''' # Keep three literal quotes\nrust = \"1\\u002e85.0\"\n";
    let doc = project::parse(text, Path::new("mise.toml")).unwrap();
    assert!(doc.entries[1].reason.is_some());
    let update = dependency_check_updates_core::PlannedUpdate {
        name: "node".into(),
        section: DependencySection::Toolchain,
        from: "20.1.0".into(),
        to: "22.1.0".into(),
    };
    assert_eq!(
        doc.apply(text, &[update], vec![]).unwrap(),
        text.replace("'''20.1.0'''", "'''22.1.0'''")
    );
}

#[test]
fn properties_without_artifact_context_are_reported_as_unsupported() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "gradle.properties",
        "webkitVersion=1.6.1\nminSdkVersion=24\norg.gradle.jvmargs=-Xmx2g\n",
    );
    let manifests = Scanner::scan_dir(tmp.path());
    let docs = project::load(&manifests, tmp.path()).unwrap();
    let doc = &docs[&tmp.path().join("gradle.properties")];
    assert_eq!(doc.dependencies().len(), 1);
    assert!(
        doc.entries[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("no statically identified artifact")
    );
}

#[tokio::test]
async fn nested_sdk_conflict_blocks_ancestor_version_sources_and_tool_pins() {
    let (server, registry) = registry().await;
    for catalog in [false, true] {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), ".tool-versions", "java 16\n");
        let wrapper =
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\n";
        write(
            tmp.path(),
            "gradle/wrapper/gradle-wrapper.properties",
            wrapper,
        );
        write(
            tmp.path(),
            "settings.gradle.kts",
            &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
        );
        write(
            tmp.path(),
            "android/settings.gradle.kts",
            &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
        );
        let (source_path, source, declaration, name) = if catalog {
            (
                "gradle/libs.versions.toml",
                "[versions]\nagp = '8.5.1'\n[plugins]\nandroid = { id = 'com.android.application', version.ref = 'agp' }\n",
                "plugins {\n    alias(libs.plugins.android)\n}\n",
                "com.android.application",
            )
        } else {
            (
                "gradle.properties",
                "agpVersion=8.5.1 # original\n",
                "classpath(\"com.android.tools.build:gradle:$agpVersion\")\n",
                "com.android.tools.build:gradle",
            )
        };
        write(tmp.path(), source_path, source);
        let build = format!("{declaration}android {{\n    compileSdk = 37\n}}\n");
        write(tmp.path(), "android/app/build.gradle.kts", &build);
        let cli = Cli::parse_from(["dcu", "-d", "-u", "--format", "json-report"]);
        let report = execute_at(&cli, tmp.path(), &registry, false)
            .await
            .unwrap();
        assert_eq!(read(tmp.path(), source_path), source);
        assert_eq!(read(tmp.path(), "android/app/build.gradle.kts"), build);
        assert_eq!(read(tmp.path(), ".tool-versions"), "java 16\n");
        assert_eq!(
            read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties"),
            wrapper
        );
        let json = report.json(crate::OutputFormat::JsonReport).unwrap();
        for dependency in [name, "gradle", "jdk"] {
            let row = json["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == dependency)
                .unwrap();
            assert_eq!(row["status"], "blocked", "{row}");
            assert_eq!(row["to"], serde_json::Value::Null);
            assert_eq!(row["updated"], false);
            assert!(row["reason"].as_str().unwrap().contains("conflict:"));
        }
        assert_eq!(report.summary().updated, 0);
    }
}

#[tokio::test]
async fn standalone_jdk_and_plain_gradle_do_not_require_android_pins() {
    let (_server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), ".tool-versions", "java 16\n");
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--fail-on-incomplete",
        "--strict-compatibility",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert_eq!(report.summary().updated, 1);
    assert_eq!(report.summary().incomplete, 0);
    assert_eq!(read(tmp.path(), ".tool-versions"), "java 17\n");
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.9-bin.zip\n",
    );
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert_eq!(report.summary().updated, 1);
    assert_eq!(report.summary().incomplete, 0);
    assert!(
        read(tmp.path(), "gradle/wrapper/gradle-wrapper.properties")
            .contains("gradle-8.13-bin.zip")
    );
}

#[tokio::test]
async fn unsupported_build_connections_remain_unverified_with_known_tool_pins() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), ".tool-versions", "java 17\n");
    let wrapper =
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.13-bin.zip\n";
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        wrapper,
    );
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!(
            "includeBuild(\"../conventions\")\nrepositories {{ maven(\"{}\") }}\n",
            server.uri()
        ),
    );
    let build = "classpath(\"com.android.tools.build:gradle:8.5.1\")\nimplementation(\"androidx.webkit:webkit:1.6.1\")\n";
    write(tmp.path(), "build.gradle.kts", build);
    let cli = Cli::parse_from(["dcu", "-u", "--strict-compatibility"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert!(report.summary().incomplete > 0);
    assert_eq!(
        read(tmp.path(), "build.gradle.kts"),
        build.replace("webkit:1.6.1", "webkit:1.12.1")
    );
    let json = report.json(crate::OutputFormat::JsonReport).unwrap();
    let agp = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "com.android.tools.build:gradle")
        .unwrap();
    assert!(
        agp["reason"]
            .as_str()
            .unwrap()
            .contains("strict compatibility blocks")
    );
}

#[tokio::test]
async fn deep_catalog_lookup_uses_consumer_repository_not_an_unused_source_context() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        "// no repositories at the catalog source\n",
    );
    write(
        tmp.path(),
        "gradle/libs.versions.toml",
        "[libraries]\nwebkit = { module = 'androidx.webkit:webkit', version = '1.6.1' }\n",
    );
    write(
        tmp.path(),
        "android/settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    let build = "dependencies {\n    implementation(libs.webkit)\n}\n";
    write(tmp.path(), "android/app/build.gradle.kts", build);
    let cli = Cli::parse_from(["dcu", "-d", "-u", "--fail-on-incomplete"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert_eq!(report.summary().updated, 1);
    assert_eq!(report.summary().incomplete, 0);
    assert!(read(tmp.path(), "gradle/libs.versions.toml").contains("version = '1.12.1'"));
    assert_eq!(read(tmp.path(), "android/app/build.gradle.kts"), build);
}

#[tokio::test]
async fn typed_extra_and_literal_provider_bindings_patch_only_source_spans() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\r\n", server.uri()),
    );
    let properties = "webkitVersion=1.6.1 # source\r\n";
    write(tmp.path(), "gradle.properties", properties);
    let build = "val materialVersion: String = \"1.8.0\" // typed\r\nval appcompatVersion: String by extra(\"1.6.1\")\r\nval webkit: String = providers.gradleProperty(\"webkitVersion\").get()\r\nimplementation(\"com.google.android.material:material:$materialVersion\")\r\nimplementation(\"androidx.appcompat:appcompat:$appcompatVersion\")\r\nimplementation(\"androidx.webkit:webkit:$webkit\")\r\n// val fake by extra(\"1.0\")\r\n";
    write(tmp.path(), "app/build.gradle.kts", build);
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--manifest",
        "app/build.gradle.kts",
        "--fail-on-incomplete",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert_eq!(
        read(tmp.path(), "gradle.properties"),
        properties.replace("1.6.1", "1.12.1")
    );
    assert_eq!(
        read(tmp.path(), "app/build.gradle.kts"),
        build
            .replace(
                "materialVersion: String = \"1.8.0\"",
                "materialVersion: String = \"1.12.0\""
            )
            .replace(
                "appcompatVersion: String by extra(\"1.6.1\")",
                "appcompatVersion: String by extra(\"1.7.0\")"
            )
    );
    for binding in [
        "val webkit: String = providers.gradleProperty(\"webkitVersion\").map { transform(it) }.get()\n",
        "val webkit: String = providers.gradleProperty(\"webkitVersion\").get()\nwebkit = calculateVersion()\n",
    ] {
        let build = format!("{binding}implementation(\"androidx.webkit:webkit:$webkit\")\n");
        write(tmp.path(), "app/build.gradle.kts", &build);
        write(tmp.path(), "gradle.properties", properties);
        let report = execute_at(&cli, tmp.path(), &registry, false)
            .await
            .unwrap();
        assert_eq!(report.exit_code(&cli), 2);
        assert_eq!(read(tmp.path(), "app/build.gradle.kts"), build);
        assert_eq!(read(tmp.path(), "gradle.properties"), properties);
    }
}

#[tokio::test]
async fn explicit_maven_access_authenticates_and_rejects_redirects_without_secret_leaks() {
    use reqwest::header::{AUTHORIZATION, HeaderMap};
    use wiremock::matchers::header;
    let (server, mut registry) = registry().await;
    let destination = MockServer::start().await;
    let repo = format!("{}/private", server.uri());
    Mock::given(method("GET")).and(path("/private/org/example/library/maven-metadata.xml"))
        .and(header("authorization", "Bearer secret-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<metadata><versioning><versions><version>1.0.0</version><version>1.1.0</version></versions></versioning></metadata>"))
        .expect(1).mount(&server).await;
    let mut headers = HeaderMap::new();
    let mut value = "Bearer secret-test"
        .parse::<reqwest::header::HeaderValue>()
        .unwrap();
    value.set_sensitive(true);
    headers.insert(AUTHORIZATION, value);
    registry.private_repositories.insert(repo.clone(), headers);
    registry.private_cache =
        Some(dependency_check_updates_core::MetadataCache::without_redirects());
    let build = format!(
        "repositories {{ maven(\"{repo}\") }}\nimplementation(\"org.example:library:1.0.0\")\n"
    );
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "build.gradle.kts", &build);
    let cli = Cli::parse_from(["dcu", "-u", "--fail-on-incomplete"]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert!(read(tmp.path(), "build.gradle.kts").contains("library:1.1.0"));
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/private/org/example/library/maven-metadata.xml"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/leak", destination.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/leak"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&destination)
        .await;
    write(tmp.path(), "build.gradle.kts", &build);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 2);
    let json = report
        .json(crate::OutputFormat::JsonReport)
        .unwrap()
        .to_string();
    assert!(!json.contains("secret-test"));
    assert!(json.contains("HTTP 302"));
    assert_eq!(read(tmp.path(), "build.gradle.kts"), build);
    destination.verify().await;
}

#[tokio::test]
async fn compatibility_extensions_verify_new_combinations_without_replacing_builtins() {
    let (server, registry) = registry().await;
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), ".tool-versions", "java 17\n");
    write(
        tmp.path(),
        "gradle/wrapper/gradle-wrapper.properties",
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.13-bin.zip\n",
    );
    write(
        tmp.path(),
        "settings.gradle.kts",
        &format!("repositories {{ maven(\"{}\") }}\n", server.uri()),
    );
    let build = "classpath(\"com.android.tools.build:gradle:99.0.0\")\n";
    write(tmp.path(), "build.gradle.kts", build);
    Mock::given(method("GET")).and(path("/com/android/tools/build/gradle/maven-metadata.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<metadata><versioning><versions><version>99.0.1</version></versions></versioning></metadata>"))
        .with_priority(1).mount(&server).await;
    write(
        tmp.path(),
        "rules.json",
        r#"{"schemaVersion":1,"verifiedAt":"2026-10-01","sources":["https://developer.android.com/build/releases/about-agp"],"agp":{"99.0":{"minGradle":"8.13","minJdk":17}}}"#,
    );
    let cli = Cli::parse_from([
        "dcu",
        "-u",
        "--strict-compatibility",
        "--fail-on-incomplete",
        "--compatibility-file",
        "rules.json",
    ]);
    let report = execute_at(&cli, tmp.path(), &registry, false)
        .await
        .unwrap();
    assert_eq!(report.exit_code(&cli), 0);
    assert_eq!(
        read(tmp.path(), "build.gradle.kts"),
        build.replace("99.0.0", "99.0.1")
    );
}

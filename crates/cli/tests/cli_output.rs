use std::process::Command;
use tempfile::TempDir;

#[test]
fn traversal_and_cleanup_failures_are_in_json_and_exit_nonzero() {
    for scan_error in [true, false] {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("package.json"), "{}").unwrap();
        let mut args = vec!["--format", "json-report", "--fail-on-incomplete"];
        if scan_error {
            std::fs::write(root.path().join(".ignore"), "[z-a]\n").unwrap();
            args.push("-d");
        } else {
            std::fs::create_dir(root.path().join("package-lock.json")).unwrap();
            args.push("--rm");
        }
        let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
            .current_dir(root.path())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let diagnostic = &report["diagnostics"][0];
        assert_eq!(
            diagnostic["code"],
            if scan_error {
                "execution-failed"
            } else {
                "cleanup-failed"
            }
        );
        if scan_error {
            assert!(diagnostic["message"].as_str().unwrap().contains("range"));
        } else {
            assert!(
                diagnostic["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("package-lock.json")
            );
        }
    }
}

#[test]
fn json_stdout_is_one_document_and_reports_unsupported_declarations() {
    let root = TempDir::new().unwrap();
    let script =
        "// implementation(\"comment:fake:1.0\")\r\nimplementation(\"g:a:${getVersion()}\")\r\n";
    let file = root.path().join("build.gradle.kts");
    std::fs::write(&file, script).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args(["--manifest", "build.gradle.kts", "--format", "json", "-v"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], "g:a");
    assert_eq!(rows[0]["status"], "unsupported");
    assert!(rows[0]["latest"].is_null());
    assert_eq!(std::fs::read_to_string(file).unwrap(), script);
}

#[test]
fn local_tool_mode_cannot_be_combined_with_project_mutations() {
    let result = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .args(["--local-tools", "-u"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("cannot be used"));
}

#[test]
fn filtered_local_mode_needs_no_project_and_emits_json() {
    let root = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args([
            "--local-tools",
            "node",
            "--reject",
            "node",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn recovery_cli_uses_no_registry_and_reports_committed_or_restored_outcome() {
    use sha2::{Digest, Sha256};
    for (mode, outcome, expected) in [
        ("rollback", "rolled-back", "old\r\n"),
        ("finish", "committed", "new\r\n"),
    ] {
        let root = TempDir::new().unwrap();
        let target = root.path().join("build.gradle.kts");
        let backup = root.path().join(".dcu-backup-test.tmp");
        let stage = root.path().join(".dcu-stage-test.tmp");
        let receipt = root.path().join(".dcu-transaction-active.json");
        std::fs::write(&target, "new\r\n").unwrap();
        std::fs::write(&backup, "old\r\n").unwrap();
        std::fs::write(&stage, "new\r\n").unwrap();
        let journal = serde_json::json!({"schemaVersion":1,"files":[{
            "path":target,"backup":backup,"staged":stage,
            "original_sha256":format!("{:x}", Sha256::digest(b"old\r\n")),
            "replacement_sha256":format!("{:x}", Sha256::digest(b"new\r\n"))
        }]});
        std::fs::write(&receipt, serde_json::to_vec(&journal).unwrap()).unwrap();
        let run = || {
            Command::new(env!("CARGO_BIN_EXE_dcu"))
                .current_dir(root.path())
                .args(["--recover", mode, "--format", "json-report"])
                .output()
                .unwrap()
        };
        let output = run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["applyOutcome"], outcome);
        assert_eq!(report["items"], serde_json::json!([]));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
        assert!(!receipt.exists());
        assert!(!backup.exists());
        assert!(!stage.exists());
        let again: serde_json::Value = serde_json::from_slice(&run().stdout).unwrap();
        assert_eq!(again["applyOutcome"], "no-changes");
    }
}

#[tokio::test]
async fn private_maven_config_is_explicit_and_credentials_are_child_process_scoped() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path},
    };
    let server = MockServer::start().await;
    Mock::given(path("/maven/org/example/library/maven-metadata.xml"))
        .and(header("authorization", "Bearer test-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<metadata><versioning><versions><version>1.0.0</version><version>1.1.0</version></versions></versioning></metadata>"))
        .expect(1).mount(&server).await;
    let root = TempDir::new().unwrap();
    let script = format!(
        "repositories {{ maven(\"{}/maven\") }}\nimplementation(\"org.example:library:1.0.0\")\n",
        server.uri()
    );
    let target = root.path().join("build.gradle.kts");
    std::fs::write(&target, &script).unwrap();
    std::fs::write(root.path().join("repositories.json"), serde_json::to_vec(&serde_json::json!({
        "schemaVersion":1, "repositories":[{"url":format!("{}/maven", server.uri()), "tokenEnv":"DCU_TEST_MAVEN_TOKEN"}]
    })).unwrap()).unwrap();
    let cwd = root.path().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_dcu"))
            .current_dir(cwd)
            .env("DCU_TEST_MAVEN_TOKEN", "test-secret")
            .args([
                "-u",
                "--maven-config",
                "repositories.json",
                "--fail-on-incomplete",
                "--format",
                "json-report",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("library:1.1.0")
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test-secret"));
    server.verify().await;
    std::fs::write(&target, &script).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args(["-u", "--fail-on-incomplete", "--format", "json-report"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), script);
    server.verify().await;
}

#[test]
fn incomplete_ci_exit_two_precedes_update_policy_and_prevents_cleanup() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("package.json"), "{}").unwrap();
    let script = "implementation(\"g:a:${lookup()}\")\r\n";
    std::fs::write(root.path().join("build.gradle.kts"), script).unwrap();
    std::fs::write(root.path().join("package-lock.json"), "keep").unwrap();
    std::fs::create_dir(root.path().join("node_modules")).unwrap();
    std::fs::write(root.path().join("node_modules/keep"), "keep").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args([
            "-d",
            "-u",
            "--rm",
            "--fail-on-incomplete",
            "-e",
            "2",
            "--format",
            "json-report",
            "-v",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schemaVersion"], 2);
    assert_eq!(json["applyOutcome"], "aborted");
    assert_eq!(json["summary"]["updated"], 0);
    assert_eq!(json["summary"]["incomplete"], 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("build.gradle.kts")).unwrap(),
        script
    );
    assert!(root.path().join("package-lock.json").exists());
    assert!(root.path().join("node_modules/keep").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args(["-d", "-e", "2", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0)); // Preserve the original -e 2 policy.
    assert!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .unwrap()
            .is_array()
    );
}

#[test]
fn legacy_is_one_object_and_rejects_multiple_manifests_before_mutations() {
    let root = TempDir::new().unwrap();
    std::fs::write(
        root.path().join("build.gradle.kts"),
        "implementation(\"g:a:${lookup()}\")\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args([
            "--manifest",
            "build.gradle.kts",
            "--format",
            "json-legacy",
            "-v",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({})
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
    std::fs::write(root.path().join("package.json"), "{}").unwrap();
    std::fs::write(root.path().join("package-lock.json"), "keep").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args(["-d", "-u", "--rm", "--format", "json-legacy"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("one effective project manifest"));
    assert!(root.path().join("package-lock.json").exists());
}

#[test]
fn pending_receipts_are_read_only_diagnostics_and_block_upgrade() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("package.json"), "{}").unwrap();
    let receipt = root.path().join(".dcu-transaction-active.json");
    std::fs::write(&receipt, "manual recovery needed").unwrap();
    for (flags, expected, outcome) in [
        (vec!["--format", "json-report"], 0, "not-requested"),
        (
            vec!["--format", "json-report", "--fail-on-incomplete"],
            2,
            "not-requested",
        ),
        (vec!["-u", "--format", "json-report"], 1, "aborted"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
            .current_dir(root.path())
            .args(flags)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["applyOutcome"], outcome);
        assert_eq!(report["diagnostics"][0]["code"], "pending-recovery");
        assert_eq!(
            std::fs::read_to_string(&receipt).unwrap(),
            "manual recovery needed"
        );
    }
}

#[test]
fn fatal_parse_errors_and_empty_local_reports_keep_the_versioned_contract() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("package.json"), "{bad JSON").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args(["--format", "json-report"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schemaVersion"], 2);
    assert_eq!(report["diagnostics"][0]["code"], "execution-failed");
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .current_dir(root.path())
        .args([
            "--local-tools",
            "node",
            "--reject",
            "node",
            "--fail-on-incomplete",
            "--format",
            "json-report",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["items"], serde_json::json!([]));
    assert_eq!(report["summary"]["checked"], 0);
    let output = Command::new(env!("CARGO_BIN_EXE_dcu"))
        .args(["--local-tools", "--strict-compatibility"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2)); // clap argument conflict.
}

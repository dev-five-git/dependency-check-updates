//! Stable reporting contracts and shared exit policy for binaries and bridges.
use crate::cli::{Cli, OutputFormat};
use dependency_check_updates_core::DcuError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Update,
    Current,
    Channel,
    Unsupported,
    Failed,
    Blocked,
    Unverified,
    Missing,
}

impl Status {
    pub fn incomplete(self) -> bool {
        matches!(
            self,
            Self::Unsupported | Self::Failed | Self::Blocked | Self::Unverified | Self::Missing
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectRow {
    pub manifest: String,
    pub name: String,
    pub section: String,
    pub from: String,
    pub to: Option<String>,
    pub latest: Option<String>,
    pub selected: Option<String>,
    #[serde(default)]
    pub compatible: Option<String>,
    pub status: Status,
    pub reason: Option<String>,
    pub compatibility: Option<String>,
    pub selection_policy: Option<String>,
    pub updated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalRow {
    pub name: String,
    pub scope: String,
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub selected: Option<String>,
    pub status: Status,
    pub reason: Option<String>,
    pub update_command: String,
    pub updated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Item {
    Project(ProjectRow),
    Local(LocalRow),
}

impl Item {
    pub fn status(&self) -> Status {
        match self {
            Self::Project(r) => r.status,
            Self::Local(r) => r.status,
        }
    }
    pub fn updated(&self) -> bool {
        match self {
            Self::Project(r) => r.updated,
            Self::Local(r) => r.updated,
        }
    }
    pub fn incomplete(&self) -> bool {
        self.status().incomplete()
            || matches!(self, Self::Project(r) if r.compatibility.as_deref().is_some_and(|s| s.starts_with("unverified:")))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Diagnostic {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub manifests: usize,
    pub checked: usize,
    pub updates: usize,
    pub updated: usize,
    pub incomplete: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ApplyOutcome {
    #[default]
    NotRequested,
    NoChanges,
    Committed,
    Aborted,
    RolledBack,
    RecoveryRequired,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RunReport {
    pub items: Vec<Item>,
    pub diagnostics: Vec<Diagnostic>,
    pub outcome: ApplyOutcome,
    pub execution_failed: bool,
    pub manifest_count: usize,
    pub compatibility_rules: Vec<crate::compatibility_rules::Provenance>,
}

impl RunReport {
    pub fn summary(&self) -> Summary {
        Summary {
            manifests: self.manifest_count,
            checked: self.items.len(),
            updates: self
                .items
                .iter()
                .filter(|r| r.status() == Status::Update)
                .count(),
            updated: self.items.iter().filter(|r| r.updated()).count(),
            incomplete: self.items.iter().filter(|r| r.incomplete()).count()
                + self.diagnostics.len(),
        }
    }
    pub fn has_updates(&self) -> bool {
        self.summary().updates != 0
    }
    pub fn incomplete(&self) -> bool {
        self.summary().incomplete != 0
    }
    pub fn exit_code(&self, cli: &Cli) -> u8 {
        if self.execution_failed {
            1
        } else if cli.fail_on_incomplete && self.incomplete() {
            2
        } else {
            u8::from(cli.error_level >= 2 && self.has_updates())
        }
    }

    pub fn json(&self, format: OutputFormat) -> Result<Value, DcuError> {
        match format {
            OutputFormat::Json => Ok(serde_json::to_value(&self.items).expect("report items")),
            OutputFormat::JsonReport => Ok(serde_json::json!({
                "schemaVersion": 2,
                "summary": self.summary(),
                "items": self.items,
                "diagnostics": self.diagnostics,
                "applyOutcome": self.outcome,
                "compatibilityRules": self.compatibility_rules,
            })),
            OutputFormat::JsonLegacy => {
                self.validate_legacy()?;
                let mut updates = serde_json::Map::new();
                for item in &self.items {
                    if let Item::Project(r) = item
                        && let Some(to) = &r.to
                    {
                        if updates.get(&r.name).is_some_and(|v| v != to) {
                            return Err(crate::project::error(
                                "json-legacy",
                                "conflicting versions cannot be represented; use --format json-report",
                            ));
                        }
                        updates.insert(r.name.clone(), to.clone().into());
                    }
                }
                Ok(Value::Object(updates))
            }
            OutputFormat::Table => unreachable!("table is not JSON"),
        }
    }

    pub fn validate_legacy(&self) -> Result<(), DcuError> {
        let manifests: BTreeSet<_> = self
            .items
            .iter()
            .filter_map(|r| match r {
                Item::Project(r) => Some(&r.manifest),
                Item::Local(_) => None,
            })
            .collect();
        if self.manifest_count > 1
            || manifests.len() > 1
            || self.items.iter().any(|r| matches!(r, Item::Local(_)))
        {
            return Err(crate::project::error(
                "json-legacy",
                "requires one effective project manifest; use --format json-report",
            ));
        }
        Ok(())
    }

    pub fn print_json(&self, format: OutputFormat) -> Result<(), DcuError> {
        println!(
            "{}",
            serde_json::to_string_pretty(&self.json(format)?).expect("JSON report")
        );
        if format != OutputFormat::JsonReport {
            for d in &self.diagnostics {
                eprintln!("{}: {}", d.code, d.message);
            }
        }
        if format == OutputFormat::JsonLegacy {
            for item in &self.items {
                if let Item::Project(r) = item
                    && r.status.incomplete()
                {
                    eprintln!(
                        "{} [{}]: {}",
                        r.name,
                        r.manifest,
                        r.reason.as_deref().unwrap_or("incomplete check")
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn exit_policy_preserves_defaults_and_prioritizes_incomplete() {
        let mut report = RunReport::default();
        report.items.push(Item::Local(LocalRow {
            name: "node".into(),
            scope: "local".into(),
            installed: None,
            latest: None,
            selected: None,
            status: Status::Missing,
            reason: Some("not installed".into()),
            update_command: "installer".into(),
            updated: false,
        }));
        assert_eq!(report.exit_code(&Cli::parse_from(["dcu", "-e", "2"])), 0);
        assert_eq!(
            report.exit_code(&Cli::parse_from(["dcu", "--fail-on-incomplete", "-e", "2"])),
            2
        );
        report.execution_failed = true;
        assert_eq!(
            report.exit_code(&Cli::parse_from(["dcu", "--fail-on-incomplete"])),
            1
        );
    }

    #[test]
    fn empty_output_contract_and_legacy_scope() {
        let mut report = RunReport::default();
        assert_eq!(
            report.json(OutputFormat::Json).unwrap(),
            serde_json::json!([])
        );
        assert_eq!(
            report.json(OutputFormat::JsonLegacy).unwrap(),
            serde_json::json!({})
        );
        let json = report.json(OutputFormat::JsonReport).unwrap();
        assert_eq!(json["schemaVersion"], 2);
        assert_eq!(json["applyOutcome"], "not-requested");
        report.manifest_count = 2;
        assert!(report.json(OutputFormat::JsonLegacy).is_err());
    }

    #[test]
    fn local_rows_are_not_representable_in_legacy_output_and_diagnostics_survive_json() {
        let report = RunReport {
            items: vec![Item::Local(LocalRow {
                name: "node".into(),
                scope: "local".into(),
                installed: None,
                latest: None,
                selected: None,
                status: Status::Missing,
                reason: Some("not installed".into()),
                update_command: "installer".into(),
                updated: false,
            })],
            diagnostics: vec![Diagnostic {
                code: "probe".into(),
                message: "failed".into(),
                path: None,
            }],
            ..RunReport::default()
        };
        assert!(report.validate_legacy().is_err());
        assert!(report.json(OutputFormat::JsonLegacy).is_err());
        report.print_json(OutputFormat::Json).unwrap();
    }

    fn contract_fields(schema: &Value, definition: &Value, value: &Value) {
        let expected: BTreeSet<_> = definition["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let actual: BTreeSet<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(actual, expected);
        for (name, rule) in definition["properties"].as_object().unwrap() {
            let rule = if let Some(reference) = rule["$ref"].as_str() {
                schema.pointer(reference.trim_start_matches('#')).unwrap()
            } else {
                rule
            };
            let field = &value[name];
            if let Some(required) = rule.get("const") {
                assert_eq!(field, required);
            }
            if let Some(choices) = rule["enum"].as_array() {
                assert!(choices.contains(field));
            }
            match rule["type"].as_str() {
                Some("string") => assert!(field.is_string()),
                Some("integer") => assert!(field.as_i64().is_some() || field.as_u64().is_some()),
                Some("boolean") => assert!(field.is_boolean()),
                Some("array") => assert!(field.is_array()),
                Some("object") => assert!(field.is_object()),
                _ if rule["type"].is_array() => assert!(field.is_null() || field.is_string()),
                _ => {}
            }
        }
    }

    #[test]
    fn serialized_report_contract_matches_schema_fields_types_statuses_and_outcomes() {
        let schema: Value =
            serde_json::from_str(include_str!("../schemas/report-v2.schema.json")).unwrap();
        let mut report = RunReport {
            compatibility_rules: crate::compatibility_rules::Rules::builtin().provenance,
            ..RunReport::default()
        };
        for status in [
            Status::Update,
            Status::Current,
            Status::Channel,
            Status::Unsupported,
            Status::Failed,
            Status::Blocked,
            Status::Unverified,
            Status::Missing,
        ] {
            report.items.push(Item::Project(ProjectRow {
                manifest: "app/build.gradle.kts".into(),
                name: "g:a".into(),
                section: "maven".into(),
                from: "1.0".into(),
                to: None,
                latest: None,
                selected: None,
                status,
                compatible: None,
                reason: None,
                compatibility: None,
                selection_policy: None,
                updated: false,
            }));
            report.items.push(Item::Local(LocalRow {
                name: "node".into(),
                scope: "local".into(),
                installed: None,
                latest: None,
                selected: None,
                status,
                reason: None,
                update_command: "installer".into(),
                updated: false,
            }));
        }
        report.diagnostics.push(Diagnostic {
            code: "failure".into(),
            message: "details".into(),
            path: None,
        });
        for outcome in [
            ApplyOutcome::NotRequested,
            ApplyOutcome::NoChanges,
            ApplyOutcome::Committed,
            ApplyOutcome::Aborted,
            ApplyOutcome::RolledBack,
            ApplyOutcome::RecoveryRequired,
        ] {
            report.outcome = outcome;
            let json = report.json(OutputFormat::JsonReport).unwrap();
            contract_fields(&schema, &schema, &json);
            contract_fields(&schema, &schema["$defs"]["summary"], &json["summary"]);
            for p in json["compatibilityRules"].as_array().unwrap() {
                contract_fields(&schema, &schema["$defs"]["provenance"], p);
            }
            for item in json["items"].as_array().unwrap() {
                let definition = if item.get("manifest").is_some() {
                    "project"
                } else {
                    "local"
                };
                contract_fields(&schema, &schema["$defs"][definition], item);
                let decoded: Item = serde_json::from_value(item.clone()).unwrap();
                assert_eq!(serde_json::to_value(decoded).unwrap(), *item);
            }
            contract_fields(
                &schema,
                &schema["$defs"]["diagnostic"],
                &json["diagnostics"][0],
            );
        }
    }

    #[test]
    fn legacy_detects_conflicting_targets_and_collapses_identical_ones() {
        let row = ProjectRow {
            manifest: "package.json".into(),
            name: "pnpm".into(),
            section: "dependencies".into(),
            from: "10.1.0".into(),
            to: Some("10.2.0".into()),
            latest: Some("10.2.0".into()),
            selected: Some("10.2.0".into()),
            compatible: None,
            status: Status::Update,
            reason: None,
            compatibility: None,
            selection_policy: None,
            updated: false,
        };
        let mut report = RunReport {
            manifest_count: 1,
            items: vec![Item::Project(row.clone()), Item::Project(row)],
            ..RunReport::default()
        };
        assert_eq!(
            report.json(OutputFormat::JsonLegacy).unwrap(),
            serde_json::json!({"pnpm":"10.2.0"})
        );
        let Item::Project(row) = &mut report.items[1] else {
            unreachable!()
        };
        row.to = Some("11.0.0".into());
        assert!(report.json(OutputFormat::JsonLegacy).is_err());
    }

    #[test]
    fn unverified_updates_count_incomplete_even_with_an_available_target() {
        let mut report = RunReport::default();
        let row = ProjectRow {
            manifest: "build.gradle.kts".into(),
            name: "gradle".into(),
            section: "toolchain".into(),
            from: "8.9".into(),
            to: Some("8.13".into()),
            latest: Some("8.13".into()),
            selected: Some("8.13".into()),
            compatible: None,
            status: Status::Update,
            reason: None,
            compatibility: Some("unverified: missing JDK pin".into()),
            selection_policy: None,
            updated: false,
        };
        report.items.push(Item::Project(row));
        assert_eq!(report.summary().incomplete, 1);
        assert_eq!(
            report.exit_code(&Cli::parse_from(["dcu", "--fail-on-incomplete", "-e", "2"])),
            2
        );
        assert_eq!(report.exit_code(&Cli::parse_from(["dcu", "-e", "2"])), 1);
    }
}

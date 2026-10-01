//! Bounded search over published pins, using only the existing compatibility rules.
use crate::{
    compatibility,
    compatibility_rules::Rules,
    pipeline::compute_updates,
    project::Document,
    report::Diagnostic,
    run::{ManifestJob, ResolvedBatch, source_entry},
    tool_registry::ToolRegistry,
};
use dependency_check_updates_core::{
    DependencySpec, PlannedUpdate, ResolvedVersion, TargetLevel, pad_to_three_segments,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

const MAX_CANDIDATES: usize = 64;
const MAX_STEPS: usize = 4096;

#[derive(Default)]
pub(crate) struct Suggestions {
    pub choices: HashMap<(usize, usize), String>,
    pub updates: compatibility::Plans,
    pub diagnostics: Vec<Diagnostic>,
    pub coupled: HashSet<(usize, usize)>,
}

struct Input {
    job: usize,
    dep_index: usize,
    path: PathBuf,
    dep: DependencySpec,
    candidates: Vec<String>,
}

#[derive(Default)]
struct Build {
    fixed: Vec<(String, String)>,
    variables: Vec<usize>,
}

fn category(name: &str) -> u8 {
    match name {
        "com.android.tools.build:gradle" | "com.android.application" | "com.android.library" => 0,
        "gradle" => 2,
        "jdk" => 3,
        "android.compileSdk" | "android.targetSdk" => 4,
        _ => 1,
    }
}

fn key(value: &str) -> Option<semver::Version> {
    semver::Version::parse(&pad_to_three_segments(value.trim_start_matches('v')))
        .ok()
        .map(|mut v| {
            v.build = semver::BuildMetadata::EMPTY;
            v
        })
}

fn add_value(build: &mut Build, path: &std::path::Path, dep: &DependencySpec, inputs: &[Input]) {
    if !compatibility::related(&dep.name) {
        return;
    }
    let matching: Vec<_> = inputs
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            i.path == path
                && i.dep.name == dep.name
                && i.dep.current_req == dep.current_req
                && i.dep.section == dep.section
        })
        .map(|(i, _)| i)
        .collect();
    if matching.is_empty() {
        build
            .fixed
            .push((dep.name.clone(), dep.current_req.clone()));
    } else {
        for i in matching {
            if !build.variables.contains(&i) {
                build.variables.push(i);
            }
        }
    }
}

fn builds(documents: &HashMap<PathBuf, Document>, inputs: &[Input]) -> Vec<Build> {
    let mut scopes: Vec<_> = documents
        .keys()
        .map(|p| compatibility::scope(p, documents))
        .collect();
    scopes.sort();
    scopes.dedup();
    scopes
        .into_iter()
        .filter_map(|root| {
            let members: Vec<_> = documents
                .values()
                .filter(|d| compatibility::scope(&d.path, documents) == root)
                .collect();
            let mut build = Build::default();
            let mut has_gradle = false;
            let mut coupled = false;
            for d in &members {
                for issue in &d.context_issues {
                    build
                        .fixed
                        .push(("unsupported-build-connection".into(), issue.clone()));
                }
                for plugin in &d.applied_plugins {
                    build
                        .fixed
                        .push(("applied-kotlin-android".into(), plugin.clone()));
                }
                for (path, dep) in d
                    .entries
                    .iter()
                    .map(|e| (&d.path, &e.dep))
                    .chain(d.resolved_uses.iter().map(|(p, dep)| (p, dep)))
                {
                    has_gradle |= dep.name == "gradle";
                    coupled |= compatibility::related(&dep.name) && dep.name != "jdk";
                    add_value(&mut build, path, dep, inputs);
                }
            }
            if !coupled {
                return None;
            }
            for d in documents.values().filter(|d| {
                root.starts_with(d.path.parent().unwrap_or(&d.path))
                    && !members.iter().any(|m| m.path == d.path)
            }) {
                for e in d.entries.iter().filter(|e| e.dep.name == "jdk") {
                    add_value(&mut build, &d.path, &e.dep, inputs);
                }
            }
            if !has_gradle {
                for parent in root.ancestors() {
                    if let Some(wrapper) =
                        documents.get(&parent.join("gradle/wrapper/gradle-wrapper.properties"))
                    {
                        for e in &wrapper.entries {
                            add_value(&mut build, &wrapper.path, &e.dep, inputs);
                        }
                        break;
                    }
                }
            }
            Some(build)
        })
        .collect()
}

fn update(input: &Input, candidate: &str) -> Option<PlannedUpdate> {
    let selected = if input.dep.section
        == dependency_check_updates_core::DependencySection::Toolchain
        && input.dep.name != "gradle"
    {
        candidate
            .split('.')
            .take(
                input
                    .dep
                    .current_req
                    .trim_start_matches('v')
                    .split('.')
                    .count(),
            )
            .collect::<Vec<_>>()
            .join(".")
    } else {
        candidate.to_owned()
    };
    compute_updates(
        std::slice::from_ref(&input.dep),
        &[(
            0,
            Ok(ResolvedVersion {
                latest: None,
                selected: Some(selected),
            }),
        )],
    )
    .pop()
}

struct Dimension {
    members: Vec<usize>,
    candidates: Vec<String>,
}

fn dimensions(inputs: &[Input], builds: &[Build]) -> Vec<Dimension> {
    let mut owners: Vec<_> = (0..inputs.len()).collect();
    for build in builds {
        for &a in &build.variables {
            for &b in &build.variables {
                if category(&inputs[a].dep.name) == category(&inputs[b].dep.name) {
                    let old = owners[b];
                    let new = owners[a];
                    for owner in &mut owners {
                        if *owner == old {
                            *owner = new;
                        }
                    }
                }
            }
        }
    }
    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for (i, owner) in owners.into_iter().enumerate() {
        if builds.iter().any(|b| b.variables.contains(&i)) {
            groups.entry(owner).or_default().push(i);
        }
    }
    let mut dims: Vec<_> = groups
        .into_values()
        .map(|members| {
            let mut candidates = inputs[members[0]].candidates.clone();
            candidates.retain(|c| {
                members
                    .iter()
                    .all(|&i| inputs[i].candidates.iter().any(|v| key(v) == key(c)))
            });
            // Keep the current common pin as an explicit non-mutating fallback.
            let current = candidates
                .iter()
                .find(|v| key(v) == key(&inputs[members[0]].dep.current_req))
                .cloned();
            candidates.truncate(MAX_CANDIDATES);
            if let Some(current) = current
                && !candidates.contains(&current)
            {
                candidates.push(current);
            }
            Dimension {
                members,
                candidates,
            }
        })
        .collect();
    dims.sort_by_key(|d| (category(&inputs[d.members[0]].dep.name), d.members[0]));
    dims
}

fn search(
    dims: &[Dimension],
    inputs: &[Input],
    builds: &[Build],
    rules: &Rules,
    assigned: &mut HashMap<usize, String>,
    depth: usize,
    steps: &mut usize,
) -> bool {
    for build in builds {
        let mut values: Vec<_> = build
            .fixed
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        for i in &build.variables {
            if let Some(v) = assigned.get(i) {
                values.push((&inputs[*i].dep.name, v));
            }
        }
        let status = compatibility::check_with_rules(&values, rules);
        if status.starts_with("conflict:")
            || (depth == dims.len() && !status.starts_with("verified:"))
        {
            return false;
        }
    }
    if depth == dims.len() {
        return true;
    }
    let dim = &dims[depth];
    for candidate in &dim.candidates {
        if *steps >= MAX_STEPS {
            return false;
        }
        *steps += 1;
        for &i in &dim.members {
            let own = inputs[i]
                .candidates
                .iter()
                .find(|v| key(v) == key(candidate))
                .unwrap();
            let value =
                update(&inputs[i], own).map_or_else(|| inputs[i].dep.current_req.clone(), |u| u.to);
            assigned.insert(i, value);
        }
        if search(dims, inputs, builds, rules, assigned, depth + 1, steps) {
            return true;
        }
        for i in &dim.members {
            assigned.remove(i);
        }
    }
    false
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn suggest(
    jobs: &[ManifestJob],
    resolved: &[Option<ResolvedBatch>],
    documents: &HashMap<PathBuf, Document>,
    registry: &ToolRegistry,
    target: TargetLevel,
    rules: &Rules,
) -> Suggestions {
    let mut result = Suggestions::default();
    let mut inputs = Vec::new();
    for (job_idx, job) in jobs.iter().enumerate() {
        for (i, dep) in job
            .deps
            .iter()
            .enumerate()
            .filter(|(_, d)| compatibility::related(&d.name))
        {
            let Some(entry) =
                source_entry(job, i).filter(|e| e.reason.is_none() && e.span.is_some())
            else {
                continue;
            };
            let Some(selected) = resolved[job_idx]
                .as_ref()
                .and_then(|b| b.iter().find(|(idx, _)| *idx == i))
                .and_then(|(_, r)| r.as_ref().ok())
                .and_then(|r| r.selected.as_deref())
            else {
                continue;
            };
            match registry.candidates(entry, target, selected).await {
                Ok(candidates) => inputs.push(Input {
                    job: job_idx,
                    dep_index: i,
                    path: job.manifest_ref.path.clone(),
                    dep: dep.clone(),
                    candidates,
                }),
                Err(e) => {
                    result.diagnostics.push(Diagnostic {
                        code: "compatible-lookup-failed".into(),
                        message: e.to_string(),
                        path: Some(job.display_path.clone()),
                    });
                    inputs.push(Input {
                        job: job_idx,
                        dep_index: i,
                        path: job.manifest_ref.path.clone(),
                        dep: dep.clone(),
                        candidates: Vec::new(),
                    });
                }
            }
        }
    }
    if inputs.is_empty() {
        return result;
    }
    let builds = builds(documents, &inputs);
    for build in &builds {
        for &i in &build.variables {
            result.coupled.insert((inputs[i].job, inputs[i].dep_index));
        }
    }
    // Search each connected component independently; an unrelated broken build
    // must not suppress verified suggestions for a separate build.
    let mut components: Vec<Vec<usize>> = (0..builds.len()).map(|i| vec![i]).collect();
    let mut a = 0;
    while a < components.len() {
        let mut b = a + 1;
        while b < components.len() {
            if components[a].iter().any(|&x| {
                components[b].iter().any(|&y| {
                    builds[x]
                        .variables
                        .iter()
                        .any(|i| builds[y].variables.contains(i))
                })
            }) {
                let other = components.remove(b);
                components[a].extend(other);
                a = 0;
                b = 1;
            } else {
                b += 1;
            }
        }
        a += 1;
    }
    for component in components {
        let scoped: Vec<_> = component
            .iter()
            .map(|&i| Build {
                fixed: builds[i].fixed.clone(),
                variables: builds[i].variables.clone(),
            })
            .collect();
        let dims = dimensions(&inputs, &scoped);
        if dims.is_empty() {
            continue;
        }
        let mut assigned = HashMap::new();
        let mut steps = 0;
        if !search(&dims, &inputs, &scoped, rules, &mut assigned, 0, &mut steps) {
            if steps >= MAX_STEPS {
                result.diagnostics.push(Diagnostic {
                    code: "compatible-search-limit".into(),
                    message:
                        "bounded compatibility search exhausted 4096 steps; no suggestion applied"
                            .into(),
                    path: None,
                });
            }
            continue;
        }
        for (i, candidate) in assigned {
            let input = &inputs[i];
            result
                .choices
                .insert((input.job, input.dep_index), candidate.clone());
            if let Some(update) = update(input, &candidate) {
                result
                    .updates
                    .entry(input.path.clone())
                    .or_default()
                    .push(update);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;

    #[test]
    fn incomplete_combinations_exhaust_a_bounded_budget_without_leaving_assignments() {
        let inputs: Vec<_> = (0..5)
            .map(|i| Input {
                job: 0,
                dep_index: i,
                path: PathBuf::from("wrapper"),
                dep: DependencySpec {
                    name: "gradle".into(),
                    current_req: "8.9".into(),
                    section: DependencySection::Toolchain,
                    path_version: None,
                },
                candidates: vec![
                    "8.13".into(),
                    "8.12".into(),
                    "8.11".into(),
                    "8.10".into(),
                    "8.9".into(),
                    "8.8".into(),
                ],
            })
            .collect();
        let dims: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| Dimension {
                members: vec![i],
                candidates: input.candidates.clone(),
            })
            .collect();
        let build = Build {
            fixed: vec![
                ("jdk".into(), "17".into()),
                ("unsupported-build-connection".into(), "dynamic".into()),
            ],
            variables: (0..5).collect(),
        };
        let mut assigned = HashMap::new();
        let mut steps = 0;
        assert!(!search(
            &dims,
            &inputs,
            &[build],
            &Rules::builtin(),
            &mut assigned,
            0,
            &mut steps
        ));
        assert_eq!(steps, MAX_STEPS);
        assert!(assigned.is_empty());
    }

    #[test]
    fn short_tool_precision_and_prefixes_are_preserved() {
        let input = Input {
            job: 0,
            dep_index: 0,
            path: PathBuf::from("tools"),
            dep: DependencySpec {
                name: "jdk".into(),
                current_req: "v17".into(),
                section: DependencySection::Toolchain,
                path_version: None,
            },
            candidates: Vec::new(),
        };
        assert_eq!(update(&input, "27.0.1+4").unwrap().to, "v27");
        assert!(update(&input, "11.0.1").is_none());
    }
}

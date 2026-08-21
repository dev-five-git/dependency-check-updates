//! Criterion micro-benchmarks for the Node.js manifest hot paths: parsing a
//! representative `package.json` and applying format-preserving version patches
//! through the public [`ManifestHandler`] surface.
//!
//! Run with `cargo bench`. Criterion stores a baseline under
//! `target/criterion/`, so a second `cargo bench` after a change prints the
//! per-function delta.

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use dependency_check_updates_core::{DependencySection, ManifestHandler, PlannedUpdate};
use dependency_check_updates_node::NodeHandler;

/// A representative `package.json` with a realistic spread of dependencies and
/// devDependencies, mirroring the shape the parser and patcher see in practice.
const PACKAGE_JSON: &str = r#"{
  "name": "bench-fixture",
  "version": "1.0.0",
  "private": true,
  "dependencies": {
    "react": "^17.0.0",
    "react-dom": "^17.0.0",
    "lodash": "^4.17.20",
    "axios": "^0.21.1",
    "express": "^4.17.1",
    "chalk": "^4.1.0",
    "commander": "^7.2.0",
    "zod": "^3.11.6"
  },
  "devDependencies": {
    "typescript": "^4.3.5",
    "eslint": "^7.32.0",
    "jest": "^27.0.6",
    "vite": "^2.4.4",
    "prettier": "^2.3.2"
  }
}
"#;

fn bench_parse(c: &mut Criterion) {
    let handler = NodeHandler;
    let path = Path::new("package.json");
    c.bench_function("node_parse_package_json", |b| {
        b.iter(|| black_box(handler.parse(black_box(PACKAGE_JSON), path)));
    });
}

fn bench_apply_updates(c: &mut Criterion) {
    let handler = NodeHandler;
    let updates = vec![
        PlannedUpdate {
            name: "react".to_owned(),
            section: DependencySection::Dependencies,
            from: "^17.0.0".to_owned(),
            to: "^18.2.0".to_owned(),
        },
        PlannedUpdate {
            name: "lodash".to_owned(),
            section: DependencySection::Dependencies,
            from: "^4.17.20".to_owned(),
            to: "^4.17.21".to_owned(),
        },
        PlannedUpdate {
            name: "typescript".to_owned(),
            section: DependencySection::DevDependencies,
            from: "^4.3.5".to_owned(),
            to: "^5.4.5".to_owned(),
        },
    ];
    c.bench_function("node_apply_updates", |b| {
        b.iter(|| black_box(handler.apply_updates(black_box(PACKAGE_JSON), black_box(&updates))));
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(60)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = bench_parse, bench_apply_updates
}
criterion_main!(benches);

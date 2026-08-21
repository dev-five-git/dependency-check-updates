//! Criterion micro-benchmarks for the hottest pure functions in the core crate.
//!
//! These cover the per-tag / per-dependency code that the scan -> resolve ->
//! patch pipeline drives thousands of times per `dcu` run, and that the
//! autonomous improvement loop most often touches: range-prefix stripping,
//! three-segment padding, and the shared `select_version` algorithm.
//!
//! Run with `cargo bench`. Criterion stores a baseline under
//! `target/criterion/`, so a second `cargo bench` after a change prints the
//! per-function delta (e.g. `change: [-6.1% -4.8%] Performance has improved`).

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use dependency_check_updates_core::{
    TargetLevel, pad_to_three_segments, select_version, strip_range_prefix,
};

/// Representative requirement strings spanning every range operator the
/// stripper handles, plus a wildcard (no digits) and a pre-release tail.
const REQS: &[&str] = &[
    "^1.2.3",
    "~2.0.0",
    ">=3.4.5",
    "=4.0.0",
    "1.0.0",
    "^0.25.11",
    ">2.1",
    "*",
    "~1.2.3-beta.1",
];

/// Representative version strings spanning 1/2/3/4-segment shapes plus
/// pre-release and build-metadata suffixes and the empty input.
const VERSIONS: &[&str] = &[
    "5",
    "5.1",
    "5.1.0",
    "5.1.2.3",
    "5.1.0-rc.1",
    "1.2-beta",
    "18.2.0",
    "0.25.11+build.7",
    "",
];

fn bench_strip_range_prefix(c: &mut Criterion) {
    c.bench_function("strip_range_prefix", |b| {
        b.iter(|| {
            for &req in REQS {
                black_box(strip_range_prefix(black_box(req)));
            }
        });
    });
}

fn bench_pad_to_three_segments(c: &mut Criterion) {
    c.bench_function("pad_to_three_segments", |b| {
        b.iter(|| {
            for &v in VERSIONS {
                black_box(pad_to_three_segments(black_box(v)));
            }
        });
    });
}

fn bench_select_version(c: &mut Criterion) {
    // A realistic, pre-sorted (ascending) candidate list like a registry returns.
    let mut candidates: Vec<semver::Version> = (0u64..40)
        .flat_map(|major| (0u64..5).map(move |minor| semver::Version::new(major, minor, major % 3)))
        .collect();
    candidates.sort();
    let current = semver::Version::new(20, 1, 0);

    c.bench_function("select_version_latest", |b| {
        b.iter(|| {
            black_box(select_version(
                black_box(Some(&current)),
                black_box(candidates.as_slice()),
                TargetLevel::Latest,
                Some("39.4.2"),
                Some("39.4.2"),
            ))
        });
    });
    c.bench_function("select_version_minor", |b| {
        b.iter(|| {
            black_box(select_version(
                black_box(Some(&current)),
                black_box(candidates.as_slice()),
                TargetLevel::Minor,
                Some("39.4.2"),
                Some("39.4.2"),
            ))
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(60)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = bench_strip_range_prefix, bench_pad_to_three_segments, bench_select_version
}
criterion_main!(benches);

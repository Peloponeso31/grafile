//! Cross-version compatibility: every checked-in release fixture opens with
//! the current build, passes the integrity checker and answers the snapshot
//! queries identically.
//!
//! Fixtures are written by `scripts/version-fixture.sh` from
//! `tests/fixtures/versions/fixture.cypher`; each release adds its own.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use grafile::api::Connection;
use grafile::vfs::{OpenMode, StdVfs};
use support::query;

/// Releases whose fixture must be present.
const RELEASES: [&str; 3] = ["0.2.0", "0.2.1", "0.2.2"];

/// `(query, rows)` pairs every fixture must reproduce.
const SNAPSHOT: &[(&str, &[&[&str]])] = &[
    ("MATCH (n) RETURN count(n)", &[&["596"]]),
    (
        "MATCH ()-[r]->() RETURN type(r), count(r) ORDER BY type(r)",
        &[&["FOLLOWS", "144"], &["KNOWS", "587"], &["SELF", "11"]],
    ),
    (
        "MATCH (p:Person) RETURN count(p), sum(p.age), min(p.email), max(p.score)",
        &[&["587", "25545", "p100@example.org", "75"]],
    ),
    (
        "MATCH (p:Person {email: 'p17@example.org'}) \
         RETURN p.name, p.tags, p.active, p.born, p.seen, p.home",
        &[&[
            "person 17",
            "[t2, all]",
            "false",
            "1990-01-18",
            "2026-01-01T00:17Z",
            "point({x: 17.0, y: 3.0})",
        ]],
    ),
    (
        "MATCH (p:Person) WHERE p.age = 42 RETURN p.email ORDER BY p.email",
        &[
            &["p132@example.org"],
            &["p222@example.org"],
            &["p312@example.org"],
            &["p402@example.org"],
            &["p42@example.org"],
            &["p492@example.org"],
            &["p582@example.org"],
        ],
    ),
    // Node 256 starts the second incidence band; 0.2.0 and 0.2.1 misread
    // its neighbours' rows.
    (
        "MATCH (:Person {email: 'p255@example.org'})-[r]->(b) RETURN type(r), b.email",
        &[&["KNOWS", "p256@example.org"]],
    ),
    (
        "MATCH (:Person {email: 'p257@example.org'})<-[r]-(b) RETURN type(r), b.email",
        &[&["KNOWS", "p256@example.org"]],
    ),
    (
        "MATCH ()-[r:KNOWS {since: 2005}]->() RETURN count(r)",
        &[&["30"]],
    ),
    ("MATCH (p:Person)-[:SELF]->(p) RETURN count(p)", &[&["11"]]),
    (
        "MATCH (f:Former) RETURN count(f), min(f.email)",
        &[&["7", "p135@example.org"]],
    ),
    (
        "MATCH (d:Doc) RETURN d.n, size(d.body) ORDER BY d.n",
        &[&["1", "5400"], &["3", "5400"]],
    ),
    (
        "MATCH (a:Person)-[:KNOWS*3]->(:Person {email: 'p512@example.org'}) RETURN a.email",
        &[&["p509@example.org"]],
    ),
];

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/versions");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixture directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "grafile"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_release_fixture_is_present() {
    let names: Vec<String> = fixtures()
        .iter()
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_owned))
        .collect();
    for release in RELEASES {
        assert!(
            names.iter().any(|name| name == release),
            "{release}: {names:?}"
        );
    }
}

#[test]
fn release_fixtures_are_clean_and_answer_the_snapshot() {
    for path in fixtures() {
        // Read-only, so the checked-in file is never modified.
        let mut conn = Connection::open_with_vfs(Arc::new(StdVfs), &path, OpenMode::ReadOnly)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let report = conn.database_mut().check_integrity().expect("check");
        assert!(report.is_clean(), "{}:\n{report}", path.display());
        for (source, expected) in SNAPSHOT {
            let expected: Vec<Vec<String>> = expected
                .iter()
                .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
                .collect();
            assert_eq!(
                query(&mut conn, source),
                expected,
                "{}: {source}",
                path.display()
            );
        }
    }
}

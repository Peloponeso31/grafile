//! Fulltext indexes: DDL, backfill, write maintenance, queries and
//! persistence.

mod support;

use std::path::Path;
use std::sync::Arc;

use grafile::api::Connection;
use grafile::vfs::OpenMode;
use grafile::vfs::mem::MemVfs;
use support::{connection, query};

const POST_INDEX: &str = "CREATE FULLTEXT INDEX post_text IF NOT EXISTS \
                          FOR (n:Post) ON EACH [n.title, n.body]";

fn search(conn: &mut Connection, text: &str) -> Vec<String> {
    let mut titles: Vec<String> = query(
        conn,
        &format!(
            "CALL db.index.fulltext.queryNodes('post_text', '{text}') YIELD node \
             RETURN node.title"
        ),
    )
    .into_iter()
    .map(|mut row| row.remove(0))
    .collect();
    titles.sort_unstable();
    titles
}

fn query_error(conn: &mut Connection, source: &str) -> grafile::Error {
    match conn.query(source, &grafile::api::Params::new()) {
        Ok(_) => panic!("query should fail: {source}"),
        Err(err) => err,
    }
}

#[test]
fn backfills_and_answers_term_phrase_and_boolean_queries() {
    let mut conn = connection();
    conn.execute(
        "CREATE (:Post {title: 'graphs', body: 'Hot take on graph databases'}), \
         (:Post {title: 'coffee', body: 'Coffee and graphs'}), \
         (:Post {title: 'tea', body: 'take hot tea'}), \
         (:Note {title: 'graphs elsewhere'})",
    )
    .expect("seed");
    conn.execute(POST_INDEX).expect("create index");

    assert_eq!(search(&mut conn, "graphs"), vec!["coffee", "graphs"]);
    assert_eq!(search(&mut conn, "GRAPHS AND coffee"), vec!["coffee"]);
    assert_eq!(
        search(&mut conn, "\"hot take\" OR tea"),
        vec!["graphs", "tea"]
    );
    assert!(search(&mut conn, "missing").is_empty());
}

#[test]
fn backfill_matches_incremental_maintenance() {
    const SEED: &str = "UNWIND range(1, 3000) AS x \
        CREATE (:Post {title: 'post ' + toString(x % 37), \
                       body: 'graph ' + toString(x % 11) + ' hot take number ' + toString(x)})";
    let mut incremental = connection();
    incremental.execute(POST_INDEX).expect("create index");
    incremental.execute(SEED).expect("seed");
    let mut backfilled = connection();
    backfilled.execute(SEED).expect("seed");
    backfilled.execute(POST_INDEX).expect("create index");

    for text in ["graph", "\"hot take\" AND post", "7 OR 3", "number"] {
        let source = format!(
            "CALL db.index.fulltext.queryNodes('post_text', '{text}') YIELD node, score \
             RETURN id(node), score"
        );
        assert_eq!(
            query(&mut backfilled, &source),
            query(&mut incremental, &source),
            "{text}"
        );
    }
}

#[test]
fn yields_node_and_descending_score() {
    let mut conn = connection();
    conn.execute(POST_INDEX).expect("create index");
    conn.execute(
        "CREATE (:Post {title: 'once', body: 'graphs'}), \
         (:Post {title: 'thrice', body: 'graphs graphs graphs'})",
    )
    .expect("seed");
    let rows = query(
        &mut conn,
        "CALL db.index.fulltext.queryNodes('post_text', 'graphs') YIELD node, score \
         MATCH (node:Post) RETURN node.title, score > 0.0",
    );
    assert_eq!(rows, vec![vec!["thrice", "true"], vec!["once", "true"]]);
}

#[test]
fn writes_keep_the_index_current() {
    let mut conn = connection();
    conn.execute(POST_INDEX).expect("create index");
    conn.execute("CREATE (:Post {title: 'a', body: 'red fox'}), (:Post {title: 'b'})")
        .expect("seed");
    conn.execute("MATCH (p:Post {title: 'a'}) SET p.body = 'blue whale'")
        .expect("set property");
    assert!(search(&mut conn, "fox").is_empty());
    assert_eq!(search(&mut conn, "whale"), vec!["a"]);

    conn.execute("MATCH (p:Post {title: 'b'}) SET p = {title: 'b', body: 'whale song'}")
        .expect("replace properties");
    assert_eq!(search(&mut conn, "whale"), vec!["a", "b"]);

    conn.execute("MATCH (p:Post {title: 'a'}) REMOVE p:Post")
        .expect("remove label");
    assert_eq!(search(&mut conn, "whale"), vec!["b"]);
    conn.execute("MATCH (p {title: 'a'}) SET p:Post")
        .expect("add label");
    assert_eq!(search(&mut conn, "whale"), vec!["a", "b"]);

    conn.execute("MATCH (p:Post {title: 'b'}) REMOVE p.body")
        .expect("remove property");
    conn.execute("MATCH (p:Post {title: 'a'}) DETACH DELETE p")
        .expect("delete");
    assert!(search(&mut conn, "whale").is_empty());
    assert!(search(&mut conn, "song").is_empty());
}

#[test]
fn non_text_values_are_ignored() {
    let mut conn = connection();
    conn.execute(POST_INDEX).expect("create index");
    conn.execute("CREATE (:Post {title: 'n', body: 42}), (:Post {title: 'l', body: ['x']})")
        .expect("seed");
    assert!(search(&mut conn, "42").is_empty());
    assert!(search(&mut conn, "x").is_empty());
}

#[test]
fn ddl_names_and_errors() {
    let mut conn = connection();
    conn.execute(POST_INDEX).expect("create index");
    conn.execute(POST_INDEX).expect("IF NOT EXISTS is a no-op");
    let err = conn
        .execute("CREATE FULLTEXT INDEX post_text FOR (n:Post) ON EACH [n.title]")
        .expect_err("duplicate name");
    assert_eq!(err.code(), grafile::ErrorCode::Exists);
    let err = conn
        .execute("CREATE INDEX post_text FOR (n:Post) ON (n.title)")
        .expect_err("names are shared with range indexes");
    assert_eq!(err.code(), grafile::ErrorCode::Exists);
    let err = conn
        .execute("DROP CONSTRAINT post_text")
        .expect_err("not a constraint");
    assert!(err.message().contains("DROP INDEX"), "{err}");

    let err = query_error(
        &mut conn,
        "CALL db.index.fulltext.queryNodes('nope', 'x') YIELD node RETURN node",
    );
    assert_eq!(err.code(), grafile::ErrorCode::NotFound);
    let err = query_error(
        &mut conn,
        "CALL db.index.fulltext.queryNodes('post_text', 'a AND') YIELD node RETURN node",
    );
    assert!(err.message().contains("AND/OR"), "{err}");
    let err = query_error(
        &mut conn,
        "CALL db.index.fulltext.queryNodes('post_text', 1) YIELD node RETURN node",
    );
    assert_eq!(err.code(), grafile::ErrorCode::Type);

    conn.execute("DROP INDEX post_text").expect("drop");
    assert!(conn.schema().fulltext_indexes.is_empty());
    conn.execute("DROP INDEX post_text IF EXISTS")
        .expect("IF EXISTS is a no-op");
}

#[test]
fn index_survives_reopen() {
    let store = grafile::vfs::mem::new_store();
    {
        let vfs = Arc::new(MemVfs::from_store(store.clone()));
        let mut conn = Connection::create_with_vfs(vfs, Path::new("ft.db"), 4096).expect("create");
        conn.execute(POST_INDEX).expect("create index");
        // Enough postings to split the inverted-index root page.
        conn.execute(
            "UNWIND range(1, 2000) AS i \
             CREATE (:Post {title: 't' + toString(i), body: 'word' + toString(i % 300)})",
        )
        .expect("seed");
    }
    let vfs = Arc::new(MemVfs::from_store(store));
    let mut conn =
        Connection::open_with_vfs(vfs, Path::new("ft.db"), OpenMode::ReadWrite).expect("open");
    assert_eq!(conn.database().header().format_minor, 4);
    let rows = query(
        &mut conn,
        "CALL db.index.fulltext.queryNodes('post_text', 'word7 OR word8') YIELD node \
         RETURN count(node)",
    );
    assert_eq!(rows, vec![vec!["14"]]);
}

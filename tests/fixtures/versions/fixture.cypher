// Cross-version compatibility fixture.
//
// Every release from 0.2.0 on runs this script with its own CLI and checks
// in the resulting file as tests/fixtures/versions/<version>.grafile:
//
//   scripts/version-fixture.sh
//
// The script is deterministic, so the file of every version must answer the
// queries in tests/compat.rs identically. It exercises every record value
// type, a unique constraint, a NOT NULL constraint, node and edge indexes,
// incidence-band boundaries (ids around 256 and 512), overflow values and
// freed pages. Keep it to syntax that 0.2.0 supports, and keep apostrophes
// out of comments: the 0.2.0 statement splitter reads them as quotes.

CREATE CONSTRAINT person_email FOR (p:Person) REQUIRE p.email IS UNIQUE;
CREATE CONSTRAINT person_name FOR (p:Person) REQUIRE p.name IS NOT NULL;
CREATE INDEX person_age FOR (p:Person) ON (p.age);
CREATE INDEX knows_since FOR ()-[r:KNOWS]-() ON (r.since);

UNWIND range(1, 600) AS i
CREATE (:Person {
  email: 'p' + toString(i) + '@example.org',
  name: 'person ' + toString(i),
  age: i % 90,
  score: toFloat(i) / 8.0,
  tags: ['t' + toString(i % 5), 'all'],
  active: i % 2 = 0,
  born: date('1990-01-01') + duration({days: i}),
  seen: datetime('2026-01-01T00:00:00Z') + duration({minutes: i}),
  home: point({x: toFloat(i), y: toFloat(i % 7)})
});

MATCH (a:Person), (b:Person)
WHERE id(b) = id(a) + 1
CREATE (a)-[:KNOWS {since: 2000 + id(a) % 20}]->(b);

MATCH (a:Person), (b:Person)
WHERE id(b) = (id(a) * 7) % 600 + 1 AND id(a) % 3 = 0
CREATE (a)-[:FOLLOWS {weight: id(a) % 4}]->(b);

MATCH (p:Person)
WHERE id(p) % 50 = 0
CREATE (p)-[:SELF]->(p);

UNWIND range(1, 3) AS i
CREATE (:Doc {n: i, body: reduce(text = '', x IN range(1, 900) | text + 'lorem ')});

MATCH (p:Person) WHERE p.age = 0 DETACH DELETE p;
MATCH (d:Doc {n: 2}) DELETE d;
MATCH (p:Person) WHERE p.age = 45 REMOVE p:Person SET p:Former;
MATCH ()-[r:FOLLOWS]->() WHERE r.weight = 3 DELETE r;

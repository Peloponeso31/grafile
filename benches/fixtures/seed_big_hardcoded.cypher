// =============================================================================
//  seed_big.cypher  -  Large synthetic graph for Neo4j 5.x, pure Cypher
// =============================================================================
//  Domain: e-commerce + social network + jobs + groups/events
//
//  RUN (cypher-shell, each statement runs as its own auto-commit transaction):
//    cypher-shell -a neo4j://localhost:7687 -u neo4j -p <password> -d neo4j \
//                 --format plain -f seed_big.cypher
//
//  Neo4j Browser: not recommended (needs ':auto' before every CALL ... IN TRANSACTIONS).
//
//  DEFAULT SIZE  ->  ~2.3 million nodes, ~13 million relationships
//
//  SIZES ARE HARDCODED (default values below). To resize, find-and-replace these
//  literals across the file:
//    users 200000 | products 100000 | orders 600000 | reviews 400000 | posts 300000
//    comments 600000 | follows 1500000 | likes 1500000 | friends 600000
//    wishlist 400000 | similar 300000 | memberships 500000 | attendances 400000
//    cities 2000 | companies 5000 | brands 2000 | tags 5000 | skills 300
//    groups 5000 | events 30000
//  Replace 200000 with 2000 (and the others proportionally) for a quick smoke test.
//  Keep the batch sizes (IN TRANSACTIONS OF n ROWS) as they are.
//
//  NOTES
//   - Uses rand(), so every run produces a different dataset (Cypher cannot seed it).
//   - Re-running without the wipe section appends duplicates and fails on constraints.
//   - Needs Neo4j 5.x (CALL {} IN TRANSACTIONS, IF NOT EXISTS, fulltext indexes).
// =============================================================================


// Categories are fixed: 12 roots, 120 mid-level, 1200 leaves = 1332 (ids 1..1332)


// -----------------------------------------------------------------------------
// 0b. OPTIONAL WIPE (uncomment to clear an existing database first)
// -----------------------------------------------------------------------------
// MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10000 ROWS;


// -----------------------------------------------------------------------------
// 1. CONSTRAINTS AND INDEXES (created first so MATCH-by-id is fast)
// -----------------------------------------------------------------------------
CREATE CONSTRAINT country_id  IF NOT EXISTS FOR (n:Country)  REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT city_id     IF NOT EXISTS FOR (n:City)     REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT category_id IF NOT EXISTS FOR (n:Category) REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT brand_id    IF NOT EXISTS FOR (n:Brand)    REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT company_id  IF NOT EXISTS FOR (n:Company)  REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT tag_id      IF NOT EXISTS FOR (n:Tag)      REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT skill_id    IF NOT EXISTS FOR (n:Skill)    REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT user_id     IF NOT EXISTS FOR (n:User)     REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT product_id  IF NOT EXISTS FOR (n:Product)  REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT order_id    IF NOT EXISTS FOR (n:Order)    REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT review_id   IF NOT EXISTS FOR (n:Review)   REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT post_id     IF NOT EXISTS FOR (n:Post)     REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT comment_id  IF NOT EXISTS FOR (n:Comment)  REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT group_id    IF NOT EXISTS FOR (n:Group)    REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT event_id    IF NOT EXISTS FOR (n:Event)    REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT user_email  IF NOT EXISTS FOR (n:User)     REQUIRE n.email IS UNIQUE;

CREATE INDEX user_country_idx   IF NOT EXISTS FOR (n:User)    ON (n.country);
CREATE INDEX user_created_idx   IF NOT EXISTS FOR (n:User)    ON (n.createdAt);
CREATE INDEX product_price_idx  IF NOT EXISTS FOR (n:Product) ON (n.price);
CREATE INDEX product_name_idx   IF NOT EXISTS FOR (n:Product) ON (n.name);
CREATE INDEX order_status_idx   IF NOT EXISTS FOR (n:Order)   ON (n.status);
CREATE INDEX order_created_idx  IF NOT EXISTS FOR (n:Order)   ON (n.createdAt);
CREATE INDEX review_rating_idx  IF NOT EXISTS FOR (n:Review)  ON (n.rating);
CREATE INDEX post_created_idx   IF NOT EXISTS FOR (n:Post)    ON (n.createdAt);
CREATE INDEX event_date_idx     IF NOT EXISTS FOR (n:Event)   ON (n.date);

CREATE FULLTEXT INDEX post_text_ft    IF NOT EXISTS FOR (n:Post)    ON EACH [n.text];
CREATE FULLTEXT INDEX review_text_ft  IF NOT EXISTS FOR (n:Review)  ON EACH [n.text];
CREATE FULLTEXT INDEX product_text_ft IF NOT EXISTS FOR (n:Product) ON EACH [n.name, n.description];

CALL db.awaitIndexes(600);


// -----------------------------------------------------------------------------
// 2. REFERENCE DATA
// -----------------------------------------------------------------------------

// 2.1 Countries (20)
UNWIND [
  {id:1,  code:'MX', name:'Mexico',         continent:'North America'},
  {id:2,  code:'US', name:'United States',  continent:'North America'},
  {id:3,  code:'CA', name:'Canada',         continent:'North America'},
  {id:4,  code:'BR', name:'Brazil',         continent:'South America'},
  {id:5,  code:'AR', name:'Argentina',      continent:'South America'},
  {id:6,  code:'CO', name:'Colombia',       continent:'South America'},
  {id:7,  code:'ES', name:'Spain',          continent:'Europe'},
  {id:8,  code:'FR', name:'France',         continent:'Europe'},
  {id:9,  code:'DE', name:'Germany',        continent:'Europe'},
  {id:10, code:'IT', name:'Italy',          continent:'Europe'},
  {id:11, code:'GB', name:'United Kingdom', continent:'Europe'},
  {id:12, code:'NL', name:'Netherlands',    continent:'Europe'},
  {id:13, code:'SE', name:'Sweden',         continent:'Europe'},
  {id:14, code:'JP', name:'Japan',          continent:'Asia'},
  {id:15, code:'KR', name:'South Korea',    continent:'Asia'},
  {id:16, code:'IN', name:'India',          continent:'Asia'},
  {id:17, code:'CN', name:'China',          continent:'Asia'},
  {id:18, code:'AU', name:'Australia',      continent:'Oceania'},
  {id:19, code:'ZA', name:'South Africa',   continent:'Africa'},
  {id:20, code:'NG', name:'Nigeria',        continent:'Africa'}
] AS row
CREATE (:Country {id: row.id, code: row.code, name: row.name, continent: row.continent});

// 2.2 Cities
UNWIND range(1, 2000) AS i
CALL {
  WITH i
  WITH i,
       ['San','Santa','Nuevo','Puerto','Villa','Monte','Rio','Port','Lake','New','East','West','North','South','Gran'] AS pre,
       ['Valle','Cruz','Mar','Sol','Rosa','Verde','Alto','Bello','Campo','Real','Luna','Grande','Pino','Roca','Isla'] AS suf
  MATCH (co:Country {id: 1 + (i % 20)})
  CREATE (c:City {
    id: i,
    name: pre[i % size(pre)] + ' ' + suf[toInteger(rand() * size(suf))] + ' ' + toString(i),
    population: 20000 + toInteger(rand() * 9000000),
    location: point({latitude: -55 + rand() * 120, longitude: -170 + rand() * 340})
  })
  CREATE (c)-[:IN_COUNTRY]->(co)
} IN TRANSACTIONS OF 1000 ROWS;

// 2.3 Category tree: 12 roots -> 120 mid -> 1200 leaves
UNWIND range(1, 12) AS i
CREATE (:Category {
  id: i, level: 1,
  name: ['Electronics','Home','Fashion','Sports','Toys','Books','Beauty','Grocery','Automotive','Garden','Pets','Office'][i-1]
});

UNWIND range(13, 132) AS i
CALL {
  WITH i
  MATCH (parent:Category {id: (i - 13) / 10 + 1})
  CREATE (c:Category {id: i, level: 2, name: parent.name + ' / Group ' + toString(i)})
  CREATE (c)-[:SUBCATEGORY_OF]->(parent)
} IN TRANSACTIONS OF 500 ROWS;

UNWIND range(133, 1332) AS i
CALL {
  WITH i
  MATCH (parent:Category {id: (i - 133) / 10 + 13})
  CREATE (c:Category {id: i, level: 3, name: parent.name + ' / Type ' + toString(i)})
  CREATE (c)-[:SUBCATEGORY_OF]->(parent)
} IN TRANSACTIONS OF 500 ROWS;

// 2.4 Brands
UNWIND range(1, 2000) AS i
CALL {
  WITH i
  WITH i, ['Aero','Nova','Terra','Zen','Volt','Luma','Orbit','Pixel','Ember','Atlas','Echo','Flux','Helix','Kilo','Nimbus'] AS a,
          ['Works','Labs','Co','Gear','Studio','Craft','Tech','House','Forge','Goods'] AS b
  CREATE (:Brand {
    id: i,
    name: a[toInteger(rand() * size(a))] + b[toInteger(rand() * size(b))] + '-' + toString(i),
    founded: 1900 + toInteger(rand() * 125),
    premium: rand() < 0.2
  })
} IN TRANSACTIONS OF 2000 ROWS;

// 2.5 Companies (employers / sellers)
UNWIND range(1, 5000) AS i
CALL {
  WITH i
  WITH i, 1 + toInteger(rand() * 2000) AS cid,
       ['Global','United','Prime','Digital','Blue','Iron','Bright','Silver','Quantum','Pacific'] AS a,
       ['Systems','Logistics','Holdings','Solutions','Industries','Media','Foods','Energy','Retail','Capital'] AS b,
       ['Technology','Retail','Finance','Health','Logistics','Education','Energy','Media','Manufacturing','Food'] AS ind
  MATCH (c:City {id: cid})
  CREATE (co:Company {
    id: i,
    name: a[toInteger(rand() * size(a))] + ' ' + b[toInteger(rand() * size(b))] + ' ' + toString(i),
    industry: ind[toInteger(rand() * size(ind))],
    employees: 5 + toInteger(rand() * rand() * 100000),
    founded: 1950 + toInteger(rand() * 75)
  })
  CREATE (co)-[:LOCATED_IN]->(c)
} IN TRANSACTIONS OF 2000 ROWS;

// 2.6 Tags and Skills
UNWIND range(1, 5000) AS i
CALL { WITH i CREATE (:Tag {id: i, name: 'tag_' + toString(i)}) } IN TRANSACTIONS OF 5000 ROWS;

UNWIND range(1, 300) AS i
CALL {
  WITH i
  WITH i, ['Python','Java','Cypher','SQL','Go','Rust','Design','Sales','Finance','Marketing','Ops','Legal','Data','Cloud','Security'] AS s
  CREATE (:Skill {id: i, name: s[i % size(s)] + ' L' + toString(1 + (i / size(s)))})
} IN TRANSACTIONS OF 1000 ROWS;


// -----------------------------------------------------------------------------
// 3. CORE ENTITIES
// -----------------------------------------------------------------------------

// 3.1 Users  (LIVES_IN a city)
UNWIND range(1, 200000) AS i
CALL {
  WITH i
  WITH i, 1 + toInteger(rand() * 2000) AS cid,
       ['Ana','Luis','Maria','Jose','Sofia','Carlos','Lucia','Diego','Valeria','Miguel','Emma','Liam','Olivia','Noah','Yuki','Hiro','Priya','Arjun','Chloe','Hans'] AS fn,
       ['Garcia','Lopez','Martinez','Hernandez','Smith','Johnson','Brown','Tanaka','Kim','Patel','Silva','Rossi','Muller','Dubois','Nguyen'] AS ln,
       ['active','active','active','inactive','suspended','premium'] AS st
  MATCH (c:City {id: cid})-[:IN_COUNTRY]->(co:Country)
  WITH i, c, co, fn[toInteger(rand() * size(fn))] AS f, ln[toInteger(rand() * size(ln))] AS l, st
  CREATE (u:User {
    id: i,
    uuid: randomUUID(),
    firstName: f,
    lastName: l,
    email: toLower(f) + '.' + toLower(l) + '.' + toString(i) + '@example.com',
    age: 16 + toInteger(rand() * 60),
    status: st[toInteger(rand() * size(st))],
    country: co.code,
    karma: toInteger(rand() * rand() * 10000),
    createdAt: datetime('2018-01-01T00:00:00Z') + duration({seconds: toInteger(rand() * 255000000)})
  })
  CREATE (u)-[:LIVES_IN]->(c)
} IN TRANSACTIONS OF 5000 ROWS;

// 3.2 Products  (IN_CATEGORY leaf, MADE_BY brand, SOLD_BY company)
UNWIND range(1, 100000) AS i
CALL {
  WITH i
  WITH i,
       133 + toInteger(rand() * 1200) AS catId,
       1 + toInteger(rand() * 2000) AS brandId,
       1 + toInteger(rand() * 5000) AS compId,
       ['Ultra','Mega','Eco','Smart','Classic','Pro','Mini','Max','Lite','Prime'] AS a,
       ['Widget','Gadget','Lamp','Chair','Bottle','Backpack','Speaker','Camera','Kettle','Jacket','Tent','Drone','Watch','Keyboard','Blender'] AS b
  MATCH (cat:Category {id: catId})
  MATCH (br:Brand {id: brandId})
  MATCH (co:Company {id: compId})
  CREATE (p:Product {
    id: i,
    sku: 'SKU-' + right('00000000' + toString(i), 8),
    name: a[toInteger(rand() * size(a))] + ' ' + b[toInteger(rand() * size(b))] + ' ' + toString(i),
    description: 'Synthetic product number ' + toString(i) + ' for load testing graph queries.',
    price: round((5 + rand() * rand() * 995) * 100) / 100,
    stock: toInteger(rand() * 1000),
    rating: round((1 + rand() * 4) * 10) / 10,
    active: rand() < 0.95,
    createdAt: date('2019-01-01') + duration({days: toInteger(rand() * 2500)})
  })
  CREATE (p)-[:IN_CATEGORY]->(cat), (p)-[:MADE_BY]->(br), (p)-[:SOLD_BY]->(co)
} IN TRANSACTIONS OF 5000 ROWS;

// 3.3 Orders + line items (PLACED, SHIPPED_TO, CONTAINS with qty/unitPrice, total computed)
UNWIND range(1, 600000) AS i
CALL {
  WITH i
  WITH i,
       1 + toInteger(rand() * 200000) AS uid,
       1 + toInteger(rand() * 2000) AS cid,
       1 + toInteger(rand() * 5) AS nLines,
       ['PENDING','PAID','SHIPPED','DELIVERED','DELIVERED','DELIVERED','CANCELLED','REFUNDED'] AS st,
       ['CARD','CARD','PAYPAL','TRANSFER','CASH','CRYPTO'] AS pay
  MATCH (u:User {id: uid})
  MATCH (c:City {id: cid})
  CREATE (o:Order {
    id: i,
    uuid: randomUUID(),
    status: st[toInteger(rand() * size(st))],
    paymentMethod: pay[toInteger(rand() * size(pay))],
    createdAt: datetime('2020-01-01T00:00:00Z') + duration({seconds: toInteger(rand() * 200000000)}),
    total: 0.0
  })
  CREATE (u)-[:PLACED]->(o), (o)-[:SHIPPED_TO]->(c)
  WITH o, nLines
  UNWIND range(1, nLines) AS k
  WITH o, 1 + toInteger(rand() * 100000) AS pid, 1 + toInteger(rand() * 3) AS qty
  MATCH (p:Product {id: pid})
  CREATE (o)-[:CONTAINS {qty: qty, unitPrice: p.price}]->(p)
  WITH o, sum(qty * p.price) AS t
  SET o.total = round(t * 100) / 100
} IN TRANSACTIONS OF 2000 ROWS;

// 3.4 Reviews  (User -WROTE-> Review -REVIEWS-> Product)
UNWIND range(1, 400000) AS i
CALL {
  WITH i
  WITH i,
       1 + toInteger(rand() * 200000) AS uid,
       1 + toInteger(rand() * 100000) AS pid,
       1 + toInteger(rand() * 5) AS rating,
       ['great','terrible','solid','fragile','fast','slow','elegant','cheap','durable','noisy','lovely','overpriced'] AS adj,
       ['quality','delivery','packaging','battery','design','support','value','fit'] AS noun
  MATCH (u:User {id: uid})
  MATCH (p:Product {id: pid})
  CREATE (r:Review {
    id: i,
    rating: rating,
    text: 'The ' + noun[toInteger(rand() * size(noun))] + ' is ' + adj[toInteger(rand() * size(adj))] +
          ' and the product feels ' + adj[toInteger(rand() * size(adj))] + '.',
    helpfulVotes: toInteger(rand() * rand() * 500),
    verified: rand() < 0.7,
    createdAt: datetime('2020-06-01T00:00:00Z') + duration({seconds: toInteger(rand() * 180000000)})
  })
  CREATE (u)-[:WROTE]->(r), (r)-[:REVIEWS]->(p)
} IN TRANSACTIONS OF 5000 ROWS;

// 3.5 Posts  (AUTHORED_BY user, TAGGED with 1-3 tags)
UNWIND range(1, 300000) AS i
CALL {
  WITH i
  WITH i,
       1 + toInteger(rand() * 200000) AS uid,
       1 + toInteger(rand() * 3) AS nTags,
       ['graphs','coffee','travel','music','football','cooking','startups','ai','movies','gaming','fitness','books'] AS topics,
       ['Just discovered','Hot take on','Anyone else into','Weekend thoughts about','Finally finished','Looking for tips on'] AS openers
  MATCH (u:User {id: uid})
  CREATE (p:Post {
    id: i,
    text: openers[toInteger(rand() * size(openers))] + ' ' + topics[toInteger(rand() * size(topics))] + ' #' + toString(i),
    visibility: CASE WHEN rand() < 0.8 THEN 'public' ELSE 'friends' END,
    createdAt: datetime('2021-01-01T00:00:00Z') + duration({seconds: toInteger(rand() * 150000000)})
  })
  CREATE (u)-[:AUTHORED]->(p)
  WITH p, nTags
  UNWIND range(1, nTags) AS k
  WITH p, 1 + toInteger(rand() * 5000) AS tid
  MATCH (t:Tag {id: tid})
  CREATE (p)-[:TAGGED]->(t)
} IN TRANSACTIONS OF 3000 ROWS;

// 3.6 Comments  (User -WROTE-> Comment -ON-> Post)
UNWIND range(1, 600000) AS i
CALL {
  WITH i
  WITH i, 1 + toInteger(rand() * 200000) AS uid, 1 + toInteger(rand() * 300000) AS pid,
       ['Totally agree','No way','Great point','Source?','Haha','This changed my mind','Thanks for sharing','Not sure about that'] AS txt
  MATCH (u:User {id: uid})
  MATCH (p:Post {id: pid})
  CREATE (c:Comment {
    id: i,
    text: txt[toInteger(rand() * size(txt))],
    createdAt: datetime('2021-06-01T00:00:00Z') + duration({seconds: toInteger(rand() * 140000000)})
  })
  CREATE (u)-[:WROTE]->(c), (c)-[:ON]->(p)
} IN TRANSACTIONS OF 5000 ROWS;

// 3.7 Groups (founded by a user, located in a city) and Events (hosted by a group)
UNWIND range(1, 5000) AS i
CALL {
  WITH i
  WITH i, 1 + toInteger(rand() * 200000) AS uid, 1 + toInteger(rand() * 2000) AS cid,
       ['Hiking','Board Games','Photography','Startups','Running','Cooking','Chess','Cycling','Book Club','Hackers'] AS t
  MATCH (u:User {id: uid})
  MATCH (c:City {id: cid})
  CREATE (g:Group {id: i, name: t[i % size(t)] + ' ' + toString(i), isPrivate: rand() < 0.25})
  CREATE (u)-[:FOUNDED]->(g), (g)-[:BASED_IN]->(c)
} IN TRANSACTIONS OF 1000 ROWS;

UNWIND range(1, 30000) AS i
CALL {
  WITH i
  WITH i, 1 + toInteger(rand() * 5000) AS gid, 1 + toInteger(rand() * 2000) AS cid
  MATCH (g:Group {id: gid})
  MATCH (c:City {id: cid})
  CREATE (e:Event {
    id: i,
    title: 'Meetup #' + toString(i),
    date: date('2021-01-01') + duration({days: toInteger(rand() * 1800)}),
    capacity: 10 + toInteger(rand() * 500)
  })
  CREATE (g)-[:HOSTS]->(e), (e)-[:HELD_IN]->(c)
} IN TRANSACTIONS OF 2000 ROWS;


// -----------------------------------------------------------------------------
// 4. USER-CENTRIC RELATIONSHIPS (per-user fan-out)
// -----------------------------------------------------------------------------

// 4.1 Employment (about 70% of users)
UNWIND range(1, 200000) AS i
CALL {
  WITH i
  WITH i WHERE rand() < 0.7
  WITH i, 1 + toInteger(rand() * 5000) AS cid,
       ['Engineer','Analyst','Manager','Designer','Sales Rep','Director','Intern','Consultant'] AS roles
  MATCH (u:User {id: i})
  MATCH (c:Company {id: cid})
  CREATE (u)-[:WORKS_AT {
    role: roles[toInteger(rand() * size(roles))],
    since: date('2010-01-01') + duration({days: toInteger(rand() * 5000)}),
    salaryBand: 1 + toInteger(rand() * 8)
  }]->(c)
} IN TRANSACTIONS OF 5000 ROWS;

// 4.2 Skills (1-5 per user, with proficiency)
UNWIND range(1, 200000) AS i
CALL {
  WITH i
  WITH i, 1 + toInteger(rand() * 5) AS n
  UNWIND range(1, n) AS k
  WITH i, 1 + toInteger(rand() * 300) AS sid
  MATCH (u:User {id: i})
  MATCH (s:Skill {id: sid})
  CREATE (u)-[:HAS_SKILL {level: 1 + toInteger(rand() * 5), years: toInteger(rand() * 20)}]->(s)
} IN TRANSACTIONS OF 3000 ROWS;

// 4.3 Interests in tags (0-5 per user)
UNWIND range(1, 200000) AS i
CALL {
  WITH i
  WITH i, toInteger(rand() * 6) AS n
  UNWIND (CASE WHEN n = 0 THEN [] ELSE range(1, n) END) AS k
  WITH i, 1 + toInteger(rand() * 5000) AS tid
  MATCH (u:User {id: i})
  MATCH (t:Tag {id: tid})
  CREATE (u)-[:INTERESTED_IN {weight: round(rand() * 100) / 100}]->(t)
} IN TRANSACTIONS OF 3000 ROWS;


// -----------------------------------------------------------------------------
// 5. SOCIAL AND BEHAVIOURAL EDGES (random pairs, never self-loops)
// -----------------------------------------------------------------------------

// 5.1 FOLLOWS
UNWIND range(1, 1500000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 200000) AS a, toInteger(rand() * (200000 - 1)) AS off
  WITH a, 1 + ((a + off) % 200000) AS b
  MATCH (x:User {id: a})
  MATCH (y:User {id: b})
  CREATE (x)-[:FOLLOWS {since: date('2019-01-01') + duration({days: toInteger(rand() * 2500)})}]->(y)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.2 FRIEND_OF
UNWIND range(1, 600000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 200000) AS a, toInteger(rand() * (200000 - 1)) AS off
  WITH a, 1 + ((a + off) % 200000) AS b
  MATCH (x:User {id: a})
  MATCH (y:User {id: b})
  CREATE (x)-[:FRIEND_OF {since: date('2018-01-01') + duration({days: toInteger(rand() * 3000)}), closeness: round(rand() * 100) / 100}]->(y)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.3 LIKES on posts
UNWIND range(1, 1500000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 200000) AS uid, 1 + toInteger(rand() * 300000) AS pid
  MATCH (u:User {id: uid})
  MATCH (p:Post {id: pid})
  CREATE (u)-[:LIKES {at: datetime('2022-01-01T00:00:00Z') + duration({seconds: toInteger(rand() * 120000000)})}]->(p)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.4 REPLY_TO between comments (about 15% of comments reply to an earlier one)
UNWIND range(2, 600000) AS i
CALL {
  WITH i
  WITH i WHERE rand() < 0.15
  WITH i, 1 + toInteger(rand() * (i - 1)) AS parentId
  MATCH (c:Comment {id: i})
  MATCH (p:Comment {id: parentId})
  CREATE (c)-[:REPLY_TO]->(p)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.5 WISHLISTED products
UNWIND range(1, 400000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 200000) AS uid, 1 + toInteger(rand() * 100000) AS pid
  MATCH (u:User {id: uid})
  MATCH (p:Product {id: pid})
  CREATE (u)-[:WISHLISTED {priority: 1 + toInteger(rand() * 5)}]->(p)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.6 SIMILAR_TO between products
UNWIND range(1, 300000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 100000) AS a, toInteger(rand() * (100000 - 1)) AS off
  WITH a, 1 + ((a + off) % 100000) AS b
  MATCH (x:Product {id: a})
  MATCH (y:Product {id: b})
  CREATE (x)-[:SIMILAR_TO {score: round(rand() * 1000) / 1000}]->(y)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.7 Group memberships
UNWIND range(1, 500000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 200000) AS uid, 1 + toInteger(rand() * 5000) AS gid,
       ['member','member','member','member','moderator','admin'] AS roles
  MATCH (u:User {id: uid})
  MATCH (g:Group {id: gid})
  CREATE (u)-[:MEMBER_OF {
    role: roles[toInteger(rand() * size(roles))],
    joined: date('2020-01-01') + duration({days: toInteger(rand() * 2000)})
  }]->(g)
} IN TRANSACTIONS OF 10000 ROWS;

// 5.8 Event attendance
UNWIND range(1, 400000) AS i
CALL {
  WITH i
  WITH 1 + toInteger(rand() * 200000) AS uid, 1 + toInteger(rand() * 30000) AS eid,
       ['going','going','maybe','declined','attended'] AS st
  MATCH (u:User {id: uid})
  MATCH (e:Event {id: eid})
  CREATE (u)-[:RSVP {status: st[toInteger(rand() * size(st))]}]->(e)
} IN TRANSACTIONS OF 10000 ROWS;


// -----------------------------------------------------------------------------
// 6. DERIVED PROPERTIES (denormalised aggregates for realistic analytics queries)
// -----------------------------------------------------------------------------
MATCH (p:Product)<-[:REVIEWS]-(r:Review)
WITH p, avg(r.rating) AS avgRating, count(r) AS nReviews
CALL {
  WITH p, avgRating, nReviews
  SET p.avgRating = round(avgRating * 100) / 100, p.reviewCount = nReviews
} IN TRANSACTIONS OF 10000 ROWS;

MATCH (u:User)-[:PLACED]->(o:Order)
WITH u, count(o) AS nOrders, sum(o.total) AS spent
CALL {
  WITH u, nOrders, spent
  SET u.orderCount = nOrders, u.lifetimeValue = round(spent * 100) / 100
} IN TRANSACTIONS OF 10000 ROWS;


// -----------------------------------------------------------------------------
// 7. SUMMARY
// -----------------------------------------------------------------------------
MATCH (n) RETURN labels(n)[0] AS label, count(*) AS nodes ORDER BY nodes DESC;
MATCH ()-[r]->() RETURN type(r) AS relationship, count(*) AS relationships ORDER BY relationships DESC;

// Sample queries to try afterwards:
//  Top spenders:        MATCH (u:User) RETURN u.firstName, u.lastName, u.lifetimeValue ORDER BY u.lifetimeValue DESC LIMIT 10;
//  Friends of friends:  MATCH (u:User {id: 1})-[:FRIEND_OF*2]-(fof) WHERE fof <> u RETURN count(DISTINCT fof);
//  Also-bought:         MATCH (p:Product {id: 1})<-[:CONTAINS]-(:Order)-[:CONTAINS]->(q:Product) RETURN q.name, count(*) AS n ORDER BY n DESC LIMIT 10;
//  Fulltext:            CALL db.index.fulltext.queryNodes('post_text_ft', 'graphs') YIELD node, score RETURN node.text, score LIMIT 10;

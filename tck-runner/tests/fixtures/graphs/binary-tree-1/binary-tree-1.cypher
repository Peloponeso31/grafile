CREATE (a:A {name: 'a'}),
       (b1:X {name: 'b1'}),
       (b2:X {name: 'b2'})
CREATE (a)-[:KNOWS]->(b1),
       (a)-[:KNOWS]->(b2);

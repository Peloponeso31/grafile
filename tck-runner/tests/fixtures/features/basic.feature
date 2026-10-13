Feature: harness smoke tests

  Scenario: [1] Returning a literal
    Given an empty graph
    When executing query:
      """
      RETURN 1 AS n
      """
    Then the result should be, in any order:
      | n |
      | 1 |
    And no side effects

  Scenario: [2] Binding a parameter and creating a node
    Given an empty graph
    And parameters are:
      | num | 42 |
    When executing query:
      """
      CREATE (n {num: $num})
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes      | 1 |
      | +properties | 1 |

  Scenario: [3] Failing on a syntax error
    Given any graph
    When executing query:
      """
      MATCH
      """
    Then a SyntaxError should be raised at compile time: InvalidSyntax

  Scenario: [4] Reading a named graph
    Given the binary-tree-1 graph
    When executing query:
      """
      MATCH (a:A)-[:KNOWS]->(b)
      RETURN a.name AS a, b.name AS b
      """
    Then the result should be, in any order:
      | a   | b    |
      | 'a' | 'b1' |
      | 'a' | 'b2' |

  Scenario: [5] A deliberate mismatch
    Given an empty graph
    When executing query:
      """
      RETURN 1 AS n
      """
    Then the result should be, in any order:
      | n |
      | 2 |

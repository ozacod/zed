
[
  (namespace_definition)
  (class_specifier)
] @fold

; Only fold block comments (/* ... */), not single-line // comments
((comment) @fold
  (#match? @fold "^/\\*"))



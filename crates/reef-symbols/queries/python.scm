; Reef's tag query for Python (R2.14, ADR 0042). Written from
; tree-sitter-python's queries/tags.scm (MIT, Copyright (c) 2016 Max Brunsfeld),
; extended: decorated definitions take the decorator in their range, and only
; upper-case module-level names are constants. Changing it bumps TAGS_VERSION.
;
; Captures: @definition.<kind> or @reference.<kind> is the occurrence's node
; (a definition's whole range), @name its name. A function directly in a class
; becomes a method in code, not here.

(decorated_definition
  definition: (class_definition
    name: (identifier) @name)) @definition.class

(decorated_definition
  definition: (function_definition
    name: (identifier) @name)) @definition.function

(class_definition
  name: (identifier) @name) @definition.class

(function_definition
  name: (identifier) @name) @definition.function

(module
  (expression_statement
    (assignment
      left: (identifier) @name) @definition.constant)
  (#match? @name "^[A-Z][A-Z0-9_]*$"))

(call
  function: [
    (identifier) @name
    (attribute
      attribute: (identifier) @name)
  ]) @reference.call

(class_definition
  superclasses: (argument_list
    [
      (identifier) @name
      (attribute
        attribute: (identifier) @name)
    ] @reference.class))

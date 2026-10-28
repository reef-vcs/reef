; Reef's tag query for TypeScript, TSX and JavaScript (R2.14, ADR 0042), run
; on both the TypeScript and the TSX grammar. Written from tree-sitter-javascript's
; and tree-sitter-typescript's queries/tags.scm (MIT, Copyright (c) 2014 Max
; Brunsfeld and Copyright (c) 2017 Max Brunsfeld), merged, since the TypeScript crate's
; query holds only TypeScript's additions, and extended with enums, type
; aliases, class fields and type references. Changing it bumps TAGS_VERSION.
;
; Captures: @definition.<kind> or @reference.<kind> is the occurrence's node
; (a definition's whole range), @name its name.

[
  (class_declaration
    name: (_) @name)
  (abstract_class_declaration
    name: (_) @name)
  (class
    name: (_) @name)
] @definition.class

((method_definition
  name: (_) @name) @definition.method
  (#not-eq? @name "constructor"))

[
  (method_signature
    name: (_) @name)
  (abstract_method_signature
    name: (_) @name)
] @definition.method

[
  (function_declaration
    name: (identifier) @name)
  (generator_function_declaration
    name: (identifier) @name)
  (function_signature
    name: (identifier) @name)
  (function_expression
    name: (identifier) @name)
] @definition.function

(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression) (generator_function)]) @definition.function

(interface_declaration
  name: (_) @name) @definition.interface

(enum_declaration
  name: (_) @name) @definition.enum

(type_alias_declaration
  name: (_) @name) @definition.type

[
  (internal_module
    name: (_) @name)
  (module
    name: (_) @name)
] @definition.module

(public_field_definition
  name: (_) @name) @definition.field

(call_expression
  function: [
    (identifier) @name
    (member_expression
      property: (_) @name)
  ]) @reference.call

(new_expression
  constructor: [
    (identifier) @name
    (member_expression
      property: (_) @name)
  ]) @reference.class

(extends_clause
  value: [
    (identifier) @name
    (member_expression
      property: (_) @name)
  ] @reference.class)

(type_identifier) @name @reference.type

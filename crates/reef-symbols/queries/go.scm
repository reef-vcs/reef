; Reef's tag query for Go (R2.14, ADR 0042). Written from tree-sitter-go's
; queries/tags.scm (MIT, Copyright (c) 2014 Max Brunsfeld), extended: a method
; captures its receiver's type as @scope, a type takes its kind from what it
; declares, and top-level constants and variables are definitions. Changing it
; bumps TAGS_VERSION.
;
; Captures: @definition.<kind> or @reference.<kind> is the occurrence's node
; (a definition's whole range), @name its name, @scope the type a definition
; belongs to when the syntax does not nest it there.

(function_declaration
  name: (identifier) @name) @definition.function

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: (_) @scope))
  name: (field_identifier) @name) @definition.method

(type_spec
  name: (type_identifier) @name
  type: (struct_type)) @definition.struct

(type_spec
  name: (type_identifier) @name
  type: (interface_type)) @definition.interface

(type_spec
  name: (type_identifier) @name) @definition.type

(type_alias
  name: (type_identifier) @name) @definition.type

(source_file
  (const_declaration
    (const_spec
      name: (identifier) @name) @definition.constant))

(source_file
  (var_declaration
    (var_spec
      name: (identifier) @name) @definition.variable))

(call_expression
  function: [
    (identifier) @name
    (parenthesized_expression (identifier) @name)
    (selector_expression field: (field_identifier) @name)
    (parenthesized_expression (selector_expression field: (field_identifier) @name))
  ]) @reference.call

(type_identifier) @name @reference.type

; Reef's tag query for Rust (R2.14, ADR 0042). Written from tree-sitter-rust's
; queries/tags.scm (MIT, Copyright (c) 2017 Maxim Sokolov), extended: a method
; in an `impl` captures the impl's self type as @scope (`impl Trait for T`
; gives `T`), kinds follow Reef's, and constants, statics, fields and scoped
; calls are captured. Changing it bumps TAGS_VERSION.
;
; Captures: @definition.<kind> or @reference.<kind> is the occurrence's node
; (a definition's whole range), @name its name, @scope the type a definition
; belongs to when the syntax does not nest it there. Patterns are in priority
; order: of two that capture one name, the first wins.

(impl_item
  type: (_) @scope
  body: (declaration_list
    (function_item
      name: (identifier) @name) @definition.method))

(impl_item
  type: (_) @scope
  body: (declaration_list
    [
      (const_item
        name: (identifier) @name)
      (static_item
        name: (identifier) @name)
    ] @definition.constant))

(impl_item
  type: (_) @scope
  body: (declaration_list
    (type_item
      name: (type_identifier) @name) @definition.type))

(trait_item
  body: (declaration_list
    [
      (function_item
        name: (identifier) @name)
      (function_signature_item
        name: (identifier) @name)
    ] @definition.method))

[
  (struct_item
    name: (type_identifier) @name)
  (union_item
    name: (type_identifier) @name)
] @definition.struct

(enum_item
  name: (type_identifier) @name) @definition.enum

(trait_item
  name: (type_identifier) @name) @definition.interface

(type_item
  name: (type_identifier) @name) @definition.type

(function_item
  name: (identifier) @name) @definition.function

(function_signature_item
  name: (identifier) @name) @definition.function

(mod_item
  name: (identifier) @name) @definition.module

(macro_definition
  name: (identifier) @name) @definition.macro

[
  (const_item
    name: (identifier) @name)
  (static_item
    name: (identifier) @name)
] @definition.constant

(field_declaration
  name: (field_identifier) @name) @definition.field

(call_expression
  function: [
    (identifier) @name
    (field_expression
      field: (field_identifier) @name)
    (scoped_identifier
      name: (identifier) @name)
    (generic_function
      function: [
        (identifier) @name
        (field_expression
          field: (field_identifier) @name)
        (scoped_identifier
          name: (identifier) @name)
      ])
  ]) @reference.call

(macro_invocation
  macro: [
    (identifier) @name
    (scoped_identifier
      name: (identifier) @name)
  ]) @reference.macro

(impl_item
  trait: [
    (type_identifier) @name
    (scoped_type_identifier
      name: (type_identifier) @name)
    (generic_type
      type: (type_identifier) @name)
  ] @reference.implementation)

(impl_item
  type: [
    (type_identifier) @name
    (scoped_type_identifier
      name: (type_identifier) @name)
    (generic_type
      type: (type_identifier) @name)
  ] @reference.type)

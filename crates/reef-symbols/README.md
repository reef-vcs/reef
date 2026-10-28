# reef-symbols

The symbol extractor of **Reef**, version control built for a world where AI agents write most of the code ([ADR 0042](../../docs/adr/0042-symbol-index.md)). Given a file's bytes, it returns the file's definitions and references with their ranges, the innermost definition enclosing each, and a digest of every definition's body.

It reads two sources. Tree-sitter tags come from Reef's own queries for Python, TypeScript, TSX, JavaScript, Go and Rust. Those queries extend the grammars' upstream `tags.scm` so methods on different types stay distinct, and each definition gets a symbol string in SCIP's grammar under the `reef-tags` scheme. SCIP documents an indexer wrote are converted to UTF-8 byte columns and the same shape. The crate depends on no Reef crate, no jj-lib, gRPC or SQLite. The Reef daemon's symbol index is its first user; the phase-four store and phase five's structural merge reuse it. The design document and roadmap are at <https://github.com/reef-vcs/reef>.

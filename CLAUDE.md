# ferro

## Comments in code

This project uses rustdoc conventions for documentation comments. They are wanted:

- `//!` at the top of a crate or module, saying what it is for and how its parts fit.
- `///` on public items: types, traits, functions, methods, fields, variants and macros, and on
  `pub(crate)` items whose contract is not obvious from the signature.
- The usual shape: a one-line summary sentence, a blank line, then detail. Use `# Examples`,
  `# Errors`, `# Panics` and `# Safety` sections where they apply, and intra-doc links such as
  [`Map`] or [`Db::update`].
- Examples in doc comments are doctests, so they must compile. Mark them `no_run` when they need a
  file, a clock or threads, and `ignore` only when they cannot compile on their own.
- Say what an item does and promises, not how it came to be: no issue numbers, measurements, or
  history in doc comments. Link to the guides in `docs/` for longer explanations.

Every `unsafe` block, and every `unsafe impl`, carries a `// SAFETY:` comment directly above it,
saying which invariants make it sound, as clippy's `undocumented_unsafe_blocks` lint expects.

Other inline `//` and `/* */` comments are not wanted: no comments that narrate a line or block of
code, explain why it was added, or restate what it does. That belongs in the commit message or in
`docs/internals/`. Makefile `##` target docs are fine.

`make ci` enforces this: `missing_docs` warns on the `ferro` and `ferro-derive` libraries, clippy
runs with `undocumented_unsafe_blocks` and `missing_safety_doc` across the workspace and the fuzz
crate, and `make doc-check` fails on any rustdoc warning, so all three fail the build.

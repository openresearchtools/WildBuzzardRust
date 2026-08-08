# Wild Buzzard incremental HTML nucleus

`wild_buzzard_html` is a first-party Rust tokenizer and tree builder over `wild_buzzard_dom`.
`Tokenizer::feed` accepts arbitrary UTF-8 string chunks, retains incomplete markup/entity/raw-text
terminators, normalizes HTML newlines, and reports half-open spans with byte plus one-based
line/column positions. Character-token boundaries may differ with feed boundaries; the tree
builder coalesces adjacent DOM text, and split-at-every-boundary tests require identical tree and
error output.

Default limits retain at most 1 MiB for an incomplete markup token, emit at most 4096 attributes
per tag, and keep at most 1024 open elements. Limit violations are structured parse errors with
deterministic recovery. The limits are caller-configurable through `TokenizerLimits`.

Implemented wave-one behavior includes doctypes and basic public/system identifiers, comments and
bogus comments, start/end tags, quoted/unquoted/boolean attributes, first-wins duplicate
attributes, common named and numeric character references (including invalid scalar replacement
and the HTML Windows-1252 control mapping), raw text, RCDATA, plaintext, void elements, implicit
`html`/`head`/`body`, quirks selection, paragraph/list/heading closures, mismatched end-tag stack
recovery, repeated root/body attribute merging, and leading-LF removal for `pre`, `listing`, and
`textarea`.

Insertion-mode whitespace uses exactly the HTML space set (TAB, LF, FF, CR, and SPACE). NBSP and
other Unicode spaces are character data, including before implicit document structure. Mixed
character tokens are split at HTML-space classification boundaries so leading ASCII space can be
ignored without discarding the following non-ASCII character.

## ESR153/html5lib/WPT references inspected

Pinned reference: `firefox/` at `c19b7e89270787889495688244ec6ee8e79288a1` (read-only, never a
build or test input).

- `parser/html/nsHtml5Tokenizer.{h,cpp}` and `nsHtml5TokenizerCppSupplement.h`.
- `parser/html/nsHtml5TreeBuilder.{h,cpp}` and `nsHtml5TreeBuilderCppSupplement.h`.
- `parser/html/nsHtml5DocumentBuilder.{h,cpp}`.
- `parser/html/nsHtml5ElementName.{h,cpp}`.
- `testing/web-platform/tests/html/syntax/parsing/resources/doctype01.dat`.
- `testing/web-platform/tests/html/syntax/parsing/resources/comments01.dat`.
- `testing/web-platform/tests/html/syntax/parsing/resources/entities01.dat` and `entities02.dat`.
- `testing/web-platform/tests/html/syntax/parsing/resources/inbody01.dat`.
- `testing/web-platform/tests/html/syntax/parsing/resources/scriptdata01.dat`.
- `testing/web-platform/tests/html/syntax/parsing/resources/blocks.dat`.
- `testing/web-platform/tests/html/syntax/parsing/resources/adoption01.dat` and `tables01.dat` to
  identify behavior that remains unsupported.
- `testing/web-platform/tests/html/syntax/parsing/newline-normalization-cr-then-lf.html`.
- `testing/web-platform/tests/html/syntax/parsing/ambiguous-ampersand.html`.
- `testing/web-platform/tests/html/syntax/parsing/no-doctype-name.html`.

History inspection included `95d7a5a7b19b` (htmlparser resync) and the parser attribute changes
around `d13776d32131`. The Rust design is new and does not translate the generated Java/C++ parser
line by line.

## Wave-one tests

Tokenizer unit tests cover every possible two-chunk boundary for mixed doctype/comment/tag/entity
input, raw-text end-tag discrimination, first-wins duplicate attributes, quoted/unquoted values
including URL slashes, numeric reference recovery, CRLF/UTF-8 positions, and EOF-in-comment.
`tests/tree_builder.rs` covers every split boundary at the DOM-output level, implicit structure,
doctype modes, RCDATA versus raw text, comments and void elements, repeated root/body attributes,
paragraph/list implied closures, and mismatched nested end tags. It also proves that leading HTML
ASCII space is ignored while a following NBSP reaches body text.

## Explicit gaps

- The named-character-reference table is a small common subset, not the full generated WHATWG
  table or its longest-match/legacy-semicolon behavior.
- Script data escaped/double-escaped states, CDATA states, and the complete tokenizer error taxonomy
  are absent.
- Doctype public/system parsing and quirks identifiers are partial.
- No fragment parsing, context element, encoding sniffing, speculative parsing, parser scripting,
  `document.write`, custom-element reactions, or tokenizer suspension.
- No table insertion modes/foster parenting, active formatting list/adoption agency algorithm,
  templates, framesets, form pointer, or full implied-end-tag/scope families.
- No SVG/MathML foreign-content namespaces, integration points, or foreign attribute adjustment.
- `noscript` does not vary with a scripting flag.
- Parse-error names/positions are stable Wild Buzzard structures but are not yet a complete mapping
  to every html5lib/WPT error label.

This package is integrated into the root workspace after `wild_buzzard_dom`. The engine will feed
decoded response text incrementally; network and encoding ownership remain outside this crate.

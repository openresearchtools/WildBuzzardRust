# Wild Buzzard JavaScript runtime

wild_buzzard_js is the first Rust-native nucleus of Wild Buzzard's JavaScript
and WebAssembly runtime. It is an interpreter, not a mock, compatibility shim,
or wrapper around SpiderMonkey or another native engine. It has no external
dependencies, contains no unsafe code, performs no I/O, and never reads the
ignored Firefox checkout at build or run time.

This is a Wave-2 subset, not an ECMAScript or Firefox-parity claim. Its tested
product target is `x86_64-unknown-linux-gnu`; it does not add platform-specific
runtime code.

## Embedding boundary

The public lifecycle is:

    use wild_buzzard_js::{Engine, RealmOptions, SourceText, ValueSnapshot};

    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let mut context = realm.context();
    let value = context.evaluate(&SourceText::new("example.js", "6 * 7;"))?;
    assert_eq!(context.snapshot(&value)?, ValueSnapshot::Number(42.0));
    let collection = context.collect_garbage()?;
    assert_eq!(collection.reclaimed.total(), 0);

- Engine compiles immutable CompiledScript values and creates realms.
- Realm owns an isolated persistent global environment record, heap, roots,
  and FIFO job queue.
- Context is the only execution and heap-mutation entry point.
- RootedValue is an owned root registration. It never exposes an arena index,
  raw pointer, or unrooted collector reference. Clones share one root, and the
  final drop unregisters it.
- `JsString` owns immutable exact UTF-16 code units, including lone
  surrogates. Symbols are exposed only as `RootedValue`: `Context::symbol`,
  `symbol_description`, `get_property_by_symbol`, and
  `set_property_by_symbol` preserve exact descriptions without exposing an
  arena index, hash, generation, or identity token. `ValueSnapshot` gives
  hosts exact string data and only an opaque Symbol/Object/Function category;
  checked and explicitly lossy UTF-8 conversions cannot be confused with
  semantic equality. Public value, exact property-key, function-name, and
  caught-error-message conversions reject over-limit UTF-16 lengths with a
  structured range error rather than reaching an internal infallible
  conversion.
- HostFunction receives only rooted this and arguments. It has no DOM
  dependency, so generated WebIDL bindings can implement this contract later.
- Job is a deterministic host-job nucleus: FIFO, one-shot, and stop-on-first
  error while leaving later jobs queued.

Heap references use private typed, generation-checked arena handles.
Environment links and closure captures are handles, not Rust reference cycles.
Each swept slot becomes a tombstone on a free list and advances its generation
before reuse. A slot whose `u32` generation is exhausted is permanently
retired, so a stale handle can never alias a later allocation. Root and realm
identities likewise use checked monotonic allocators: exhausted identity space
is permanently refused before an identity can alias.

## Tracing collection

`Context::collect_garbage` runs an explicit, non-moving, stop-the-world
mark/sweep collection. This wave intentionally has no allocation threshold or
implicit collection, making collection points deterministic. A successful call
returns before/after `HeapStatistics`, per-arena live/reusable/retired slot
counters, and per-kind reclamation totals.

The mark roots and strong edges are complete for the implemented heap:

- the intrinsic and global environments owned by the realm;
- every live `RootedValue` registration, including rooted exception values and
  values captured by host callbacks or queued jobs;
- environment outer links and every initialized binding;
- ordinary-object and function prototype links, data-property values, and
  accessor getter/setter functions;
- Symbol-valued property keys, whose generation is validated while tracing;
- sparse Array element descriptors, distinguishing an absent hole from a
  stored `undefined`; and
- every script function's captured lexical environment.

String property names own immutable `JsString` content rather than collector
handles. They are therefore not tracing edges: reclaiming the temporary string
value that produced a key cannot invalidate the key. Symbol keys are different:
they are strong checked heap edges, remain live while installed, and stop being
traced immediately after deletion. Symbol descriptions themselves are owned
exact `JsString` content inside the Symbol record rather than separate heap
string handles.

Host callbacks and jobs are opaque Rust values, but they cannot obtain private
raw heap handles. A callback or job that retains JavaScript data must retain a
`RootedValue`, which is consequently present in the root registry. Dropping the
last clone removes that registration. There are no weak edges in the current
language surface.

Collection is accepted only when the entire realm is idle. A realm-wide entry
counter covers interpreter, host-callback, and job execution across all of its
contexts, because their temporary values and call frames are deliberately not
traced in this collector wave. An in-entry request returns structured
`ActiveExecution`; a reentrant request returns `CollectionInProgress`. A stale
or invalid edge returns `InvalidHeapGraph`, clears partial marks, performs no
sweep, and leaves the collector usable. No unsafe code or movable object
addresses are involved.

## Implemented language surface

The lexer records UTF-8 byte offsets and one-based Unicode-scalar line/column
locations. It supports comments, decimal/radix numeric literals, exact
code-unit string escapes including fixed and braced Unicode escapes,
identifiers, and the tokens needed by this subset. Raw source scalars are
encoded to UTF-16 while escaped lone surrogates remain exact. LF, CR, CRLF,
U+2028, and U+2029 string line continuations contribute no code units and all
four line-terminator characters update source locations. Unsupported legacy
decimal/octal escapes fail explicitly. The
recursive-descent/precedence parser produces a private located AST and performs
early checks for invalid assignment targets, duplicate direct lexical
declarations, lexical/variable declaration collisions at their enclosing
statement-list, loop-head, catch, and function contours, invalid
return/break/continue, missing const initializers, malformed throw, and
duplicate parameters.

The interpreter currently supports:

- undefined, null, booleans, IEEE-754 numbers, immutable strings containing
  arbitrary UTF-16 code-unit sequences, and identity-bearing Symbol
  primitives with absent-or-exact descriptions;
- unary plus, minus, not, and `typeof`; arithmetic plus, minus, multiply, divide,
  and remainder;
- relational, abstract/strict equality, and short-circuit logical operators;
- `let`, `const`, and `var`; simple assignment; block scoping; TDZ and const
  checks; shadowing; persistent top-level declarations; variable hoisting;
  legal variable/function redeclaration; and no reinitialization when a
  hoisted `var` declaration executes;
- expression statements and completion values; blocks; if/else; while;
  do-while; classic three-part `for` statements with empty, expression,
  `var`, `let`, or `const` initializers; and loop-targeting break/continue;
- function declarations/expressions, calls, return, recursion, mutable closure
  capture, dynamic regular-function this, call stacks, variable-scoped direct
  function-body declarations, and lexical nested-block function declarations;
- ordinary object literals; private complete data/accessor descriptors;
  bounded, cycle-safe prototype lookup; receiver-aware get/set; own deletion;
  and semantic rejection distinct from abrupt exceptions;
- `Object.create`, `Object.getPrototypeOf`, `Object.setPrototypeOf`,
  `Object.defineProperty`, `Object.getOwnPropertyDescriptor`, `Object.hasOwn`,
  `Object.preventExtensions`, `Object.isExtensible`,
  `Object.getOwnPropertyNames`, and `Object.getOwnPropertySymbols` for ordinary
  objects;
- callable but non-constructible `Symbol`, `Symbol.prototype.description`,
  `toString`, and `valueOf`; distinct identity despite equal descriptions;
  symbol-keyed descriptor, read, write, own-check, and deletion operations; and
  explicit TypeErrors for implicit Symbol-to-string/number conversion;
- `Reflect.ownKeys`, backed by the same own-key source as the two Object
  filters: canonical indices ascending, other strings in insertion order, and
  Symbols in insertion order. Redefinition retains position, deletion followed
  by reinsertion appends within its category, and Arrays retain sorted elements
  followed by implicit `length`, named strings, and Symbols;
- real function-object `name`, `length`, and `prototype` descriptors; `new`
  evaluation and constructibility checks; prototype-derived `this`
  allocation; and object-return override;
- sparse Arrays, literals with elisions, exact canonical index recognition,
  holes with prototype fallthrough, `length` growth and descending
  truncation, deletion without length change, `Array`, `Array.isArray`, and
  bounded Array-only `push`/`pop` methods;
- shorthand properties, named/computed property access, function properties,
  method-call `this`, and member-property `delete`;
- content-hashed exact string property keys; UTF-16 code-unit string length,
  equality, concatenation, relational ordering, and primitive string index
  access with non-configurable one-unit elements;
- direct IdentifierReference handling for `typeof`: an unresolvable name
  produces `"undefined"`, while an uninitialized lexical binding and every
  non-identifier operand retain normal evaluation and abrupt completion;
- throw, optional catch bindings, catchable engine errors, and
  try/catch/finally abrupt-completion precedence;
- automatic semicolon insertion at EOF, closing braces, and line terminators,
  including the restricted newline after return and forbidden newline after
  throw;
- explicit step and call-depth limits with structured failures;
- deterministic live-Symbol and own-key limits. Bounds and checked arithmetic
  are validated before heap publication or property mutation; enumeration
  validates both its source and result Array size before materializing values;
- reusable compilation, cross-realm value rejection, rooted host callbacks,
  and deterministic jobs.

Each script or function body performs declaration instantiation before
statement execution. Recursively collected `var` names receive an initialized
variable binding once, and direct body function declarations install into that
same variable environment in source order. Executing a `var` declaration does
not reset an existing value. A lexical slot instead has a distinct
Uninitialized state rather than using undefined; closures that read it early
get ReferenceError.

Execution tracks separate lexical- and variable-environment handles. Entering
a block, catch clause, or lexical loop head replaces only the lexical handle,
so nested `var` declarations still target the containing script or function.
A classic `for (let ...; ...; ...)` clones the initialized head bindings before
the first test and again after each normal or continue completion, before the
update expression. Closures consequently retain the correct iteration value;
`const` heads are deliberately not freshened. Loop environments and captured
values use the ordinary traced environment links, so this feature adds no
untraced collector edge. Empty versus value-bearing completion is preserved:
loops return their last non-empty body value, including values carried into a
break or continue completion by an earlier statement.

The persistent realm-global record distinguishes lexical, variable, and host
bindings. A later global `var` or function may redeclare an existing variable
binding, but it cannot replace a lexical or host binding, and declaration
preflight avoids partially installing a conflicting declaration list. This is
an intentionally bounded declarative model, not ECMAScript's complete split
Global Environment Record and global-object property protocol.

## Deliberate gaps and current divergences

Unsupported syntax fails during lexing/parsing, and unsupported runtime
coercions return a structured error. The major gaps are:

- switch, labels, destructuring, default/rest parameters,
  arrow/async/generator functions, classes, and strict-mode directives;
- `for-in` and `for-of`; comma expressions, update operators, and compound
  assignment in classic `for` components; labelled break/continue; and the
  broader expression grammar used by full ECMAScript loops;
- enumerable-only enumeration (`Object.keys`, `for-in`), bigints, private
  names, proxies, typed arrays, Arguments exotics, weak references, and most
  standard built-ins;
- accessor literal syntax, classes/derived constructors, bound functions,
  Reflect methods other than `ownKeys`, `instanceof`, array spread/iterators,
  and generic Array methods;
- the global Symbol registry (`Symbol.for` and `Symbol.keyFor`), well-known
  Symbols such as `Symbol.iterator`, iterator protocols and `@@iterator`
  consumers, boxed Symbol wrapper objects and their `[[SymbolData]]` receiver
  behavior, and Symbol-aware `Object.prototype.toString` behavior. The current
  prototype methods/getter accept primitive Symbols, correctly reject
  `%Symbol.prototype%` itself, and do not yet accept boxed Symbols;
- primitive wrapper objects and primitive-target behavior for the current
  Object methods; `Object.create` property bags; object-to-property-key
  conversion; and exotic prototype hooks;
- complete ECMAScript ToPrimitive, especially object coercion in arithmetic,
  equality, and property keys;
- rope, dependent, inline, Latin-1-compressed, external, atomized, and shared
  string representations; raw WTF-16 source input and UTF-16 diagnostic
  columns; full ECMAScript identifier classification, template/regex literals,
  legacy decimal/octal string escapes, and exact number-to-string formatting;
- strict-mode directive handling and its throwing primitive-property assignment
  behavior; the current assignment evaluator implements only the non-strict
  ignored-write result for primitive string properties;
- global-object property creation and its CanDeclareGlobalVar/
  CanDeclareGlobalFunction checks, object-record restrictions, and deletion
  behavior. Intrinsics live in an outer host environment, so a fresh global
  variable can shadow an intrinsic instead of reusing a global-object
  property. Annex B declaration rewriting is excluded: a `var` matching a
  simple catch parameter is rejected, direct script/function-body functions
  are variable scoped, and nested block functions remain lexical;
- remaining standard intrinsic objects, error constructors/prototypes,
  modules, dynamic import, promises, microtasks, workers, and debugger hooks;
- bytecode, baseline/optimizing JITs, incremental or concurrent collection,
  nursery/generational collection, write barriers, weak edges/finalization,
  compaction, and all WebAssembly decoding/execution;
- automatic allocation-pressure collection, fallible/OOM-aware allocation,
  heap-size limits, memory reporting beyond slot counts, and host-defined
  traceable edges other than `RootedValue` registrations.

## Firefox ESR153 and Test262 evidence

Reference checkout: Firefox ESR153 commit
c19b7e89270787889495688244ec6ee8e79288a1. These files were inspected as
behavioral and architectural references only; none is a build input and no
code was copied:

- js/public/RootingAPI.h — rooted/handle separation and persistent-root
  lifetime constraints.
- js/public/HeapAPI.h — explicit idle/tracing/collecting heap states and the
  invariant that non-idle heap work is observable as busy.
- js/src/gc/RootMarking.cpp — enumeration of active stacks, exact roots,
  persistent roots, contexts, realms, zones, and other runtime-owned roots.
- js/src/gc/Sweeping.cpp and js/src/gc/ArenaList.h — marked-cell retention and
  rebuilding allocation free lists while dead cells are finalized.
- js/src/jsapi-tests/testPersistentRooted.cpp and
  js/src/jsapi-tests/testGCMarking.cpp — persistent-root lifetime and live/dead
  graph tracing assertions.
- js/src/vm/EnvironmentObject.h and js/src/vm/EnvironmentObject.cpp — lexical
  environment chains, call/block/named-function environments, and separation
  of static scope from runtime environments.
- js/src/vm/Interpreter.cpp — CheckLexical, CheckAliasedLexical, InitLexical,
  ThrowSetConst, PushLexicalEnv, PopLexicalEnv, and fresh block environments.
- js/src/frontend/TokenStream.cpp, js/src/frontend/Parser.cpp, and
  js/src/frontend/BytecodeEmitter.cpp — source positions, restricted
  productions, early errors, and the parser/runtime boundary. In particular,
  Parser.cpp's `noteDeclaredName`, `declarationList`, `variableStatement`,
  `doWhileStatement`, `forHeadStart`, and `forStatement`, plus
  BytecodeEmitter.cpp's `emitDeclarationInstantiation`, `emitCStyleFor`,
  `emitDo`, and `emitTypeof`, were inspected for this slice.
- js/src/frontend/ParseContext.h and js/src/frontend/ParseContext.cpp — var
  scope traversal and lexical/variable early-error contours, including
  `tryDeclareVarHelper`.
- js/src/frontend/CForEmitter.h and js/src/frontend/CForEmitter.cpp — classic
  for-loop control-flow ordering, continue placement, and lexical-environment
  freshening before the initial test and before the update.
- js/src/vm/EnvironmentObject.cpp — global declaration conflict preflight,
  especially `CheckGlobalDeclarationConflicts`; Wild Buzzard implements only
  the bounded declarative subset documented above.
- js/src/vm/Interpreter.cpp — `GetNameOperation`, the distinct `Typeof` and
  `TypeofExpr` execution paths, and `FreshenLexicalEnv`/
  `RecreateLexicalEnv` handling.
- js/src/vm/Realm.h, js/src/vm/JSContext.h, and js/src/vm/Stack.h — realm,
  context, and call-frame responsibilities.
- js/public/PropertyDescriptor.h, js/src/vm/ObjectOperations.h,
  js/src/vm/ObjectOperations-inl.h, js/src/vm/JSObject.cpp, and
  js/src/vm/NativeObject.cpp — incomplete descriptor inputs, complete stored
  descriptors, receiver-aware ordinary get/set, deletion, and prototype
  mutation.
- js/src/builtin/Array.h, js/src/builtin/Array.cpp, js/src/vm/StringType.h,
  and js/src/vm/StringType.cpp — the `0..=4294967294` array-index domain,
  sparse holes, non-writable length checks, and descending truncation with
  partial failure.
- js/src/vm/Interpreter.cpp, js/src/vm/PlainObject-inl.h,
  js/src/vm/PlainObject.cpp, and js/src/vm/Stack.cpp — constructor argument
  ordering, constructibility, prototype-derived `this`, and return override.
- js/src/vm/StringType.h, js/src/vm/StringType.cpp, js/src/util/Text.h, and
  js/public/String.h — UTF-16 code-unit semantics, content equality/hash/order,
  ropes and linearization, no-GC borrowed character access, and the
  `(1 << 30) - 2` maximum length.
- js/public/Id.h, js/src/vm/PropertyKey.h, js/src/vm/JSAtomUtils-inl.h, and
  js/src/vm/JSObject.cpp — `ToPropertyKey`, canonical integer names, atomized
  string-key identity, and checked tracing of GC-backed keys.
- js/src/vm/SymbolType.h, js/src/vm/SymbolType.cpp,
  js/src/builtin/Symbol.cpp, js/src/vm/NativeObject.h,
  js/src/vm/NativeObject.cpp, js/src/vm/Iteration.cpp,
  js/src/builtin/Object.cpp, and js/src/builtin/Reflect.cpp — fresh Symbol
  identity, optional descriptions, traced Symbol edges, property storage, and
  the index/string/Symbol own-key category order.
- js/src/frontend/TokenStream.cpp, js/src/builtin/String.cpp,
  js/src/vm/Interpreter-inl.h, dom/base/nsJSUtils.h, and
  dom/bindings/BindingUtils.cpp — surrogate-preserving escapes, one-code-unit
  string elements, and the DOMString/USVString/ByteString boundary.

Full history was used to check why these invariants exist. In particular,
git blame and git log --follow traced the lexical-environment explanation to
45f2e559d8c82, uninitialized lexical checks to 26afff63cf931 and
e717c881aa8de, and the persistent-root ownership warning to 8037e525bfcab.
Descriptor/prototype/Array history was also checked at
af2eb6a564d1ed675cf4500a3b4d43039131099c,
7fdeaf63be08529a8af981b0585fc124ce8e2498,
72cd7aaf3e802c622192eb2dad938cfacc8d39e2,
e26ad8e0973d3b5f166e64fb76cf58f516335680, and
2638f12f8330d78051552fb074e8d092485b040b. Constructor history was checked at
76389186cba9a9e9451364f70c1e7343da58cbf9 and
ef40d39fb711bbc4874732430118da5260823358. String history was checked at
ad7567066bcb66673096c25d3795fa7affa88175 (braced escapes),
33f75c0a53416da625492f010ef2e628a34667c1 (maximum length),
32dbf10f478bef1930be79dcb1f4339e1b987128 (index recognition),
3e6412b573c5eb460dbcff59f50b8af56aebe48f (iterative rope hashing), and
4df664485b3f1052d727088159222810b3ada11a (cross-rope surrogate encoding).
Symbol and own-key history was checked at 702a79f1be8c (identity hashing that
does not derive from an address), 5d127c32e4a0 (separate string and Symbol key
passes), and 60d121260bc6 (stable order after descriptor redefinition).
Declaration and loop history was checked at
84d28b0b7bb3cd87e7c6c6831ea1e6cf8b2d708e (Bug 1216623, evaluating
`for (let ...)` initializers inside the new binding scope),
dd11e4067356f289080581ad021cc6ec16616d85 (Bug 1456404, introducing
`CForEmitter`), aa3219e559a423643abb15091477d9b0e434bea7 (Bug 1341937,
carrying scope information through freshen/recreate operations), and
d54c45bcdd7b2ce0491613fbbe7f2070d9d97262 (Bug 1529439, shared var
redeclaration logic). Name-resolution history was checked at
e7ddf68c02815b57df114467acf0a5ec71cf1c76 (Bug 1341061, refactoring the
NAME-family runtime operations).
Wild Buzzard preserves the observable invariants with a simpler Rust design
rather than translating the C++ representation.

Focused semantic cases were derived from these checked-in tests:

- js/src/tests/test262/language/statements/let/block-local-use-before-initialization-in-prior-statement.js
- js/src/tests/test262/language/statements/let/block-local-closure-get-before-initialization.js
- js/src/tests/test262/language/statements/const/function-local-use-before-initialization-in-declaration-statement.js
- js/src/tests/test262/language/block-scope/shadowing/lookup-from-closure.js
- js/src/tests/test262/language/expressions/addition/S11.6.1_A2.1_T1.js
- js/src/tests/test262/language/expressions/strict-equals/S11.9.4_A4.3.js
- js/src/tests/test262/language/statements/try/completion-values-fn-finally-abrupt.js
- js/src/tests/test262/language/statements/try/scope-catch-param-var-none.js
- js/src/tests/test262/language/expressions/function/scope-name-var-close.js
- js/src/tests/test262/language/statements/block/scope-lex-close.js and
  scope-var-none.js; language/global-code/script-decl-var-collision.js
- js/src/tests/test262/language/statements/for/head-let-bound-names-in-stmt.js,
  head-const-bound-names-in-stmt.js, scope-head-lex-open.js,
  scope-head-lex-close.js, scope-body-lex-open.js, and
  scope-body-lex-boundary.js
- js/src/tests/test262/language/statements/for/head-let-fresh-binding-per-iteration.js
  and head-const-fresh-binding-per-iteration.js
- js/src/tests/test262/language/statements/for/cptn-expr-expr-iter.js,
  cptn-decl-expr-iter.js, and S12.6.3_A11.1_T1.js
- js/src/tests/test262/language/statements/do-while/cptn-normal.js
- js/src/tests/test262/language/expressions/typeof/unresolvable-reference.js,
  get-value-ref-err.js, and get-value.js
- js/src/jit-test/tests/closures/setname-closure.js
- js/src/jit-test/tests/closures/lambda-light-returned.js
- js/src/jit-test/tests/closures/lambdafc.js
- js/src/tests/test262/built-ins/Object/defineProperty/15.2.3.6-4-1.js,
  15.2.3.6-4-2.js through -4-9.js, -4-12.js through -4-20.js, -4-63.js
  through -4-65.js, -4-161.js, and -4-168.js through -4-170.js
- js/src/tests/test262/built-ins/Object/defineProperty/15.2.3.6-3-32.js and
  15.2.3.6-3-129.js — inherited descriptor-object fields
- js/src/tests/test262/built-ins/Reflect/get/return-value-from-receiver.js and
  the Reflect/set receiver, writable, and accessor cases — ordinary operation
  invariants adapted to the currently available Object surface
- js/src/tests/test262/built-ins/Object/setPrototypeOf/success.js and
  set-failure-cycle.js
- js/src/tests/test262/language/expressions/new/ctorExpr-fn-ref-before-args-eval.js,
  ctorExpr-isCtor-after-args-eval.js, and the S11.2.2 constructor-return cases
- js/src/tests/test262/language/expressions/array/S11.1.4_A1.2.js through
  S11.1.4_A1.7.js; js/src/tests/test262/built-ins/Array/S15.4.5.2_A1_T1.js,
  S15.4.5.2_A2_T1.js, and the Array length invalid/truncation cases
- js/src/jit-test/tests/arrays/set-length-sparse-1.js and
  set-length-sparse-2.js
- js/src/tests/test262/language/source-text/6.1.js and
  js/src/tests/test262/staging/sm/String/unicode-braced.js
- js/src/tests/test262/language/expressions/less-than/S11.8.1_A4.12_T1.js
  and the matching greater-than/less-than-or-equal/greater-than-or-equal cases
- js/src/tests/test262/built-ins/String/prototype/at/returns-code-unit.js,
  built-ins/String/numeric-properties.js, and language/types/string/S8.4_A5.js
- js/src/tests/non262/String/well-formed.js,
  js/src/tests/non262/String/utf8-encode.js, and
  js/src/jit-test/tests/basic/max-string-length.js
- js/src/tests/test262/built-ins/Symbol/uniqueness.js,
  invoked-with-new.js, desc-to-string-symbol.js, and
  prototype/description/this-val-symbol.js
- js/src/tests/test262/built-ins/Object/getOwnPropertySymbols/object-contains-symbol-property-with-description.js
  and order-after-define-property.js; the corresponding
  Object/getOwnPropertyNames/order-after-define-property.js case
- js/src/tests/test262/built-ins/Reflect/ownKeys/return-on-corresponding-order.js,
  return-on-corresponding-order-large-index.js, order-after-define-property.js,
  and target-is-symbol-throws.js
- js/src/tests/non262/Object/property-order.js and
  js/src/jit-test/tests/basic/property-enumeration-order.js

The local tests restate observable assertions; they do not import or execute
files from firefox/. A proper metadata-aware Test262 runner and recorded shard
results are not yet implemented.

## Staged roadmap

1. Stabilize this interpreter contract: add the Symbol registry and well-known
   Symbols, optimized string representations, richer references/completions,
   broader standard intrinsics and exotics, iterators, for-in/for-of, broader
   statements/functions, parser recovery, fuzzing, and a pinned Test262
   harness.
2. Lower the AST to verified bytecode with explicit stack maps, interrupt
   checks, source notes, and debugger-safe scopes. Keep the AST interpreter as
   a differential oracle.
3. Evolve the proven stop-the-world collector into an incremental
   nursery/generational design. Add stack maps so safe-point collection can run
   during execution, then add write barriers, weak processing, finalization,
   compaction tests, host tracing hooks, Wasm edges, and OOM-safe allocation.
4. Add modules, promises, async functions, and realm-aware microtask jobs with
   deterministic rejection/error reporting and browser event-loop handoff.
5. Add debugger/profiler hooks, breakpoints, stepping, scope inspection, and
   source maps without exposing mutable collector internals.
6. Add WebAssembly validation and interpretation using the same values,
   rooting, exceptions, jobs, and collector. Grow against the Wasm spec tests
   before adding compilation tiers.
7. Add a baseline and then optimizing JIT. Wild Buzzard's only supported
   platform and ABI target is x86_64-unknown-linux-gnu; JIT executable-memory
   policy, unwind metadata, signal handling, and acceptance evidence will be
   Linux x86-64 only. No Windows, macOS, Android, or mobile backend is planned.

Firefox/Test262 parity can be claimed only after pinned full-suite evidence,
GC/JIT/Wasm stress and differential testing, fuzzing, and integration through
generated DOM bindings. This Wave-2 crate claims none of that completeness.

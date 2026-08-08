# Wild Buzzard JavaScript runtime

wild_buzzard_js is the first Rust-native nucleus of Wild Buzzard's JavaScript
and WebAssembly runtime. It is an interpreter, not a mock, compatibility shim,
or wrapper around SpiderMonkey or another native engine. It has no external
dependencies, contains no unsafe code, performs no I/O, and never reads the
ignored Firefox checkout at build or run time.

This is a Wave-1 subset, not an ECMAScript or Firefox-parity claim.

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
- Realm owns an isolated global lexical environment, heap, roots, and FIFO
  job queue.
- Context is the only execution and heap-mutation entry point.
- RootedValue is an owned root registration. It never exposes an arena index,
  raw pointer, or unrooted collector reference. Clones share one root, and the
  final drop unregisters it.
- ValueSnapshot gives hosts owned primitive data and opaque Object/Function
  categories.
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
- ordinary-object and function own-property values; and
- every script function's captured lexical environment.

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
locations. It supports comments, decimal/radix numeric literals, basic string
escapes, identifiers, and the tokens needed by this subset. The
recursive-descent/precedence parser produces a private located AST and performs
early checks for invalid assignment targets, duplicate direct lexical
declarations, invalid return/break/continue, missing const initializers,
malformed throw, and duplicate parameters.

The interpreter currently supports:

- undefined, null, booleans, IEEE-754 numbers, and strings;
- unary plus, minus, and not; arithmetic plus, minus, multiply, divide,
  and remainder;
- relational, abstract/strict equality, and short-circuit logical operators;
- let/const, simple assignment, block scoping, TDZ checks, const checks,
  shadowing, and persistent top-level lexical bindings;
- expression statements and completion values, blocks, if/else, while, break,
  and continue;
- function declarations/expressions, calls, return, recursion, mutable closure
  capture, dynamic regular-function this, and call stacks;
- ordinary object literals, shorthand properties, named/computed own-property
  reads and writes, function properties, and method-call this;
- throw, optional catch bindings, catchable engine errors, and
  try/catch/finally abrupt-completion precedence;
- automatic semicolon insertion at EOF, closing braces, and line terminators,
  including the restricted newline after return and forbidden newline after
  throw;
- explicit step and call-depth limits with structured failures;
- reusable compilation, cross-realm value rejection, rooted host callbacks,
  and deterministic jobs.

Declaration instantiation happens before statement execution. A lexical slot
therefore has a distinct Uninitialized state rather than using undefined;
closures that read it early get ReferenceError. A block receives a fresh
environment on every entry, including each loop iteration.

## Deliberate gaps and current divergences

Unsupported syntax fails during lexing/parsing, and unsupported runtime
coercions return a structured error. The major gaps are:

- var, for/do/switch, labels, destructuring, default/rest parameters,
  arrow/async/generator functions, classes, and strict-mode directives;
- arrays, prototypes, property descriptors/accessors, constructors/new,
  deletion, enumeration, symbols, bigints, private names, proxies, weak
  references, and most standard built-ins;
- complete ECMAScript ToPrimitive, especially object coercion in arithmetic,
  equality, and property keys;
- UTF-16/WTF-16 string storage and lone-surrogate escapes (storage is UTF-8 in
  this wave), full ECMAScript identifier classification, template/regex
  literals, and exact number-to-string formatting;
- global var/function declaration-object interactions and Annex B behavior;
  direct function declarations are treated as block lexical declarations in
  this wave;
- realms' standard intrinsic objects, error constructors/prototypes, modules,
  dynamic import, promises, microtasks, workers, and debugger hooks;
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
  productions, early errors, and the parser/runtime boundary.
- js/src/vm/Realm.h, js/src/vm/JSContext.h, and js/src/vm/Stack.h — realm,
  context, and call-frame responsibilities.

Full history was used to check why these invariants exist. In particular,
git blame and git log --follow traced the lexical-environment explanation to
45f2e559d8c82, uninitialized lexical checks to 26afff63cf931 and
e717c881aa8de, and the persistent-root ownership warning to 8037e525bfcab.
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
- js/src/jit-test/tests/closures/setname-closure.js
- js/src/jit-test/tests/closures/lambda-light-returned.js
- js/src/jit-test/tests/closures/lambdafc.js

The local tests restate observable assertions; they do not import or execute
files from firefox/. A proper metadata-aware Test262 runner and recorded shard
results are not yet implemented.

## Staged roadmap

1. Stabilize this interpreter contract: add WTF-16 strings, spec algorithms,
   richer references/completions, standard objects and descriptors, arrays,
   iterators, var, broader statements/functions, parser recovery, fuzzing, and
   a pinned Test262 harness.
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
generated DOM bindings. This Wave-1 crate claims none of that completeness.

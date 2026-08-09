# W2-A2J contained Brimstone JIT infrastructure

- Task: Establish the first disabled, bounded, reviewable native-code infrastructure gate on the
  canonical Brimstone engine without exposing generated code to product dispatch or untrusted
  browser content.
- Owner: Agent 2 — JavaScript/WebAssembly; independently audited and integrated by the main
  orchestrator.
- Status: Complete for the contained infrastructure proof. NO-GO for product VM dispatch, DOM
  bindings, untrusted bytecode/content, or a baseline/optimizing-tier parity claim.
- Upstream baselines: Brimstone
  `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; exact Cranelift `0.134.3` source from the imported
  Wasmtime v47.0.3 revision `5554cc1a651da536af2cc46c7324bdc085b162e3` under `js/wasmtime`.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; SpiderMonkey bytecode
  validation, Baseline/Ion frame, safepoint, stack-map, invalidation, exception, and jit-test paths
  remain architecture/behavioral reference only. No SpiderMonkey implementation was copied.
- Wild Buzzard paths changed: Brimstone workspace/package feature manifests and lock; bytecode
  instruction metadata accessors, exhaustive metadata, checked verifier, and module exports; and
  the new `runtime/jit` ABI, hotness/interrupt, executable-cache, and compiler modules.
- Contract added or changed: `baseline_jit` is optional and off by default, rejects non-Linux-x86-64
  builds, and leaves `PRODUCT_DISPATCH_ENABLED` compile-time false. Every one of 151 opcodes now has
  explicit operand use/def, control-flow, and conservative effect metadata. The verifier decodes
  trusted compiler output without `InstructionIterator` pointer casts and applies byte/instruction,
  register/range, constant/cache, flag/enum, canonical-width, branch-boundary, and backedge checks.
- Generated-code boundary: lifetime-branded owners bind activation, shadow-frame schema, and
  initialized raw value slots for one synchronous call. All generated field offsets use
  `offset_of!` and have layout tests. The compiler emits only boxed constants/moves, SMI immediate
  add/sub, forward exact-boolean branches, and return. Non-SMI/overflow, unsupported opcodes, and
  backedges produce an exact verified side-exit offset.
- W^X boundary: executable code is thread-affine and admitted only through a hard byte/entry-budget
  cache. Each mapping is anonymous RW for copying, then `mprotect`ed RX before exposure; no RWX
  phase exists. Entries are evicted before a replacement mapping is created, keeping live mapped
  bytes within the configured cap. Code-call borrows prevent an entry pointer outliving eviction.
- Tests run and results: external locked targets passed 23 default library tests, 45
  `baseline_jit` tests, 46 combined `baseline_jit,gc_stress_test` tests, 45 release JIT tests, and 45
  nightly AddressSanitizer/LeakSanitizer JIT tests. Strict Clippy with warnings denied passed both
  default and JIT configurations. The refreshed integration harness passed 185/0/0 normally and
  179/0 with six configured GC-stress skips; the new primitive-prototype regression passed directly
  with both features and the Test262 host exposed. `git diff --check`, source-artifact, feature-off
  dependency-tree, exact-local-Cranelift, and prohibited-pattern audits passed.
- Independent review corrections: strict equality/not-equality were moved from the simple class to
  an allocating safepoint because rope-string comparison may flatten; Wide/ExtraWide branch tests
  now prove prefix-inclusive bases; all generated ABI offsets were centralized; raw executable
  allocation became test-only; direct libc and the Cranelift registry closure were aligned with the
  selected Wasmtime manifest/lock. The apparent interpreter prefixed-branch mismatch was retracted
  after confirming dispatch decodes at the opcode but branches from the unchanged prefix start.
- Independent review verdict: GO for this contained, disabled infrastructure gate with no remaining
  code blocker; explicitly NO-GO for DOM/untrusted execution, product dispatch, or parity claims.
- Parity evidence: these tests prove a bounded native-code subset and its fail-closed admission
  machinery. They do not show site performance, broad ECMAScript execution, product tiering,
  Firefox parity, or correctness across an allocating native safepoint.
- Known behavioral differences and blockers: side exits are validated ABI records only and cannot
  resume the interpreter. The shadow-frame schema is not registered with Brimstone's GC root walker.
  Generated code has no helpers, calls, relocations, traps, native safepoints/stack maps, exceptions,
  unwind/debugger metadata, invalidation, backedge execution, OSR, property-cache fast path, or
  product dispatch. The verifier is defense in depth for trusted compiler output, not an untrusted
  serialized-bytecode loader; dynamic scope metadata is outside its present inputs. Full Test262,
  fuzzing, and Miri were not run.
- Unsafe or FFI introduced: the cache directly calls Linux `mmap`, `mprotect`, `munmap`, and
  `sysconf` through libc. Generated entry invocation transmutes the owned RX address to the exact
  versioned System V signature inside a branded synchronous call. Both unsafe boundaries are
  isolated in the JIT module and unavailable when the feature is off.
- Dependency/provenance impact: no registry Cranelift or `cranelift-jit` is used. The exact local
  Cranelift closure aligns to Wasmtime's lock, including regalloc2 0.15.1 and hashbrown 0.17.0.
  Compatible shared packages advance to the Wasmtime-required bumpalo 3.20.2, libc 0.2.185, log
  0.4.28, and smallvec 1.15.1. Cranelift retains its Apache-2.0 WITH LLVM-exception license;
  Brimstone remains MIT.
- Provider or network implications: no runtime endpoint or ambient capability is introduced.
  Cargo still needs the separately recorded registry/Git inputs unless an admitted offline vendor
  closure is provided.
- Recommended next action: register a canonical native frame with the root walker, add one stable
  allocating helper and exact interpreter-resume side exit, and prove collection at every resulting
  native safepoint before enabling any backedge or VM dispatch. Then grow calls, exceptions,
  property caches with stable traced identities/invalidation, and interruption in separate gates.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ast::{
    AssignmentTarget, BinaryOperator, DeclarationKind, Expression, ExpressionKind, Function,
    Literal, LogicalOperator, MemberProperty, Statement, StatementKind, UnaryOperator,
};
use crate::error::{DiagnosticLocation, ErrorKind, JsError, JsResult, StackFrame, SyntaxIssue};
use crate::heap::{
    ArenaStatistics as PrivateArenaStatistics, Binding, BindingState, Callable, EnvironmentId,
    FunctionRecord, Heap, HeapArenaStatistics as PrivateHeapArenaStatistics, HostFunctionRecord,
    ObjectKind, ObjectRef, PropertyDescriptor, PropertyKind, RawValue, ReclaimedCounts,
    ScriptFunction, TraceError,
};
use crate::parser;
use crate::source::{SourceSpan, SourceText};

static NEXT_REALM_ID: AtomicU64 = AtomicU64::new(1);

/// Deterministic execution limits applied by a [`Context`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    /// Maximum AST evaluation steps in one outermost entry.
    pub max_steps: u64,
    /// Maximum active JavaScript and host call frames.
    pub max_call_depth: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_steps: 1_000_000,
            max_call_depth: 256,
        }
    }
}

/// Process-level JavaScript engine configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EngineOptions {
    /// Default limits copied into newly created contexts.
    pub limits: ExecutionLimits,
}

#[derive(Debug)]
struct EngineInner {
    options: EngineOptions,
}

/// Process-level owner and compiler entry point.
#[derive(Debug)]
pub struct Engine {
    inner: Rc<EngineInner>,
}

impl Engine {
    /// Creates an engine with deterministic default limits.
    #[must_use]
    pub fn new(options: EngineOptions) -> Self {
        Self {
            inner: Rc::new(EngineInner { options }),
        }
    }

    /// Parses source into an immutable, reusable script.
    ///
    /// # Errors
    ///
    /// Returns a located [`ErrorKind::SyntaxError`] for source outside the
    /// implemented grammar or for an early semantic error.
    pub fn compile(&self, source: &SourceText) -> JsResult<CompiledScript> {
        compile_source(source)
    }

    /// Creates an isolated realm with its own global environment and heap.
    #[must_use]
    pub fn create_realm(&self, options: RealmOptions) -> Realm {
        Realm::new(Rc::clone(&self.inner), options)
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineOptions::default())
    }
}

/// Realm configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealmOptions {
    /// Human-readable realm name used by diagnostics and tooling.
    pub name: String,
}

impl Default for RealmOptions {
    fn default() -> Self {
        Self {
            name: "default".to_owned(),
        }
    }
}

/// Stable identifier for one isolated realm.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RealmId(u64);

struct RealmState {
    heap: Heap,
    intrinsic_environment: EnvironmentId,
    global_environment: EnvironmentId,
    intrinsics: Intrinsics,
}

#[derive(Clone, Copy)]
struct Intrinsics {
    object: ObjectRef,
    function: ObjectRef,
    array: ObjectRef,
}

impl Intrinsics {
    const fn roots(self) -> [RawValue; 3] {
        [
            self.object.as_value(),
            self.function.as_value(),
            self.array.as_value(),
        ]
    }
}

struct RealmCore {
    engine: Rc<EngineInner>,
    id: RealmId,
    name: String,
    state: RefCell<RealmState>,
    roots: RefCell<RootRegistry>,
    jobs: RefCell<VecDeque<Box<dyn Job>>>,
    active_entries: Cell<usize>,
    collection_active: Cell<bool>,
}

/// An ECMAScript realm with isolated globals and heap state.
pub struct Realm {
    core: Rc<RealmCore>,
}

impl Realm {
    fn new(engine: Rc<EngineInner>, options: RealmOptions) -> Self {
        let mut heap = Heap::default();
        let intrinsic_environment = heap.allocate_environment(None);
        {
            let environment = heap
                .environment_mut(intrinsic_environment)
                .expect("freshly allocated intrinsic environment exists");
            environment.bindings.insert(
                "undefined".to_owned(),
                Binding {
                    mutable: false,
                    state: BindingState::Initialized(RawValue::Undefined),
                },
            );
            environment.bindings.insert(
                "NaN".to_owned(),
                Binding {
                    mutable: false,
                    state: BindingState::Initialized(RawValue::Number(f64::NAN)),
                },
            );
            environment.bindings.insert(
                "Infinity".to_owned(),
                Binding {
                    mutable: false,
                    state: BindingState::Initialized(RawValue::Number(f64::INFINITY)),
                },
            );
        }
        let global_environment = heap.allocate_environment(Some(intrinsic_environment));
        let intrinsics = initialize_intrinsics(&mut heap, intrinsic_environment);
        Self {
            core: Rc::new(RealmCore {
                engine,
                id: allocate_realm_id(&NEXT_REALM_ID)
                    .expect("realm identity space exhausted without reusing an identity"),
                name: options.name,
                state: RefCell::new(RealmState {
                    heap,
                    intrinsic_environment,
                    global_environment,
                    intrinsics,
                }),
                roots: RefCell::new(RootRegistry::default()),
                jobs: RefCell::new(VecDeque::new()),
                active_entries: Cell::new(0),
                collection_active: Cell::new(false),
            }),
        }
    }

    /// Returns the realm's stable identifier.
    #[must_use]
    pub fn id(&self) -> RealmId {
        self.core.id
    }

    /// Returns the realm's human-readable name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.core.name
    }

    /// Creates an execution context for this realm.
    #[must_use]
    pub fn context(&self) -> Context {
        Context {
            realm: Rc::clone(&self.core),
            limits: self.core.engine.options.limits,
            steps: 0,
            entry_depth: 0,
            frames: Vec::new(),
            source_stack: Vec::new(),
        }
    }
}

impl fmt::Debug for Realm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Realm")
            .field("id", &self.core.id)
            .field("name", &self.core.name)
            .finish_non_exhaustive()
    }
}

type BuiltinCallback = fn(&mut Context, &RootedValue, &[RootedValue]) -> JsResult<RootedValue>;

fn initialize_intrinsics(heap: &mut Heap, environment: EnvironmentId) -> Intrinsics {
    let object_prototype = heap
        .allocate_object(None, HashMap::new())
        .as_object_ref()
        .expect("ordinary object allocation returns an object");
    let function_prototype = allocate_builtin_function(
        heap,
        "",
        0,
        |context, _, _| Ok(context.undefined()),
        Some(object_prototype),
        false,
    )
    .as_object_ref()
    .expect("function allocation returns an object");
    let array_prototype = heap
        .allocate_array(Some(object_prototype), 0, BTreeMap::new())
        .as_object_ref()
        .expect("array allocation returns an object");
    let intrinsics = Intrinsics {
        object: object_prototype,
        function: function_prototype,
        array: array_prototype,
    };

    let object_constructor = allocate_builtin_function(
        heap,
        "Object",
        1,
        builtin_object_constructor,
        Some(function_prototype),
        true,
    );
    let array_constructor = allocate_builtin_function(
        heap,
        "Array",
        1,
        builtin_array_constructor,
        Some(function_prototype),
        true,
    );
    link_constructor(heap, object_constructor, object_prototype);
    link_constructor(heap, array_constructor, array_prototype);

    install_object_intrinsics(heap, object_constructor, function_prototype);
    install_array_intrinsics(heap, array_constructor, array_prototype, function_prototype);

    let record = heap
        .environment_mut(environment)
        .expect("intrinsic environment remains live during initialization");
    for (name, value) in [("Object", object_constructor), ("Array", array_constructor)] {
        record.bindings.insert(
            name.to_owned(),
            Binding {
                mutable: false,
                state: BindingState::Initialized(value),
            },
        );
    }
    intrinsics
}

fn install_object_intrinsics(
    heap: &mut Heap,
    object_constructor: RawValue,
    function_prototype: ObjectRef,
) {
    for (name, arity, callback) in [
        ("create", 2, builtin_object_create as BuiltinCallback),
        ("getPrototypeOf", 1, builtin_object_get_prototype_of),
        ("setPrototypeOf", 2, builtin_object_set_prototype_of),
        ("defineProperty", 3, builtin_object_define_property),
        (
            "getOwnPropertyDescriptor",
            2,
            builtin_object_get_own_property_descriptor,
        ),
        ("hasOwn", 2, builtin_object_has_own),
        ("preventExtensions", 1, builtin_object_prevent_extensions),
        ("isExtensible", 1, builtin_object_is_extensible),
    ] {
        let function =
            allocate_builtin_function(heap, name, arity, callback, Some(function_prototype), false);
        define_direct_data(heap, object_constructor, name, function, true, false, true);
    }
}

fn install_array_intrinsics(
    heap: &mut Heap,
    array_constructor: RawValue,
    array_prototype: ObjectRef,
    function_prototype: ObjectRef,
) {
    let is_array = allocate_builtin_function(
        heap,
        "isArray",
        1,
        builtin_array_is_array,
        Some(function_prototype),
        false,
    );
    define_direct_data(
        heap,
        array_constructor,
        "isArray",
        is_array,
        true,
        false,
        true,
    );
    for (name, callback) in [
        ("push", builtin_array_push as BuiltinCallback),
        ("pop", builtin_array_pop as BuiltinCallback),
    ] {
        let function = allocate_builtin_function(
            heap,
            name,
            usize::from(name == "push"),
            callback,
            Some(function_prototype),
            false,
        );
        define_direct_data(
            heap,
            array_prototype.as_value(),
            name,
            function,
            true,
            false,
            true,
        );
    }
}

fn allocate_builtin_function(
    heap: &mut Heap,
    name: &str,
    arity: usize,
    callback: BuiltinCallback,
    prototype: Option<ObjectRef>,
    constructible: bool,
) -> RawValue {
    heap.allocate_function(
        Callable::Host(HostFunctionRecord {
            name: name.to_owned(),
            arity,
            callback: Rc::new(callback),
        }),
        prototype,
        constructible,
    )
}

fn link_constructor(heap: &mut Heap, constructor: RawValue, prototype: ObjectRef) {
    define_direct_data(
        heap,
        constructor,
        "prototype",
        prototype.as_value(),
        true,
        false,
        false,
    );
    define_direct_data(
        heap,
        prototype.as_value(),
        "constructor",
        constructor,
        true,
        false,
        true,
    );
}

fn define_direct_data(
    heap: &mut Heap,
    target: RawValue,
    property: &str,
    value: RawValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) {
    let object = target
        .as_object_ref()
        .expect("intrinsic property targets are objects");
    heap.object_data_mut(object)
        .expect("intrinsic property target remains live")
        .properties
        .insert(
            property.to_owned(),
            PropertyDescriptor::data(value, writable, enumerable, configurable),
        );
}

/// Parsed script that can be evaluated repeatedly or in multiple realms.
#[derive(Clone, Debug)]
pub struct CompiledScript {
    program: Arc<crate::ast::Program>,
    source_name: Arc<str>,
}

impl CompiledScript {
    /// Returns the source name captured during compilation.
    #[must_use]
    pub fn source_name(&self) -> &str {
        &self.source_name
    }
}

fn compile_source(source: &SourceText) -> JsResult<CompiledScript> {
    let program =
        parser::parse(source.text()).map_err(|issue| syntax_error(source.name(), issue))?;
    Ok(CompiledScript {
        program: Arc::new(program),
        source_name: source.name_arc(),
    })
}

fn syntax_error(source_name: &str, issue: SyntaxIssue) -> JsError {
    JsError::located(
        ErrorKind::SyntaxError,
        issue.message,
        DiagnosticLocation {
            source_name: source_name.to_owned(),
            span: issue.span,
        },
        Vec::new(),
    )
}

#[derive(Default)]
struct RootRegistry {
    identities: MonotonicId,
    values: BTreeMap<u64, RawValue>,
}

impl RootRegistry {
    fn insert(&mut self, value: RawValue) -> Option<u64> {
        let id = self.identities.allocate()?;
        self.values.insert(id, value);
        Some(id)
    }
}

struct MonotonicId {
    next: Option<u64>,
}

impl MonotonicId {
    const fn new(first: u64) -> Self {
        Self { next: Some(first) }
    }

    fn allocate(&mut self) -> Option<u64> {
        let id = self.next?;
        self.next = id.checked_add(1);
        Some(id)
    }
}

impl Default for MonotonicId {
    fn default() -> Self {
        Self::new(0)
    }
}

fn allocate_realm_id(counter: &AtomicU64) -> Option<RealmId> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .ok()
        .map(RealmId)
}

struct RootRecord {
    realm: Weak<RealmCore>,
    id: u64,
}

impl Drop for RootRecord {
    fn drop(&mut self) {
        if let Some(realm) = self.realm.upgrade() {
            realm.roots.borrow_mut().values.remove(&self.id);
        }
    }
}

/// Owned, traced embedding handle to a JavaScript value.
///
/// Clones share one root registration. No heap address or unrooted collector
/// reference is exposed to the embedder.
#[derive(Clone)]
pub struct RootedValue {
    realm: Rc<RealmCore>,
    root: Rc<RootRecord>,
}

impl RootedValue {
    /// Realm that owns this value.
    #[must_use]
    pub fn realm_id(&self) -> RealmId {
        self.realm.id
    }
}

impl fmt::Debug for RootedValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RootedValue")
            .field("realm_id", &self.realm.id)
            .field("root_id", &self.root.id)
            .finish_non_exhaustive()
    }
}

/// Coarse JavaScript value category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueType {
    /// ECMAScript `undefined`.
    Undefined,
    /// ECMAScript `null`.
    Null,
    /// Boolean primitive.
    Boolean,
    /// IEEE-754 number primitive.
    Number,
    /// String primitive.
    String,
    /// Ordinary object.
    Object,
    /// Callable function object.
    Function,
}

/// Owned primitive snapshot or opaque object/function category.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueSnapshot {
    /// ECMAScript `undefined`.
    Undefined,
    /// ECMAScript `null`.
    Null,
    /// Boolean primitive.
    Boolean(bool),
    /// IEEE-754 number primitive.
    Number(f64),
    /// Owned UTF-8 string snapshot.
    String(String),
    /// Ordinary object without an exposed heap address.
    Object,
    /// Callable function without an exposed heap address.
    Function,
}

impl ValueSnapshot {
    /// Returns the snapshot's JavaScript category.
    #[must_use]
    pub const fn value_type(&self) -> ValueType {
        match self {
            Self::Undefined => ValueType::Undefined,
            Self::Null => ValueType::Null,
            Self::Boolean(_) => ValueType::Boolean,
            Self::Number(_) => ValueType::Number,
            Self::String(_) => ValueType::String,
            Self::Object => ValueType::Object,
            Self::Function => ValueType::Function,
        }
    }
}

/// Provider-neutral callback implemented by a browser host binding.
pub trait HostFunction {
    /// Invokes the callback with rooted `this` and argument values.
    ///
    /// # Errors
    ///
    /// Returns an embedding or script exception to the caller.
    fn call(
        &self,
        context: &mut Context,
        this: &RootedValue,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue>;
}

impl<F> HostFunction for F
where
    F: Fn(&mut Context, &RootedValue, &[RootedValue]) -> JsResult<RootedValue> + 'static,
{
    fn call(
        &self,
        context: &mut Context,
        this: &RootedValue,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        self(context, this, arguments)
    }
}

fn builtin_object_constructor(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_constructor_builtin(arguments)
}

fn builtin_array_constructor(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.array_from_constructor_arguments(arguments)
}

fn builtin_object_create(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_create_builtin(arguments)
}

fn builtin_object_get_prototype_of(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_get_prototype_of_builtin(arguments)
}

fn builtin_object_set_prototype_of(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_set_prototype_of_builtin(arguments)
}

fn builtin_object_define_property(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_define_property_builtin(arguments)
}

fn builtin_object_get_own_property_descriptor(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_get_own_property_descriptor_builtin(arguments)
}

fn builtin_object_has_own(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_has_own_builtin(arguments)
}

fn builtin_object_prevent_extensions(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_prevent_extensions_builtin(arguments)
}

fn builtin_object_is_extensible(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.object_is_extensible_builtin(arguments)
}

fn builtin_array_is_array(
    context: &mut Context,
    _: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.array_is_array_builtin(arguments)
}

fn builtin_array_push(
    context: &mut Context,
    this: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.array_push_builtin(this, arguments)
}

fn builtin_array_pop(
    context: &mut Context,
    this: &RootedValue,
    arguments: &[RootedValue],
) -> JsResult<RootedValue> {
    context.array_pop_builtin(this, arguments)
}

/// One deterministic realm job.
pub trait Job {
    /// Consumes and runs the job.
    ///
    /// # Errors
    ///
    /// Returns an embedding or script error, which stops the current queue
    /// drain while leaving later jobs queued.
    fn run(self: Box<Self>, context: &mut Context) -> JsResult<()>;
}

impl<F> Job for F
where
    F: FnOnce(&mut Context) -> JsResult<()> + 'static,
{
    fn run(self: Box<Self>, context: &mut Context) -> JsResult<()> {
        (*self)(context)
    }
}

/// Failure returned while draining the FIFO job queue.
#[derive(Debug)]
pub struct JobRunError {
    /// Number of jobs completed before the failing job.
    pub completed: usize,
    /// Error returned by the failing job.
    pub error: JsError,
}

impl fmt::Display for JobRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "job queue stopped after {} completed job(s): {}",
            self.completed, self.error
        )
    }
}

impl std::error::Error for JobRunError {}

/// Slot-state counters for one non-moving typed heap arena.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArenaStatistics {
    /// Total slots ever added to this arena.
    pub capacity: usize,
    /// Slots currently containing live allocations.
    pub live: usize,
    /// Tombstoned slots available for generation-advancing reuse.
    pub reusable: usize,
    /// Slots permanently retired after generation exhaustion.
    pub retired: usize,
}

impl From<PrivateArenaStatistics> for ArenaStatistics {
    fn from(statistics: PrivateArenaStatistics) -> Self {
        Self {
            capacity: statistics.capacity,
            live: statistics.live,
            reusable: statistics.free,
            retired: statistics.retired,
        }
    }
}

/// Detailed slot-state counters for every typed heap arena.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeapArenaStatistics {
    /// String arena counters.
    pub strings: ArenaStatistics,
    /// Ordinary-object arena counters.
    pub objects: ArenaStatistics,
    /// Function arena counters.
    pub functions: ArenaStatistics,
    /// Lexical-environment arena counters.
    pub environments: ArenaStatistics,
}

impl From<PrivateHeapArenaStatistics> for HeapArenaStatistics {
    fn from(statistics: PrivateHeapArenaStatistics) -> Self {
        Self {
            strings: statistics.strings.into(),
            objects: statistics.objects.into(),
            functions: statistics.functions.into(),
            environments: statistics.environments.into(),
        }
    }
}

/// Allocation counts reclaimed by one or more garbage collections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReclaimedStatistics {
    /// Reclaimed strings.
    pub strings: usize,
    /// Reclaimed ordinary objects.
    pub objects: usize,
    /// Reclaimed functions.
    pub functions: usize,
    /// Reclaimed lexical environments.
    pub environments: usize,
}

impl ReclaimedStatistics {
    /// Total allocations represented by these counters.
    #[must_use]
    pub const fn total(self) -> usize {
        self.strings
            .saturating_add(self.objects)
            .saturating_add(self.functions)
            .saturating_add(self.environments)
    }
}

impl From<ReclaimedCounts> for ReclaimedStatistics {
    fn from(counts: ReclaimedCounts) -> Self {
        Self {
            strings: counts.strings,
            objects: counts.objects,
            functions: counts.functions,
            environments: counts.environments,
        }
    }
}

/// Current private-heap, root, and collection counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeapStatistics {
    /// Live arena-allocated strings.
    pub strings: usize,
    /// Live arena-allocated ordinary objects.
    pub objects: usize,
    /// Live arena-allocated functions.
    pub functions: usize,
    /// Live arena-allocated lexical environments.
    pub environments: usize,
    /// Embedding root registrations.
    pub roots: usize,
    /// Detailed capacity, tombstone, and retirement counts.
    pub arenas: HeapArenaStatistics,
    /// Number of successful explicit collections.
    pub collections: u64,
    /// Cumulative successful-collection reclamation counts.
    pub total_reclaimed: ReclaimedStatistics,
}

/// Why an explicit collection request could not run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionErrorKind {
    /// JavaScript, a host callback, or a job currently exposes transient values.
    ActiveExecution,
    /// Another collection is already in progress for this realm.
    CollectionInProgress,
    /// A traced live edge contained a stale or otherwise invalid private handle.
    InvalidHeapGraph,
}

/// Structured failure from [`Context::collect_garbage`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionError {
    kind: CollectionErrorKind,
    message: String,
}

impl CollectionError {
    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> CollectionErrorKind {
        self.kind
    }

    /// Returns a human-readable diagnostic without exposing private handles.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CollectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CollectionError {}

/// Before/after diagnostics from one successful explicit collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollectionReport {
    /// Heap state immediately before tracing.
    pub before: HeapStatistics,
    /// Heap state immediately after sweeping.
    pub after: HeapStatistics,
    /// Allocations reclaimed by this collection.
    pub reclaimed: ReclaimedStatistics,
}

#[derive(Clone)]
struct CallFrame {
    stack_frame: StackFrame,
    this_value: RawValue,
}

struct CollectionGuard {
    realm: Rc<RealmCore>,
}

impl Drop for CollectionGuard {
    fn drop(&mut self) {
        self.realm.collection_active.set(false);
    }
}

/// Mutable execution and embedding entry point for one realm.
pub struct Context {
    realm: Rc<RealmCore>,
    limits: ExecutionLimits,
    steps: u64,
    entry_depth: usize,
    frames: Vec<CallFrame>,
    source_stack: Vec<Arc<str>>,
}

const MAX_PROTOTYPE_CHAIN: usize = 1_024;

#[derive(Clone, Copy)]
enum DescriptorUpdateKind {
    Generic,
    Data {
        value: Option<RawValue>,
        writable: Option<bool>,
    },
    Accessor {
        getter: AccessorUpdate,
        setter: AccessorUpdate,
    },
}

#[derive(Clone, Copy)]
enum AccessorUpdate {
    Absent,
    Present(Option<RawValue>),
}

#[derive(Clone, Copy)]
struct DescriptorUpdate {
    kind: DescriptorUpdateKind,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}

impl DescriptorUpdate {
    const fn data(value: RawValue) -> Self {
        Self {
            kind: DescriptorUpdateKind::Data {
                value: Some(value),
                writable: None,
            },
            enumerable: None,
            configurable: None,
        }
    }

    const fn default_data(value: RawValue) -> Self {
        Self {
            kind: DescriptorUpdateKind::Data {
                value: Some(value),
                writable: Some(true),
            },
            enumerable: Some(true),
            configurable: Some(true),
        }
    }
}

impl Context {
    /// Realm executed by this context.
    #[must_use]
    pub fn realm_id(&self) -> RealmId {
        self.realm.id
    }

    /// Returns the current deterministic execution limits.
    #[must_use]
    pub const fn execution_limits(&self) -> ExecutionLimits {
        self.limits
    }

    /// Replaces the limits used by subsequent outermost entries.
    pub const fn set_execution_limits(&mut self, limits: ExecutionLimits) {
        self.limits = limits;
    }

    /// Compiles and evaluates source in this context's realm.
    ///
    /// # Errors
    ///
    /// Returns a structured syntax, runtime, resource-limit, or thrown-value
    /// error.
    pub fn evaluate(&mut self, source: &SourceText) -> JsResult<RootedValue> {
        let script = compile_source(source)?;
        self.evaluate_script(&script)
    }

    /// Evaluates a previously compiled script in this context's realm.
    ///
    /// # Errors
    ///
    /// Returns a structured runtime, resource-limit, or thrown-value error.
    pub fn evaluate_script(&mut self, script: &CompiledScript) -> JsResult<RootedValue> {
        self.begin_entry();
        self.source_stack.push(Arc::clone(&script.source_name));
        let global = self.realm.state.borrow().global_environment;
        let result = self.execute_statement_list(&script.program.statements, global);
        self.source_stack.pop();
        self.end_entry();
        match result {
            Ok(Completion::Normal(value)) => Ok(self.root(value.unwrap_or(RawValue::Undefined))),
            Ok(Completion::Return(_)) => Err(JsError::new(
                ErrorKind::InternalError,
                "parser allowed return at script level",
            )),
            Ok(Completion::Break | Completion::Continue) => Err(JsError::new(
                ErrorKind::InternalError,
                "parser allowed loop control at script level",
            )),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    /// Creates a rooted `undefined` value.
    #[must_use]
    pub fn undefined(&self) -> RootedValue {
        self.root(RawValue::Undefined)
    }

    /// Creates a rooted `null` value.
    #[must_use]
    pub fn null(&self) -> RootedValue {
        self.root(RawValue::Null)
    }

    /// Creates a rooted Boolean value.
    #[must_use]
    pub fn boolean(&self, value: bool) -> RootedValue {
        self.root(RawValue::Boolean(value))
    }

    /// Creates a rooted number value.
    #[must_use]
    pub fn number(&self, value: f64) -> RootedValue {
        self.root(RawValue::Number(value))
    }

    /// Creates a rooted string value.
    #[must_use]
    pub fn string(&self, value: impl Into<Arc<str>>) -> RootedValue {
        let raw = self.realm.state.borrow_mut().heap.allocate_string(value);
        self.root(raw)
    }

    /// Creates a rooted empty ordinary object.
    #[must_use]
    pub fn object(&self) -> RootedValue {
        let mut state = self.realm.state.borrow_mut();
        let prototype = state.intrinsics.object;
        let raw = state.heap.allocate_object(Some(prototype), HashMap::new());
        self.root(raw)
    }

    /// Takes an owned, pointer-free snapshot of a rooted value.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::TypeError`] for a value from another realm, or an
    /// internal error if root/heap validation fails.
    pub fn snapshot(&self, value: &RootedValue) -> JsResult<ValueSnapshot> {
        let raw = self.raw(value)?;
        let state = self.realm.state.borrow();
        Ok(match raw {
            RawValue::Undefined => ValueSnapshot::Undefined,
            RawValue::Null => ValueSnapshot::Null,
            RawValue::Boolean(value) => ValueSnapshot::Boolean(value),
            RawValue::Number(value) => ValueSnapshot::Number(value),
            RawValue::String(id) => ValueSnapshot::String(
                state
                    .heap
                    .string(id)
                    .ok_or_else(invalid_heap_handle)?
                    .to_owned(),
            ),
            RawValue::Object(_) => ValueSnapshot::Object,
            RawValue::Function(_) => ValueSnapshot::Function,
        })
    }

    /// Returns a value's coarse JavaScript category.
    ///
    /// # Errors
    ///
    /// Returns the same realm/root validation errors as [`Self::snapshot`].
    pub fn value_type(&self, value: &RootedValue) -> JsResult<ValueType> {
        Ok(self.snapshot(value)?.value_type())
    }

    /// Defines an embedding-provided global data binding.
    ///
    /// # Errors
    ///
    /// Returns a type error for a cross-realm value or duplicate binding.
    pub fn define_global(
        &mut self,
        name: impl Into<String>,
        value: &RootedValue,
        mutable: bool,
    ) -> JsResult<()> {
        let name = name.into();
        let value = self.raw(value)?;
        let global = self.realm.state.borrow().global_environment;
        let mut state = self.realm.state.borrow_mut();
        let environment = state
            .heap
            .environment_mut(global)
            .ok_or_else(invalid_heap_handle)?;
        if environment.bindings.contains_key(&name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("global binding '{name}' already exists"),
            ));
        }
        environment.bindings.insert(
            name,
            Binding {
                mutable,
                state: BindingState::Initialized(value),
            },
        );
        Ok(())
    }

    /// Defines a provider-neutral host callback as a global function.
    ///
    /// # Errors
    ///
    /// Returns a type error if the requested global name already exists.
    pub fn define_host_function<F>(
        &mut self,
        name: impl Into<String>,
        arity: usize,
        callback: F,
    ) -> JsResult<()>
    where
        F: HostFunction + 'static,
    {
        let name = name.into();
        let mut state = self.realm.state.borrow_mut();
        let prototype = state.intrinsics.function;
        let function = state.heap.allocate_function(
            Callable::Host(HostFunctionRecord {
                name: name.clone(),
                arity,
                callback: Rc::new(callback),
            }),
            Some(prototype),
            false,
        );
        drop(state);
        let rooted = self.root(function);
        self.define_global(name, &rooted, false)
    }

    /// Reads an ordinary, function, or Array property through its prototype chain.
    ///
    /// # Errors
    ///
    /// Returns a realm-validation or JavaScript property-access error.
    pub fn get_property(&mut self, value: &RootedValue, property: &str) -> JsResult<RootedValue> {
        self.begin_entry();
        let result = self
            .raw(value)
            .map_err(|error| self.error_to_thrown(&error, None))
            .and_then(|raw| self.get_property_raw(raw, property, None));
        self.end_entry();
        result
            .map(|raw| self.root(raw))
            .map_err(|thrown| self.thrown_to_error(thrown))
    }

    /// Writes a property with ordinary receiver-aware descriptor semantics.
    ///
    /// # Errors
    ///
    /// Returns a realm-validation or JavaScript property-write error.
    pub fn set_property(
        &mut self,
        value: &RootedValue,
        property: impl Into<String>,
        new_value: &RootedValue,
    ) -> JsResult<()> {
        let target = self.raw(value)?;
        let new_value = self.raw(new_value)?;
        let property = property.into();
        self.begin_entry();
        let result = self.set_property_raw(target, &property, new_value, None);
        self.end_entry();
        match result {
            Ok(true) => Ok(()),
            Ok(false) => Err(JsError::new(
                ErrorKind::TypeError,
                "property assignment was rejected by its descriptor",
            )),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    /// Calls a rooted function. `None` supplies `undefined` as `this`.
    ///
    /// # Errors
    ///
    /// Returns a realm-validation, non-callable, callback, runtime, thrown,
    /// or resource-limit error.
    pub fn call(
        &mut self,
        function: &RootedValue,
        this: Option<&RootedValue>,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let function = self.raw(function)?;
        let this = this.map_or(Ok(RawValue::Undefined), |value| self.raw(value))?;
        let arguments = arguments
            .iter()
            .map(|argument| self.raw(argument))
            .collect::<JsResult<Vec<_>>>()?;
        self.begin_entry();
        let result = self.call_raw(function, this, &arguments, None);
        self.end_entry();
        result
            .map(|raw| self.root(raw))
            .map_err(|thrown| self.thrown_to_error(thrown))
    }

    /// Adds a job to the realm's FIFO queue.
    pub fn enqueue_job<J>(&mut self, job: J)
    where
        J: Job + 'static,
    {
        self.realm.jobs.borrow_mut().push_back(Box::new(job));
    }

    /// Drains jobs in FIFO order.
    ///
    /// A failing job is removed, draining stops, and later jobs remain queued.
    ///
    /// # Errors
    ///
    /// Returns [`JobRunError`] at the first failing job.
    pub fn run_jobs(&mut self) -> Result<usize, JobRunError> {
        let mut completed = 0;
        loop {
            let Some(job) = self.realm.jobs.borrow_mut().pop_front() else {
                return Ok(completed);
            };
            self.begin_entry();
            let result = job.run(self);
            self.end_entry();
            match result {
                Ok(()) => completed += 1,
                Err(error) => return Err(JobRunError { completed, error }),
            }
        }
    }

    /// Number of jobs waiting in this realm.
    #[must_use]
    pub fn pending_job_count(&self) -> usize {
        self.realm.jobs.borrow().len()
    }

    /// Returns heap, root, slot-state, and collection counters.
    #[must_use]
    pub fn heap_statistics(&self) -> HeapStatistics {
        let state = self.realm.state.borrow();
        heap_statistics(&state.heap, self.realm.roots.borrow().values.len())
    }

    /// Runs a non-moving stop-the-world tracing collection at an idle realm
    /// safe point.
    ///
    /// Collection is explicit in this runtime wave; allocation does not
    /// trigger an implicit threshold collection. Roots include both permanent
    /// realm environments and every live [`RootedValue`] registration. Script
    /// closures, environment chains, bindings, and object/function properties
    /// are then traced transitively.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionErrorKind::ActiveExecution`] while any context in
    /// this realm is executing JavaScript, a host callback, or a job. Returns
    /// [`CollectionErrorKind::CollectionInProgress`] for a reentrant request,
    /// or [`CollectionErrorKind::InvalidHeapGraph`] if private handle
    /// validation fails while tracing. A failed trace never sweeps.
    pub fn collect_garbage(&mut self) -> Result<CollectionReport, CollectionError> {
        if self.realm.active_entries.get() != 0 {
            return Err(CollectionError {
                kind: CollectionErrorKind::ActiveExecution,
                message: "garbage collection requires an idle realm safe point".to_owned(),
            });
        }
        if self.realm.collection_active.replace(true) {
            return Err(CollectionError {
                kind: CollectionErrorKind::CollectionInProgress,
                message: "garbage collection is already active for this realm".to_owned(),
            });
        }
        let _guard = CollectionGuard {
            realm: Rc::clone(&self.realm),
        };

        let mut roots: Vec<_> = self.realm.roots.borrow().values.values().copied().collect();
        let embedding_root_count = roots.len();
        let mut state = self.realm.state.borrow_mut();
        let before = heap_statistics(&state.heap, embedding_root_count);
        roots.extend(state.intrinsics.roots());
        let permanent_environments = [state.intrinsic_environment, state.global_environment];
        let collection = state
            .heap
            .collect(&roots, &permanent_environments)
            .map_err(collection_trace_error)?;
        let after = heap_statistics(&state.heap, self.realm.roots.borrow().values.len());
        Ok(CollectionReport {
            before,
            after,
            reclaimed: collection.reclaimed.into(),
        })
    }

    fn root(&self, value: RawValue) -> RootedValue {
        let id = self
            .realm
            .roots
            .borrow_mut()
            .insert(value)
            .expect("root identity space exhausted without reusing an identity");
        RootedValue {
            realm: Rc::clone(&self.realm),
            root: Rc::new(RootRecord {
                realm: Rc::downgrade(&self.realm),
                id,
            }),
        }
    }

    fn raw(&self, value: &RootedValue) -> JsResult<RawValue> {
        if !Rc::ptr_eq(&self.realm, &value.realm) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "value belongs to a different realm",
            ));
        }
        self.realm
            .roots
            .borrow()
            .values
            .get(&value.root.id)
            .copied()
            .ok_or_else(|| JsError::new(ErrorKind::InternalError, "root registration is missing"))
    }

    fn begin_entry(&mut self) {
        if self.entry_depth == 0 {
            self.steps = 0;
        }
        let entry_depth = self
            .entry_depth
            .checked_add(1)
            .expect("context entry depth exhausted");
        let realm_entries = self
            .realm
            .active_entries
            .get()
            .checked_add(1)
            .expect("realm entry depth exhausted");
        self.entry_depth = entry_depth;
        self.realm.active_entries.set(realm_entries);
    }

    fn end_entry(&mut self) {
        self.entry_depth = self
            .entry_depth
            .checked_sub(1)
            .expect("context entry depth underflow");
        let realm_entries = self
            .realm
            .active_entries
            .get()
            .checked_sub(1)
            .expect("realm entry depth underflow");
        self.realm.active_entries.set(realm_entries);
    }
}

fn heap_statistics(heap: &Heap, roots: usize) -> HeapStatistics {
    let (strings, objects, functions, environments) = heap.counts();
    HeapStatistics {
        strings,
        objects,
        functions,
        environments,
        roots,
        arenas: heap.arena_statistics().into(),
        collections: heap.collection_count(),
        total_reclaimed: heap.total_reclaimed().into(),
    }
}

fn collection_trace_error(error: TraceError) -> CollectionError {
    CollectionError {
        kind: CollectionErrorKind::InvalidHeapGraph,
        message: format!(
            "garbage collection found an invalid {} handle in the live heap graph",
            error.kind.name()
        ),
    }
}

fn invalid_heap_handle() -> JsError {
    JsError::new(
        ErrorKind::InternalError,
        "private heap handle failed validation",
    )
}

#[derive(Clone)]
enum ThrownPayload {
    Runtime { kind: ErrorKind, message: String },
    Value(RawValue),
}

#[derive(Clone)]
struct Thrown {
    payload: ThrownPayload,
    location: DiagnosticLocation,
    stack: Vec<StackFrame>,
}

enum Completion {
    Normal(Option<RawValue>),
    Return(RawValue),
    Break,
    Continue,
}

type EvalResult<T> = Result<T, Thrown>;

impl Context {
    fn execute_statement_list(
        &mut self,
        statements: &[Statement],
        environment: EnvironmentId,
    ) -> EvalResult<Completion> {
        self.instantiate_declarations(statements, environment)?;
        let mut value = None;
        for statement in statements {
            match self.execute_statement(statement, environment)? {
                Completion::Normal(Some(next)) => value = Some(next),
                Completion::Normal(None) => {}
                abrupt => return Ok(abrupt),
            }
        }
        Ok(Completion::Normal(value))
    }

    fn instantiate_declarations(
        &mut self,
        statements: &[Statement],
        environment: EnvironmentId,
    ) -> EvalResult<()> {
        let mut declarations = Vec::new();
        for statement in statements {
            match &statement.kind {
                StatementKind::LexicalDeclaration { kind, bindings } => {
                    for binding in bindings {
                        declarations.push((
                            binding.name.clone(),
                            *kind == DeclarationKind::Let,
                            binding.span,
                            None,
                        ));
                    }
                }
                StatementKind::FunctionDeclaration(function) => {
                    let Some(name) = &function.name else {
                        return Err(self.runtime_error(
                            ErrorKind::InternalError,
                            "function declaration has no name",
                            Some(function.span),
                        ));
                    };
                    declarations.push((name.clone(), true, function.span, Some(function.clone())));
                }
                _ => {}
            }
        }

        {
            let state = self.realm.state.borrow();
            let Some(record) = state.heap.environment(environment) else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "lexical environment handle is invalid",
                    None,
                ));
            };
            for (name, _, span, _) in &declarations {
                if record.bindings.contains_key(name) {
                    return Err(self.runtime_error(
                        ErrorKind::SyntaxError,
                        format!("lexical declaration '{name}' conflicts with an existing binding"),
                        Some(*span),
                    ));
                }
            }
        }

        {
            let mut state = self.realm.state.borrow_mut();
            let Some(record) = state.heap.environment_mut(environment) else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "lexical environment handle is invalid",
                    None,
                ));
            };
            for (name, mutable, _, _) in &declarations {
                record.bindings.insert(
                    name.clone(),
                    Binding {
                        mutable: *mutable,
                        state: BindingState::Uninitialized,
                    },
                );
            }
        }

        for (name, _, span, function) in declarations {
            if let Some(function) = function {
                let value = self.allocate_script_function(function, environment);
                self.initialize_binding(environment, &name, value, Some(span))?;
            }
        }
        Ok(())
    }

    fn execute_statement(
        &mut self,
        statement: &Statement,
        environment: EnvironmentId,
    ) -> EvalResult<Completion> {
        self.tick(Some(statement.span))?;
        match &statement.kind {
            StatementKind::Empty | StatementKind::FunctionDeclaration(_) => {
                Ok(Completion::Normal(None))
            }
            StatementKind::Expression(expression) => self
                .evaluate_expression(expression, environment)
                .map(|value| Completion::Normal(Some(value))),
            StatementKind::LexicalDeclaration { bindings, .. } => {
                for binding in bindings {
                    let value = if let Some(initializer) = &binding.initializer {
                        self.evaluate_expression(initializer, environment)?
                    } else {
                        RawValue::Undefined
                    };
                    self.initialize_binding(environment, &binding.name, value, Some(binding.span))?;
                }
                Ok(Completion::Normal(None))
            }
            StatementKind::Block(statements) => {
                let block_environment = self.allocate_environment(Some(environment));
                self.execute_statement_list(statements, block_environment)
            }
            StatementKind::If {
                test,
                consequent,
                alternate,
            } => {
                let condition = self.evaluate_expression(test, environment)?;
                if self.to_boolean(condition) {
                    self.execute_statement(consequent, environment)
                } else if let Some(alternate) = alternate {
                    self.execute_statement(alternate, environment)
                } else {
                    Ok(Completion::Normal(None))
                }
            }
            StatementKind::While { test, body } => {
                let mut value = None;
                loop {
                    let condition = self.evaluate_expression(test, environment)?;
                    if !self.to_boolean(condition) {
                        return Ok(Completion::Normal(value));
                    }
                    match self.execute_statement(body, environment)? {
                        Completion::Normal(Some(next)) => value = Some(next),
                        Completion::Normal(None) | Completion::Continue => {}
                        Completion::Break => return Ok(Completion::Normal(value)),
                        completion @ Completion::Return(_) => return Ok(completion),
                    }
                }
            }
            StatementKind::Break => Ok(Completion::Break),
            StatementKind::Continue => Ok(Completion::Continue),
            StatementKind::Return(expression) => {
                let value = if let Some(expression) = expression {
                    self.evaluate_expression(expression, environment)?
                } else {
                    RawValue::Undefined
                };
                Ok(Completion::Return(value))
            }
            StatementKind::Throw(expression) => {
                let value = self.evaluate_expression(expression, environment)?;
                Err(self.thrown_value(value, Some(statement.span)))
            }
            StatementKind::Try {
                body,
                catch,
                finally,
            } => self.execute_try_statement(body, catch.as_ref(), finally.as_deref(), environment),
        }
    }

    fn execute_try_statement(
        &mut self,
        body: &Statement,
        catch: Option<&crate::ast::CatchClause>,
        finally: Option<&Statement>,
        environment: EnvironmentId,
    ) -> EvalResult<Completion> {
        let mut outcome = self.execute_statement(body, environment);
        if let (Err(thrown), Some(catch)) = (&outcome, catch) {
            let catch_environment = self.allocate_environment(Some(environment));
            if let Some(parameter) = &catch.parameter {
                let value = self.catch_value(thrown);
                self.create_initialized_binding(
                    catch_environment,
                    parameter,
                    true,
                    value,
                    Some(catch.span),
                )?;
            }
            outcome = match &catch.body.kind {
                StatementKind::Block(statements) => {
                    self.execute_statement_list(statements, catch_environment)
                }
                _ => Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "catch body is not a block",
                    Some(catch.span),
                )),
            };
        }
        if let Some(finally) = finally {
            match self.execute_statement(finally, environment) {
                Ok(Completion::Normal(_)) => outcome,
                abrupt => abrupt,
            }
        } else {
            outcome
        }
    }

    fn allocate_environment(&self, outer: Option<EnvironmentId>) -> EnvironmentId {
        self.realm
            .state
            .borrow_mut()
            .heap
            .allocate_environment(outer)
    }

    fn allocate_script_function(&self, function: Function, closure: EnvironmentId) -> RawValue {
        let source_name = self
            .source_stack
            .last()
            .cloned()
            .unwrap_or_else(|| Arc::from("<host>"));
        let mut state = self.realm.state.borrow_mut();
        let function_prototype = state.intrinsics.function;
        let object_prototype = state.intrinsics.object;
        let function = state.heap.allocate_function(
            Callable::Script(ScriptFunction {
                function,
                closure,
                source_name,
            }),
            Some(function_prototype),
            true,
        );
        let prototype = state
            .heap
            .allocate_object(Some(object_prototype), HashMap::new())
            .as_object_ref()
            .expect("constructor prototype allocation returns an object");
        link_constructor(&mut state.heap, function, prototype);
        function
    }

    fn create_initialized_binding(
        &mut self,
        environment: EnvironmentId,
        name: &str,
        mutable: bool,
        value: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<()> {
        let mut state = self.realm.state.borrow_mut();
        let Some(record) = state.heap.environment_mut(environment) else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "lexical environment handle is invalid",
                span,
            ));
        };
        if record.bindings.contains_key(name) {
            return Err(self.runtime_error(
                ErrorKind::SyntaxError,
                format!("binding '{name}' is already declared"),
                span,
            ));
        }
        record.bindings.insert(
            name.to_owned(),
            Binding {
                mutable,
                state: BindingState::Initialized(value),
            },
        );
        Ok(())
    }

    fn initialize_binding(
        &mut self,
        environment: EnvironmentId,
        name: &str,
        value: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<()> {
        let mut state = self.realm.state.borrow_mut();
        let Some(record) = state.heap.environment_mut(environment) else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "lexical environment handle is invalid",
                span,
            ));
        };
        let Some(binding) = record.bindings.get_mut(name) else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                format!("binding '{name}' was not instantiated"),
                span,
            ));
        };
        if matches!(binding.state, BindingState::Initialized(_)) {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                format!("binding '{name}' was initialized twice"),
                span,
            ));
        }
        binding.state = BindingState::Initialized(value);
        Ok(())
    }

    fn get_binding(
        &mut self,
        mut environment: EnvironmentId,
        name: &str,
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        loop {
            let (binding, outer) = {
                let state = self.realm.state.borrow();
                let Some(record) = state.heap.environment(environment) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "lexical environment handle is invalid",
                        span,
                    ));
                };
                (record.bindings.get(name).copied(), record.outer)
            };
            if let Some(binding) = binding {
                return match binding.state {
                    BindingState::Uninitialized => Err(self.runtime_error(
                        ErrorKind::ReferenceError,
                        format!("cannot access '{name}' before initialization"),
                        span,
                    )),
                    BindingState::Initialized(value) => Ok(value),
                };
            }
            let Some(outer) = outer else {
                return Err(self.runtime_error(
                    ErrorKind::ReferenceError,
                    format!("'{name}' is not defined"),
                    span,
                ));
            };
            environment = outer;
        }
    }

    fn set_binding(
        &mut self,
        mut environment: EnvironmentId,
        name: &str,
        value: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<()> {
        loop {
            let (binding, outer) = {
                let state = self.realm.state.borrow();
                let Some(record) = state.heap.environment(environment) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "lexical environment handle is invalid",
                        span,
                    ));
                };
                (record.bindings.get(name).copied(), record.outer)
            };
            if let Some(binding) = binding {
                if matches!(binding.state, BindingState::Uninitialized) {
                    return Err(self.runtime_error(
                        ErrorKind::ReferenceError,
                        format!("cannot access '{name}' before initialization"),
                        span,
                    ));
                }
                if !binding.mutable {
                    return Err(self.runtime_error(
                        ErrorKind::TypeError,
                        format!("assignment to constant binding '{name}'"),
                        span,
                    ));
                }
                let mut state = self.realm.state.borrow_mut();
                let Some(record) = state.heap.environment_mut(environment) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "lexical environment handle is invalid",
                        span,
                    ));
                };
                let Some(binding) = record.bindings.get_mut(name) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "resolved binding disappeared",
                        span,
                    ));
                };
                binding.state = BindingState::Initialized(value);
                return Ok(());
            }
            let Some(outer) = outer else {
                return Err(self.runtime_error(
                    ErrorKind::ReferenceError,
                    format!("'{name}' is not defined"),
                    span,
                ));
            };
            environment = outer;
        }
    }
}

impl Context {
    fn evaluate_expression(
        &mut self,
        expression: &Expression,
        environment: EnvironmentId,
    ) -> EvalResult<RawValue> {
        self.tick(Some(expression.span))?;
        match &expression.kind {
            ExpressionKind::Literal(literal) => Ok(match literal {
                Literal::Null => RawValue::Null,
                Literal::Boolean(value) => RawValue::Boolean(*value),
                Literal::Number(value) => RawValue::Number(*value),
                Literal::String(value) => self
                    .realm
                    .state
                    .borrow_mut()
                    .heap
                    .allocate_string(Arc::<str>::from(value.as_str())),
            }),
            ExpressionKind::Identifier(name) => {
                self.get_binding(environment, name, Some(expression.span))
            }
            ExpressionKind::This => Ok(self
                .frames
                .last()
                .map_or(RawValue::Undefined, |frame| frame.this_value)),
            ExpressionKind::Unary { operator, operand } => {
                let value = self.evaluate_expression(operand, environment)?;
                match operator {
                    UnaryOperator::Plus => self
                        .to_number(value, Some(expression.span))
                        .map(RawValue::Number),
                    UnaryOperator::Minus => self
                        .to_number(value, Some(expression.span))
                        .map(|number| RawValue::Number(-number)),
                    UnaryOperator::Not => Ok(RawValue::Boolean(!self.to_boolean(value))),
                }
            }
            ExpressionKind::Binary {
                operator,
                left,
                right,
            } => {
                let left = self.evaluate_expression(left, environment)?;
                let right = self.evaluate_expression(right, environment)?;
                self.evaluate_binary(*operator, left, right, Some(expression.span))
            }
            ExpressionKind::Logical {
                operator,
                left,
                right,
            } => {
                let left = self.evaluate_expression(left, environment)?;
                match operator {
                    LogicalOperator::And if !self.to_boolean(left) => Ok(left),
                    LogicalOperator::Or if self.to_boolean(left) => Ok(left),
                    LogicalOperator::And | LogicalOperator::Or => {
                        self.evaluate_expression(right, environment)
                    }
                }
            }
            ExpressionKind::Assignment { target, value } => {
                self.evaluate_assignment(target, value, environment, expression.span)
            }
            ExpressionKind::Function(function) => {
                self.evaluate_function_expression(function, environment)
            }
            ExpressionKind::Object(properties) => {
                self.evaluate_object_literal(properties, environment)
            }
            ExpressionKind::Array(elements) => {
                self.evaluate_array_literal(elements, environment, expression.span)
            }
            ExpressionKind::Member { object, property } => {
                let object = self.evaluate_expression(object, environment)?;
                let property = self.evaluate_member_property(property, environment)?;
                self.get_property_raw(object, &property, Some(expression.span))
            }
            ExpressionKind::Delete { object, property } => {
                let object = self.evaluate_expression(object, environment)?;
                let property = self.evaluate_member_property(property, environment)?;
                self.delete_property_raw(object, &property, Some(expression.span))
                    .map(RawValue::Boolean)
            }
            ExpressionKind::Call { callee, arguments } => {
                self.evaluate_call(callee, arguments, environment, expression.span)
            }
            ExpressionKind::Construct { callee, arguments } => {
                let callee = self.evaluate_expression(callee, environment)?;
                let mut evaluated_arguments = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    evaluated_arguments.push(self.evaluate_expression(argument, environment)?);
                }
                self.construct_raw(callee, &evaluated_arguments, Some(expression.span))
            }
        }
    }

    fn evaluate_object_literal(
        &mut self,
        properties: &[crate::ast::ObjectProperty],
        environment: EnvironmentId,
    ) -> EvalResult<RawValue> {
        let mut object_properties = HashMap::new();
        for property in properties {
            let value = self.evaluate_expression(&property.value, environment)?;
            object_properties.insert(
                property.key.clone(),
                PropertyDescriptor::default_data(value),
            );
        }
        let mut state = self.realm.state.borrow_mut();
        let prototype = state.intrinsics.object;
        Ok(state
            .heap
            .allocate_object(Some(prototype), object_properties))
    }

    fn evaluate_array_literal(
        &mut self,
        elements: &[Option<Expression>],
        environment: EnvironmentId,
        span: SourceSpan,
    ) -> EvalResult<RawValue> {
        let mut values = BTreeMap::new();
        for (index, element) in elements.iter().enumerate() {
            if let Some(element) = element {
                let value = self.evaluate_expression(element, environment)?;
                let index = u32::try_from(index).map_err(|_| {
                    self.runtime_error(
                        ErrorKind::RangeError,
                        "array literal exceeds the supported length range",
                        Some(span),
                    )
                })?;
                values.insert(index, PropertyDescriptor::default_data(value));
            }
        }
        let length = u32::try_from(elements.len()).map_err(|_| {
            self.runtime_error(
                ErrorKind::RangeError,
                "array literal exceeds the supported length range",
                Some(span),
            )
        })?;
        let mut state = self.realm.state.borrow_mut();
        let prototype = state.intrinsics.array;
        Ok(state.heap.allocate_array(Some(prototype), length, values))
    }

    fn evaluate_assignment(
        &mut self,
        target: &AssignmentTarget,
        expression: &Expression,
        environment: EnvironmentId,
        span: SourceSpan,
    ) -> EvalResult<RawValue> {
        match target {
            AssignmentTarget::Identifier(name) => {
                let value = self.evaluate_expression(expression, environment)?;
                self.set_binding(environment, name, value, Some(span))?;
                Ok(value)
            }
            AssignmentTarget::Member { object, property } => {
                let object = self.evaluate_expression(object, environment)?;
                let property = self.evaluate_member_property(property, environment)?;
                let value = self.evaluate_expression(expression, environment)?;
                let _ = self.set_property_raw(object, &property, value, Some(span))?;
                Ok(value)
            }
        }
    }

    fn evaluate_function_expression(
        &mut self,
        function: &Function,
        environment: EnvironmentId,
    ) -> EvalResult<RawValue> {
        if let Some(name) = &function.name {
            let named_environment = self.allocate_environment(Some(environment));
            let value = self.allocate_script_function(function.clone(), named_environment);
            self.create_initialized_binding(
                named_environment,
                name,
                false,
                value,
                Some(function.span),
            )?;
            Ok(value)
        } else {
            Ok(self.allocate_script_function(function.clone(), environment))
        }
    }

    fn evaluate_call(
        &mut self,
        callee: &Expression,
        arguments: &[Expression],
        environment: EnvironmentId,
        span: SourceSpan,
    ) -> EvalResult<RawValue> {
        let (function, this) = if let ExpressionKind::Member { object, property } = &callee.kind {
            let object = self.evaluate_expression(object, environment)?;
            let property = self.evaluate_member_property(property, environment)?;
            let function = self.get_property_raw(object, &property, Some(callee.span))?;
            (function, object)
        } else {
            (
                self.evaluate_expression(callee, environment)?,
                RawValue::Undefined,
            )
        };
        let mut evaluated_arguments = Vec::with_capacity(arguments.len());
        for argument in arguments {
            evaluated_arguments.push(self.evaluate_expression(argument, environment)?);
        }
        self.call_raw(function, this, &evaluated_arguments, Some(span))
    }

    fn evaluate_member_property(
        &mut self,
        property: &MemberProperty,
        environment: EnvironmentId,
    ) -> EvalResult<String> {
        match property {
            MemberProperty::Named(name) => Ok(name.clone()),
            MemberProperty::Computed(expression) => {
                let value = self.evaluate_expression(expression, environment)?;
                self.to_property_key(value, Some(expression.span))
            }
        }
    }

    fn evaluate_binary(
        &mut self,
        operator: BinaryOperator,
        left: RawValue,
        right: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        match operator {
            BinaryOperator::Add => self.add(left, right, span),
            BinaryOperator::Subtract => Ok(RawValue::Number(
                self.to_number(left, span)? - self.to_number(right, span)?,
            )),
            BinaryOperator::Multiply => Ok(RawValue::Number(
                self.to_number(left, span)? * self.to_number(right, span)?,
            )),
            BinaryOperator::Divide => Ok(RawValue::Number(
                self.to_number(left, span)? / self.to_number(right, span)?,
            )),
            BinaryOperator::Remainder => Ok(RawValue::Number(
                self.to_number(left, span)? % self.to_number(right, span)?,
            )),
            BinaryOperator::LessThan
            | BinaryOperator::LessThanOrEqual
            | BinaryOperator::GreaterThan
            | BinaryOperator::GreaterThanOrEqual => self.relational(operator, left, right, span),
            BinaryOperator::StrictEqual => Ok(RawValue::Boolean(self.strict_equal(left, right))),
            BinaryOperator::StrictNotEqual => {
                Ok(RawValue::Boolean(!self.strict_equal(left, right)))
            }
            BinaryOperator::Equal => self
                .abstract_equal(left, right, span)
                .map(RawValue::Boolean),
            BinaryOperator::NotEqual => self
                .abstract_equal(left, right, span)
                .map(|equal| RawValue::Boolean(!equal)),
        }
    }
}

impl Context {
    fn get_property_raw(
        &mut self,
        value: RawValue,
        property: &str,
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        match value {
            RawValue::Object(id) => {
                self.get_from_object(ObjectRef::Object(id), value, property, span)
            }
            RawValue::Function(id) => {
                self.get_from_object(ObjectRef::Function(id), value, property, span)
            }
            RawValue::String(id) if property == "length" => {
                let state = self.realm.state.borrow();
                let Some(string) = state.heap.string(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "string handle is invalid",
                        span,
                    ));
                };
                Ok(RawValue::Number(usize_to_number(
                    string.encode_utf16().count(),
                )))
            }
            RawValue::Null | RawValue::Undefined => Err(self.runtime_error(
                ErrorKind::TypeError,
                format!("cannot read property '{property}' of {}", value.type_name()),
                span,
            )),
            RawValue::Boolean(_) | RawValue::Number(_) | RawValue::String(_) => {
                Ok(RawValue::Undefined)
            }
        }
    }

    fn get_from_object(
        &mut self,
        mut object: ObjectRef,
        receiver: RawValue,
        property: &str,
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        let mut visited = HashSet::new();
        for _ in 0..MAX_PROTOTYPE_CHAIN {
            if !visited.insert(object) {
                return Err(self.prototype_chain_error(span));
            }
            if let Some(descriptor) = self.own_property_descriptor(object, property, span)? {
                return match descriptor.kind {
                    PropertyKind::Data { value, .. } => Ok(value),
                    PropertyKind::Accessor { getter: None, .. } => Ok(RawValue::Undefined),
                    PropertyKind::Accessor {
                        getter: Some(getter),
                        ..
                    } => self.call_raw(getter, receiver, &[], span),
                };
            }
            let Some(prototype) = self.prototype_of(object, span)? else {
                return Ok(RawValue::Undefined);
            };
            object = prototype;
        }
        Err(self.prototype_chain_error(span))
    }

    fn has_property_raw(
        &self,
        mut object: ObjectRef,
        property: &str,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        let mut visited = HashSet::new();
        for _ in 0..MAX_PROTOTYPE_CHAIN {
            if !visited.insert(object) {
                return Err(self.prototype_chain_error(span));
            }
            if self
                .own_property_descriptor(object, property, span)?
                .is_some()
            {
                return Ok(true);
            }
            let Some(prototype) = self.prototype_of(object, span)? else {
                return Ok(false);
            };
            object = prototype;
        }
        Err(self.prototype_chain_error(span))
    }

    fn set_property_raw(
        &mut self,
        target: RawValue,
        property: &str,
        value: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        let Some(receiver) = target.as_object_ref() else {
            return Err(self.runtime_error(
                ErrorKind::TypeError,
                format!("cannot set property on {}", target.type_name()),
                span,
            ));
        };

        let mut holder = receiver;
        let mut visited = HashSet::new();
        let inherited = loop {
            if visited.len() >= MAX_PROTOTYPE_CHAIN || !visited.insert(holder) {
                return Err(self.prototype_chain_error(span));
            }
            if let Some(descriptor) = self.own_property_descriptor(holder, property, span)? {
                break Some(descriptor);
            }
            let Some(prototype) = self.prototype_of(holder, span)? else {
                break None;
            };
            holder = prototype;
        };

        match inherited.map(|descriptor| descriptor.kind) {
            Some(
                PropertyKind::Accessor { setter: None, .. }
                | PropertyKind::Data {
                    writable: false, ..
                },
            ) => Ok(false),
            Some(PropertyKind::Accessor {
                setter: Some(setter),
                ..
            }) => {
                self.call_raw(setter, target, &[value], span)?;
                Ok(true)
            }
            Some(PropertyKind::Data { writable: true, .. }) | None => {
                match self.own_property_descriptor(receiver, property, span)? {
                    Some(PropertyDescriptor {
                        kind:
                            PropertyKind::Accessor { .. }
                            | PropertyKind::Data {
                                writable: false, ..
                            },
                        ..
                    }) => Ok(false),
                    Some(_) => self.define_own_property(
                        receiver,
                        property,
                        DescriptorUpdate::data(value),
                        span,
                    ),
                    None => self.define_own_property(
                        receiver,
                        property,
                        DescriptorUpdate::default_data(value),
                        span,
                    ),
                }
            }
        }
    }

    fn delete_property_raw(
        &mut self,
        target: RawValue,
        property: &str,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        let Some(object) = target.as_object_ref() else {
            return match target {
                RawValue::Null | RawValue::Undefined => Err(self.runtime_error(
                    ErrorKind::TypeError,
                    format!("cannot delete property of {}", target.type_name()),
                    span,
                )),
                RawValue::String(_) if property == "length" => Ok(false),
                _ => Ok(true),
            };
        };
        let Some(descriptor) = self.own_property_descriptor(object, property, span)? else {
            return Ok(true);
        };
        if !descriptor.configurable {
            return Ok(false);
        }

        let mut state = self.realm.state.borrow_mut();
        match object {
            ObjectRef::Object(id) => {
                let Some(record) = state.heap.object_mut(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "object handle is invalid",
                        span,
                    ));
                };
                if let ObjectKind::Array(array) = &mut record.kind
                    && let Some(index) = canonical_array_index(property)
                {
                    array.elements.remove(&index);
                } else {
                    record.data.properties.remove(property);
                }
            }
            ObjectRef::Function(id) => {
                let Some(record) = state.heap.function_mut(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "function handle is invalid",
                        span,
                    ));
                };
                record.data.properties.remove(property);
            }
        }
        Ok(true)
    }

    fn own_property_descriptor(
        &self,
        object: ObjectRef,
        property: &str,
        span: Option<SourceSpan>,
    ) -> EvalResult<Option<PropertyDescriptor>> {
        let state = self.realm.state.borrow();
        match object {
            ObjectRef::Object(id) => {
                let Some(record) = state.heap.object(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "object handle is invalid",
                        span,
                    ));
                };
                if let ObjectKind::Array(array) = &record.kind {
                    if property == "length" {
                        return Ok(Some(PropertyDescriptor::data(
                            RawValue::Number(f64::from(array.length)),
                            array.length_writable,
                            false,
                            false,
                        )));
                    }
                    if let Some(index) = canonical_array_index(property) {
                        return Ok(array.elements.get(&index).copied());
                    }
                }
                Ok(record.data.properties.get(property).copied())
            }
            ObjectRef::Function(id) => {
                let Some(record) = state.heap.function(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "function handle is invalid",
                        span,
                    ));
                };
                Ok(record.data.properties.get(property).copied())
            }
        }
    }

    fn prototype_of(
        &self,
        object: ObjectRef,
        span: Option<SourceSpan>,
    ) -> EvalResult<Option<ObjectRef>> {
        self.realm
            .state
            .borrow()
            .heap
            .object_data(object)
            .map(|data| data.prototype)
            .ok_or_else(|| {
                self.runtime_error(ErrorKind::InternalError, "object handle is invalid", span)
            })
    }

    fn define_own_property(
        &mut self,
        object: ObjectRef,
        property: &str,
        update: DescriptorUpdate,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        if let ObjectRef::Object(id) = object {
            let is_array = {
                let state = self.realm.state.borrow();
                let Some(record) = state.heap.object(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "object handle is invalid",
                        span,
                    ));
                };
                matches!(record.kind, ObjectKind::Array(_))
            };
            if is_array && property == "length" {
                return self.define_array_length(id, update, span);
            }
            if is_array && let Some(index) = canonical_array_index(property) {
                return self.define_array_index(id, index, update, span);
            }
        }

        let (current, extensible) = {
            let state = self.realm.state.borrow();
            let Some(data) = state.heap.object_data(object) else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "object handle is invalid",
                    span,
                ));
            };
            (data.properties.get(property).copied(), data.extensible)
        };
        let Some(next) = self.validate_descriptor_update(current, extensible, update) else {
            return Ok(false);
        };
        let mut state = self.realm.state.borrow_mut();
        let Some(data) = state.heap.object_data_mut(object) else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "object handle is invalid",
                span,
            ));
        };
        data.properties.insert(property.to_owned(), next);
        Ok(true)
    }

    fn define_array_index(
        &mut self,
        id: crate::heap::ObjectId,
        index: u32,
        update: DescriptorUpdate,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        let (current, extensible, old_length, length_writable) = {
            let state = self.realm.state.borrow();
            let Some(record) = state.heap.object(id) else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "array handle is invalid",
                    span,
                ));
            };
            let ObjectKind::Array(array) = &record.kind else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "array index definition targeted an ordinary object",
                    span,
                ));
            };
            (
                array.elements.get(&index).copied(),
                record.data.extensible,
                array.length,
                array.length_writable,
            )
        };
        if index >= old_length && !length_writable {
            return Ok(false);
        }
        let Some(next) = self.validate_descriptor_update(current, extensible, update) else {
            return Ok(false);
        };
        let mut state = self.realm.state.borrow_mut();
        let Some(record) = state.heap.object_mut(id) else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "array handle is invalid",
                span,
            ));
        };
        let ObjectKind::Array(array) = &mut record.kind else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "array index definition targeted an ordinary object",
                span,
            ));
        };
        array.elements.insert(index, next);
        if index >= old_length {
            array.length = index + 1;
        }
        Ok(true)
    }

    fn define_array_length(
        &mut self,
        id: crate::heap::ObjectId,
        mut update: DescriptorUpdate,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        if let DescriptorUpdateKind::Data {
            value: Some(value),
            writable,
        } = update.kind
        {
            let length = self.array_length_value(value, span)?;
            update.kind = DescriptorUpdateKind::Data {
                value: Some(RawValue::Number(f64::from(length))),
                writable,
            };
        }
        let (old_length, old_writable) = {
            let state = self.realm.state.borrow();
            let Some(record) = state.heap.object(id) else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "array handle is invalid",
                    span,
                ));
            };
            let ObjectKind::Array(array) = &record.kind else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "array length definition targeted an ordinary object",
                    span,
                ));
            };
            (array.length, array.length_writable)
        };
        let current = PropertyDescriptor::data(
            RawValue::Number(f64::from(old_length)),
            old_writable,
            false,
            false,
        );
        let Some(next) = self.validate_descriptor_update(Some(current), true, update) else {
            return Ok(false);
        };
        let PropertyKind::Data {
            value: RawValue::Number(new_length),
            writable: new_writable,
        } = next.kind
        else {
            return Ok(false);
        };
        let new_length = f64_to_u32_exact(new_length).expect("validated array length is a uint32");

        let mut state = self.realm.state.borrow_mut();
        let Some(record) = state.heap.object_mut(id) else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "array handle is invalid",
                span,
            ));
        };
        let ObjectKind::Array(array) = &mut record.kind else {
            return Err(self.runtime_error(
                ErrorKind::InternalError,
                "array length definition targeted an ordinary object",
                span,
            ));
        };
        if new_length >= old_length {
            array.length = new_length;
            array.length_writable = new_writable;
            return Ok(true);
        }

        array.length = new_length;
        for index in array
            .elements
            .range(new_length..)
            .map(|(index, _)| *index)
            .rev()
            .collect::<Vec<_>>()
        {
            if array
                .elements
                .get(&index)
                .is_some_and(|descriptor| !descriptor.configurable)
            {
                array.length = index + 1;
                array.length_writable = new_writable;
                return Ok(false);
            }
            array.elements.remove(&index);
        }
        array.length_writable = new_writable;
        Ok(true)
    }

    fn array_length_value(&self, value: RawValue, span: Option<SourceSpan>) -> EvalResult<u32> {
        let number = self.to_number(value, span)?;
        f64_to_u32_exact(number)
            .ok_or_else(|| self.runtime_error(ErrorKind::RangeError, "invalid array length", span))
    }

    fn validate_descriptor_update(
        &self,
        current: Option<PropertyDescriptor>,
        extensible: bool,
        update: DescriptorUpdate,
    ) -> Option<PropertyDescriptor> {
        let Some(current) = current else {
            if !extensible {
                return None;
            }
            let mut descriptor = match update.kind {
                DescriptorUpdateKind::Accessor { .. } => {
                    PropertyDescriptor::accessor(None, None, false, false)
                }
                DescriptorUpdateKind::Generic | DescriptorUpdateKind::Data { .. } => {
                    PropertyDescriptor::data(RawValue::Undefined, false, false, false)
                }
            };
            Self::apply_descriptor_fields(&mut descriptor, update);
            return Some(descriptor);
        };

        if !current.configurable {
            if update.configurable == Some(true)
                || update
                    .enumerable
                    .is_some_and(|enumerable| enumerable != current.enumerable)
            {
                return None;
            }
            match (current.kind, update.kind) {
                (_, DescriptorUpdateKind::Generic)
                | (PropertyKind::Data { writable: true, .. }, DescriptorUpdateKind::Data { .. }) => {
                }
                (PropertyKind::Data { .. }, DescriptorUpdateKind::Accessor { .. })
                | (PropertyKind::Accessor { .. }, DescriptorUpdateKind::Data { .. }) => {
                    return None;
                }
                (
                    PropertyKind::Data {
                        value: old_value,
                        writable: false,
                    },
                    DescriptorUpdateKind::Data { value, writable },
                ) => {
                    if writable == Some(true)
                        || value.is_some_and(|value| !self.same_value(old_value, value))
                    {
                        return None;
                    }
                }
                (
                    PropertyKind::Accessor {
                        getter: read_function,
                        setter: write_function,
                    },
                    DescriptorUpdateKind::Accessor { getter, setter },
                ) => {
                    if matches!(
                        getter,
                        AccessorUpdate::Present(requested)
                            if !self.same_optional_value(read_function, requested)
                    ) || matches!(
                        setter,
                        AccessorUpdate::Present(requested)
                            if !self.same_optional_value(write_function, requested)
                    ) {
                        return None;
                    }
                }
            }
        }

        let mut descriptor = current;
        match (descriptor.kind, update.kind) {
            (PropertyKind::Data { .. }, DescriptorUpdateKind::Accessor { .. }) => {
                descriptor.kind = PropertyKind::Accessor {
                    getter: None,
                    setter: None,
                };
            }
            (PropertyKind::Accessor { .. }, DescriptorUpdateKind::Data { .. }) => {
                descriptor.kind = PropertyKind::Data {
                    value: RawValue::Undefined,
                    writable: false,
                };
            }
            _ => {}
        }
        Self::apply_descriptor_fields(&mut descriptor, update);
        Some(descriptor)
    }

    fn apply_descriptor_fields(descriptor: &mut PropertyDescriptor, update: DescriptorUpdate) {
        if let Some(enumerable) = update.enumerable {
            descriptor.enumerable = enumerable;
        }
        if let Some(configurable) = update.configurable {
            descriptor.configurable = configurable;
        }
        match (&mut descriptor.kind, update.kind) {
            (_, DescriptorUpdateKind::Generic) => {}
            (
                PropertyKind::Data { value, writable },
                DescriptorUpdateKind::Data {
                    value: next_value,
                    writable: next_writable,
                },
            ) => {
                if let Some(next_value) = next_value {
                    *value = next_value;
                }
                if let Some(next_writable) = next_writable {
                    *writable = next_writable;
                }
            }
            (
                PropertyKind::Accessor { getter, setter },
                DescriptorUpdateKind::Accessor {
                    getter: read_update,
                    setter: write_update,
                },
            ) => {
                if let AccessorUpdate::Present(value) = read_update {
                    *getter = value;
                }
                if let AccessorUpdate::Present(value) = write_update {
                    *setter = value;
                }
            }
            _ => unreachable!("descriptor kind is normalized before fields are applied"),
        }
    }

    fn same_optional_value(&self, left: Option<RawValue>, right: Option<RawValue>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(left), Some(right)) => self.same_value(left, right),
            (None, Some(_)) | (Some(_), None) => false,
        }
    }

    fn same_value(&self, left: RawValue, right: RawValue) -> bool {
        match (left, right) {
            (RawValue::Number(left), RawValue::Number(right)) => {
                left.to_bits() == right.to_bits() || left.is_nan() && right.is_nan()
            }
            _ => self.strict_equal(left, right),
        }
    }

    fn set_prototype_raw(
        &self,
        target: ObjectRef,
        prototype: Option<ObjectRef>,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        let (current, extensible) = {
            let state = self.realm.state.borrow();
            let Some(data) = state.heap.object_data(target) else {
                return Err(self.runtime_error(
                    ErrorKind::InternalError,
                    "object handle is invalid",
                    span,
                ));
            };
            (data.prototype, data.extensible)
        };
        if current == prototype {
            return Ok(true);
        }
        if !extensible {
            return Ok(false);
        }
        let mut candidate = prototype;
        let mut visited = HashSet::new();
        for _ in 0..MAX_PROTOTYPE_CHAIN {
            let Some(object) = candidate else {
                let mut state = self.realm.state.borrow_mut();
                let Some(data) = state.heap.object_data_mut(target) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "object handle is invalid",
                        span,
                    ));
                };
                data.prototype = prototype;
                return Ok(true);
            };
            if object == target || !visited.insert(object) {
                return Ok(false);
            }
            candidate = self.prototype_of(object, span)?;
        }
        Ok(false)
    }

    fn construct_raw(
        &mut self,
        constructor: RawValue,
        arguments: &[RawValue],
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        let RawValue::Function(function) = constructor else {
            return Err(self.runtime_error(
                ErrorKind::TypeError,
                format!("{} is not a constructor", constructor.type_name()),
                span,
            ));
        };
        let constructible = self
            .realm
            .state
            .borrow()
            .heap
            .function(function)
            .map(|record| record.constructible)
            .ok_or_else(|| {
                self.runtime_error(ErrorKind::InternalError, "function handle is invalid", span)
            })?;
        if !constructible {
            return Err(self.runtime_error(
                ErrorKind::TypeError,
                "function is not a constructor",
                span,
            ));
        }
        let candidate = self.get_property_raw(constructor, "prototype", span)?;
        let prototype = candidate
            .as_object_ref()
            .unwrap_or_else(|| self.realm.state.borrow().intrinsics.object);
        let this_value = self
            .realm
            .state
            .borrow_mut()
            .heap
            .allocate_object(Some(prototype), HashMap::new());
        let result = self.call_raw(constructor, this_value, arguments, span)?;
        Ok(if result.as_object_ref().is_some() {
            result
        } else {
            this_value
        })
    }

    fn prototype_chain_error(&self, span: Option<SourceSpan>) -> Thrown {
        self.runtime_error(
            ErrorKind::InternalError,
            "prototype chain is cyclic or exceeds the implementation bound",
            span,
        )
    }

    fn to_property_key(&self, value: RawValue, span: Option<SourceSpan>) -> EvalResult<String> {
        match value {
            RawValue::Object(_) | RawValue::Function(_) => Err(self.runtime_error(
                ErrorKind::TypeError,
                "object-to-primitive property key conversion is not implemented",
                span,
            )),
            _ => self.to_string_primitive(value, span),
        }
    }

    fn call_raw(
        &mut self,
        function: RawValue,
        this_value: RawValue,
        arguments: &[RawValue],
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        self.tick(span)?;
        let RawValue::Function(function_id) = function else {
            return Err(self.runtime_error(
                ErrorKind::TypeError,
                format!("{} is not callable", function.type_name()),
                span,
            ));
        };
        if self.frames.len() >= self.limits.max_call_depth {
            return Err(self.runtime_error(
                ErrorKind::RangeError,
                "maximum call depth exceeded",
                span,
            ));
        }
        let record: FunctionRecord = {
            let state = self.realm.state.borrow();
            state.heap.function(function_id).cloned().ok_or_else(|| {
                self.runtime_error(ErrorKind::InternalError, "function handle is invalid", span)
            })?
        };
        let call_site = span.map(|span| self.location(span));
        self.frames.push(CallFrame {
            stack_frame: StackFrame {
                function_name: record.display_name().to_owned(),
                call_site,
            },
            this_value,
        });

        let result = match record.callable {
            Callable::Host(host) => {
                let rooted_this = self.root(this_value);
                let rooted_arguments: Vec<_> = arguments
                    .iter()
                    .copied()
                    .map(|argument| self.root(argument))
                    .collect();
                match host.callback.call(self, &rooted_this, &rooted_arguments) {
                    Ok(value) => self
                        .raw(&value)
                        .map_err(|error| self.error_to_thrown(&error, span)),
                    Err(error) => Err(self.error_to_thrown(&error, span)),
                }
            }
            Callable::Script(script) => {
                self.source_stack.push(Arc::clone(&script.source_name));
                let call_environment = self.allocate_environment(Some(script.closure));
                let setup = script.function.parameters.iter().enumerate().try_for_each(
                    |(index, parameter)| {
                        self.create_initialized_binding(
                            call_environment,
                            parameter,
                            true,
                            arguments.get(index).copied().unwrap_or(RawValue::Undefined),
                            Some(script.function.span),
                        )
                    },
                );
                let result = setup.and_then(|()| {
                    self.execute_statement_list(&script.function.body, call_environment)
                        .and_then(|completion| match completion {
                            Completion::Return(value) => Ok(value),
                            Completion::Normal(_) => Ok(RawValue::Undefined),
                            Completion::Break | Completion::Continue => Err(self.runtime_error(
                                ErrorKind::InternalError,
                                "loop control escaped a function body",
                                Some(script.function.span),
                            )),
                        })
                });
                self.source_stack.pop();
                result
            }
        };
        self.frames.pop();
        result
    }
}

impl Context {
    fn object_constructor_builtin(&mut self, arguments: &[RootedValue]) -> JsResult<RootedValue> {
        let value = self.raw_argument(arguments, 0)?;
        match value {
            RawValue::Object(_) | RawValue::Function(_) => Ok(self.root(value)),
            RawValue::Undefined | RawValue::Null => Ok(self.object()),
            RawValue::Boolean(_) | RawValue::Number(_) | RawValue::String(_) => Err(JsError::new(
                ErrorKind::TypeError,
                "primitive wrapper objects are outside the current Object constructor subset",
            )),
        }
    }

    fn array_from_constructor_arguments(
        &mut self,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let values = arguments
            .iter()
            .map(|argument| self.raw(argument))
            .collect::<JsResult<Vec<_>>>()?;
        let (length, elements) = if let [RawValue::Number(length)] = values.as_slice() {
            let length = match self.array_length_value(RawValue::Number(*length), None) {
                Ok(length) => length,
                Err(thrown) => return Err(self.thrown_to_error(thrown)),
            };
            (length, BTreeMap::new())
        } else {
            let length = u32::try_from(values.len()).map_err(|_| {
                JsError::new(
                    ErrorKind::RangeError,
                    "Array constructor argument count exceeds the array length range",
                )
            })?;
            let elements = values
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    let index = u32::try_from(index)
                        .expect("argument count was validated as an array length");
                    (index, PropertyDescriptor::default_data(value))
                })
                .collect();
            (length, elements)
        };
        let mut state = self.realm.state.borrow_mut();
        let prototype = state.intrinsics.array;
        let array = state.heap.allocate_array(Some(prototype), length, elements);
        drop(state);
        Ok(self.root(array))
    }

    fn object_create_builtin(&mut self, arguments: &[RootedValue]) -> JsResult<RootedValue> {
        let prototype = match self.raw_argument(arguments, 0)? {
            RawValue::Null => None,
            value => value.as_object_ref().map(Some).ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "Object.create prototype must be an object or null",
                )
            })?,
        };
        if !matches!(self.raw_argument(arguments, 1)?, RawValue::Undefined) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Object.create property bags are outside the current descriptor subset",
            ));
        }
        let object = self
            .realm
            .state
            .borrow_mut()
            .heap
            .allocate_object(prototype, HashMap::new());
        Ok(self.root(object))
    }

    fn object_get_prototype_of_builtin(
        &mut self,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target = self.require_object_argument(arguments, 0, "Object.getPrototypeOf")?;
        match self.prototype_of(target, None) {
            Ok(Some(prototype)) => Ok(self.root(prototype.as_value())),
            Ok(None) => Ok(self.null()),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn object_set_prototype_of_builtin(
        &mut self,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target_value = self.raw_argument(arguments, 0)?;
        let target = target_value.as_object_ref().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Object.setPrototypeOf target must be an object",
            )
        })?;
        let prototype = match self.raw_argument(arguments, 1)? {
            RawValue::Null => None,
            value => value.as_object_ref().map(Some).ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "Object.setPrototypeOf prototype must be an object or null",
                )
            })?,
        };
        match self.set_prototype_raw(target, prototype, None) {
            Ok(true) => Ok(self.root(target_value)),
            Ok(false) => Err(JsError::new(
                ErrorKind::TypeError,
                "Object.setPrototypeOf rejected the prototype mutation",
            )),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn object_define_property_builtin(
        &mut self,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target_value = self.raw_argument(arguments, 0)?;
        let target = target_value.as_object_ref().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Object.defineProperty target must be an object",
            )
        })?;
        let key_value = self.raw_argument(arguments, 1)?;
        let key = match self.to_property_key(key_value, None) {
            Ok(key) => key,
            Err(thrown) => return Err(self.thrown_to_error(thrown)),
        };
        let descriptor_value = self.raw_argument(arguments, 2)?;
        let descriptor = self.property_descriptor_from_object(descriptor_value)?;
        match self.define_own_property(target, &key, descriptor, None) {
            Ok(true) => Ok(self.root(target_value)),
            Ok(false) => Err(JsError::new(
                ErrorKind::TypeError,
                format!("cannot define property '{key}' with the requested descriptor"),
            )),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn object_get_own_property_descriptor_builtin(
        &mut self,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target =
            self.require_object_argument(arguments, 0, "Object.getOwnPropertyDescriptor")?;
        let key_value = self.raw_argument(arguments, 1)?;
        let key = match self.to_property_key(key_value, None) {
            Ok(key) => key,
            Err(thrown) => return Err(self.thrown_to_error(thrown)),
        };
        let descriptor = match self.own_property_descriptor(target, &key, None) {
            Ok(descriptor) => descriptor,
            Err(thrown) => return Err(self.thrown_to_error(thrown)),
        };
        let Some(descriptor) = descriptor else {
            return Ok(self.undefined());
        };
        let mut properties = HashMap::new();
        match descriptor.kind {
            PropertyKind::Data { value, writable } => {
                properties.insert("value".to_owned(), PropertyDescriptor::default_data(value));
                properties.insert(
                    "writable".to_owned(),
                    PropertyDescriptor::default_data(RawValue::Boolean(writable)),
                );
            }
            PropertyKind::Accessor { getter, setter } => {
                properties.insert(
                    "get".to_owned(),
                    PropertyDescriptor::default_data(getter.unwrap_or(RawValue::Undefined)),
                );
                properties.insert(
                    "set".to_owned(),
                    PropertyDescriptor::default_data(setter.unwrap_or(RawValue::Undefined)),
                );
            }
        }
        properties.insert(
            "enumerable".to_owned(),
            PropertyDescriptor::default_data(RawValue::Boolean(descriptor.enumerable)),
        );
        properties.insert(
            "configurable".to_owned(),
            PropertyDescriptor::default_data(RawValue::Boolean(descriptor.configurable)),
        );
        let mut state = self.realm.state.borrow_mut();
        let prototype = state.intrinsics.object;
        let result = state.heap.allocate_object(Some(prototype), properties);
        drop(state);
        Ok(self.root(result))
    }

    fn object_has_own_builtin(&mut self, arguments: &[RootedValue]) -> JsResult<RootedValue> {
        let target = self.require_object_argument(arguments, 0, "Object.hasOwn")?;
        let key_value = self.raw_argument(arguments, 1)?;
        let key = match self.to_property_key(key_value, None) {
            Ok(key) => key,
            Err(thrown) => return Err(self.thrown_to_error(thrown)),
        };
        match self.own_property_descriptor(target, &key, None) {
            Ok(descriptor) => Ok(self.boolean(descriptor.is_some())),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn object_prevent_extensions_builtin(
        &mut self,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target_value = self.raw_argument(arguments, 0)?;
        let target = target_value.as_object_ref().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Object.preventExtensions target must be an object",
            )
        })?;
        let mut state = self.realm.state.borrow_mut();
        let Some(data) = state.heap.object_data_mut(target) else {
            return Err(invalid_heap_handle());
        };
        data.extensible = false;
        drop(state);
        Ok(self.root(target_value))
    }

    fn object_is_extensible_builtin(&mut self, arguments: &[RootedValue]) -> JsResult<RootedValue> {
        let target = self.require_object_argument(arguments, 0, "Object.isExtensible")?;
        let state = self.realm.state.borrow();
        let extensible = state
            .heap
            .object_data(target)
            .map(|data| data.extensible)
            .ok_or_else(invalid_heap_handle)?;
        drop(state);
        Ok(self.boolean(extensible))
    }

    fn array_is_array_builtin(&mut self, arguments: &[RootedValue]) -> JsResult<RootedValue> {
        let value = self.raw_argument(arguments, 0)?;
        let is_array = match value {
            RawValue::Object(id) => self
                .realm
                .state
                .borrow()
                .heap
                .object(id)
                .is_some_and(|record| matches!(record.kind, ObjectKind::Array(_))),
            _ => false,
        };
        Ok(self.boolean(is_array))
    }

    fn array_push_builtin(
        &mut self,
        this: &RootedValue,
        arguments: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target = self.raw(this)?;
        let old_length = self.require_array_length(target, "Array.prototype.push")?;
        let argument_count = u32::try_from(arguments.len()).map_err(|_| {
            JsError::new(
                ErrorKind::RangeError,
                "push argument count exceeds the array length range",
            )
        })?;
        let new_length = old_length.checked_add(argument_count).ok_or_else(|| {
            JsError::new(ErrorKind::RangeError, "push exceeds the array length range")
        })?;
        for (offset, argument) in arguments.iter().enumerate() {
            let offset = u32::try_from(offset).expect("push argument count was validated");
            let index = old_length + offset;
            let value = self.raw(argument)?;
            match self.set_property_raw(target, &index.to_string(), value, None) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        format!("array rejected assignment to index {index}"),
                    ));
                }
                Err(thrown) => return Err(self.thrown_to_error(thrown)),
            }
        }
        match self.set_property_raw(
            target,
            "length",
            RawValue::Number(f64::from(new_length)),
            None,
        ) {
            Ok(true) => Ok(self.number(f64::from(new_length))),
            Ok(false) => Err(JsError::new(
                ErrorKind::TypeError,
                "array length is not writable",
            )),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn array_pop_builtin(
        &mut self,
        this: &RootedValue,
        _: &[RootedValue],
    ) -> JsResult<RootedValue> {
        let target = self.raw(this)?;
        let old_length = self.require_array_length(target, "Array.prototype.pop")?;
        let new_length = old_length.saturating_sub(1);
        let result = if old_length == 0 {
            RawValue::Undefined
        } else {
            let key = new_length.to_string();
            let result = match self.get_property_raw(target, &key, None) {
                Ok(value) => value,
                Err(thrown) => return Err(self.thrown_to_error(thrown)),
            };
            match self.delete_property_raw(target, &key, None) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        format!("array rejected deletion of index {key}"),
                    ));
                }
                Err(thrown) => return Err(self.thrown_to_error(thrown)),
            }
            result
        };
        match self.set_property_raw(
            target,
            "length",
            RawValue::Number(f64::from(new_length)),
            None,
        ) {
            Ok(true) => Ok(self.root(result)),
            Ok(false) => Err(JsError::new(
                ErrorKind::TypeError,
                "array length is not writable",
            )),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn property_descriptor_from_object(&mut self, value: RawValue) -> JsResult<DescriptorUpdate> {
        let object = value.as_object_ref().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "property descriptor must be an object",
            )
        })?;
        let enumerable = self.descriptor_field(object, value, "enumerable")?;
        let configurable = self.descriptor_field(object, value, "configurable")?;
        let descriptor_value = self.descriptor_field(object, value, "value")?;
        let writable = self.descriptor_field(object, value, "writable")?;
        let getter = self.descriptor_field(object, value, "get")?;
        let setter = self.descriptor_field(object, value, "set")?;
        let is_data = descriptor_value.is_some() || writable.is_some();
        let is_accessor = getter.is_some() || setter.is_some();
        if is_data && is_accessor {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "property descriptor cannot mix data and accessor fields",
            ));
        }
        let kind = if is_accessor {
            DescriptorUpdateKind::Accessor {
                getter: match getter {
                    Some(value) => {
                        AccessorUpdate::Present(Self::descriptor_callable(value, "getter")?)
                    }
                    None => AccessorUpdate::Absent,
                },
                setter: match setter {
                    Some(value) => {
                        AccessorUpdate::Present(Self::descriptor_callable(value, "setter")?)
                    }
                    None => AccessorUpdate::Absent,
                },
            }
        } else if is_data {
            DescriptorUpdateKind::Data {
                value: descriptor_value,
                writable: writable.map(|value| self.to_boolean(value)),
            }
        } else {
            DescriptorUpdateKind::Generic
        };
        Ok(DescriptorUpdate {
            kind,
            enumerable: enumerable.map(|value| self.to_boolean(value)),
            configurable: configurable.map(|value| self.to_boolean(value)),
        })
    }

    fn descriptor_field(
        &mut self,
        object: ObjectRef,
        receiver: RawValue,
        property: &str,
    ) -> JsResult<Option<RawValue>> {
        let present = match self.has_property_raw(object, property, None) {
            Ok(present) => present,
            Err(thrown) => return Err(self.thrown_to_error(thrown)),
        };
        if !present {
            return Ok(None);
        }
        match self.get_property_raw(receiver, property, None) {
            Ok(value) => Ok(Some(value)),
            Err(thrown) => Err(self.thrown_to_error(thrown)),
        }
    }

    fn descriptor_callable(value: RawValue, field: &str) -> JsResult<Option<RawValue>> {
        match value {
            RawValue::Undefined => Ok(None),
            RawValue::Function(_) => Ok(Some(value)),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                format!("descriptor {field} must be callable or undefined"),
            )),
        }
    }

    fn raw_argument(&self, arguments: &[RootedValue], index: usize) -> JsResult<RawValue> {
        arguments
            .get(index)
            .map_or(Ok(RawValue::Undefined), |value| self.raw(value))
    }

    fn require_object_argument(
        &self,
        arguments: &[RootedValue],
        index: usize,
        operation: &str,
    ) -> JsResult<ObjectRef> {
        self.raw_argument(arguments, index)?
            .as_object_ref()
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    format!("{operation} target must be an object"),
                )
            })
    }

    fn require_array_length(&self, value: RawValue, operation: &str) -> JsResult<u32> {
        let RawValue::Object(id) = value else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("{operation} currently requires an Array receiver"),
            ));
        };
        let state = self.realm.state.borrow();
        let Some(record) = state.heap.object(id) else {
            return Err(invalid_heap_handle());
        };
        let ObjectKind::Array(array) = &record.kind else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("{operation} currently requires an Array receiver"),
            ));
        };
        Ok(array.length)
    }
}

impl Context {
    fn tick(&mut self, span: Option<SourceSpan>) -> EvalResult<()> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.limits.max_steps {
            Err(self.runtime_error(
                ErrorKind::ResourceLimit,
                format!("execution exceeded {} steps", self.limits.max_steps),
                span,
            ))
        } else {
            Ok(())
        }
    }

    fn location(&self, span: SourceSpan) -> DiagnosticLocation {
        DiagnosticLocation {
            source_name: self
                .source_stack
                .last()
                .map_or_else(|| "<host>".to_owned(), ToString::to_string),
            span,
        }
    }

    fn fallback_location(&self) -> DiagnosticLocation {
        let start = crate::source::SourceLocation::start();
        self.location(SourceSpan::new(start, start))
    }

    fn captured_stack(&self) -> Vec<StackFrame> {
        self.frames
            .iter()
            .rev()
            .map(|frame| frame.stack_frame.clone())
            .collect()
    }

    fn runtime_error(
        &self,
        kind: ErrorKind,
        message: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Thrown {
        Thrown {
            payload: ThrownPayload::Runtime {
                kind,
                message: message.into(),
            },
            location: span.map_or_else(|| self.fallback_location(), |span| self.location(span)),
            stack: self.captured_stack(),
        }
    }

    fn thrown_value(&self, value: RawValue, span: Option<SourceSpan>) -> Thrown {
        Thrown {
            payload: ThrownPayload::Value(value),
            location: span.map_or_else(|| self.fallback_location(), |span| self.location(span)),
            stack: self.captured_stack(),
        }
    }

    fn catch_value(&self, thrown: &Thrown) -> RawValue {
        match &thrown.payload {
            ThrownPayload::Value(value) => *value,
            ThrownPayload::Runtime { kind, message } => {
                let mut state = self.realm.state.borrow_mut();
                let name = state.heap.allocate_string(Arc::<str>::from(kind.name()));
                let message = state
                    .heap
                    .allocate_string(Arc::<str>::from(message.as_str()));
                let prototype = state.intrinsics.object;
                state.heap.allocate_object(
                    Some(prototype),
                    HashMap::from([
                        ("name".to_owned(), PropertyDescriptor::default_data(name)),
                        (
                            "message".to_owned(),
                            PropertyDescriptor::default_data(message),
                        ),
                    ]),
                )
            }
        }
    }

    fn thrown_to_error(&self, thrown: Thrown) -> JsError {
        match thrown.payload {
            ThrownPayload::Runtime { kind, message } => {
                JsError::located(kind, message, thrown.location, thrown.stack)
            }
            ThrownPayload::Value(value) => {
                let message = self
                    .display_value(value)
                    .unwrap_or_else(|_| "<unprintable thrown value>".to_owned());
                JsError::thrown(
                    format!("uncaught {message}"),
                    thrown.location,
                    thrown.stack,
                    self.root(value),
                )
            }
        }
    }

    fn error_to_thrown(&self, error: &JsError, span: Option<SourceSpan>) -> Thrown {
        if let Some(value) = error.exception_value()
            && let Ok(raw) = self.raw(value)
        {
            return self.thrown_value(raw, span);
        }
        let mut thrown = self.runtime_error(error.kind(), error.message().to_owned(), span);
        if let Some(location) = error.location() {
            thrown.location = location.clone();
        }
        if !error.stack().is_empty() {
            thrown.stack = error.stack().to_vec();
        }
        thrown
    }
}

impl Context {
    fn strict_equal(&self, left: RawValue, right: RawValue) -> bool {
        match (left, right) {
            (RawValue::Undefined, RawValue::Undefined) | (RawValue::Null, RawValue::Null) => true,
            (RawValue::Boolean(left), RawValue::Boolean(right)) => left == right,
            (RawValue::Number(left), RawValue::Number(right)) => js_number_equal(left, right),
            (RawValue::String(left), RawValue::String(right)) => {
                let state = self.realm.state.borrow();
                state.heap.string(left) == state.heap.string(right)
            }
            (RawValue::Object(left), RawValue::Object(right)) => left == right,
            (RawValue::Function(left), RawValue::Function(right)) => left == right,
            _ => false,
        }
    }

    fn abstract_equal(
        &self,
        left: RawValue,
        right: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<bool> {
        if same_value_category(left, right) {
            return Ok(self.strict_equal(left, right));
        }
        match (left, right) {
            (RawValue::Null, RawValue::Undefined) | (RawValue::Undefined, RawValue::Null) => {
                Ok(true)
            }
            (RawValue::Object(_) | RawValue::Function(_), RawValue::Null | RawValue::Undefined)
            | (RawValue::Null | RawValue::Undefined, RawValue::Object(_) | RawValue::Function(_)) => {
                Ok(false)
            }
            (RawValue::Number(number), RawValue::String(string)) => {
                let string = self.string_copy(string, span)?;
                Ok(js_number_equal(number, parse_numeric_string(&string)))
            }
            (RawValue::String(string), RawValue::Number(number)) => {
                let string = self.string_copy(string, span)?;
                Ok(js_number_equal(parse_numeric_string(&string), number))
            }
            (RawValue::Boolean(boolean), other) => {
                self.abstract_equal(RawValue::Number(f64::from(boolean)), other, span)
            }
            (other, RawValue::Boolean(boolean)) => {
                self.abstract_equal(other, RawValue::Number(f64::from(boolean)), span)
            }
            (RawValue::Object(_) | RawValue::Function(_), _)
            | (_, RawValue::Object(_) | RawValue::Function(_)) => Err(self.runtime_error(
                ErrorKind::TypeError,
                "object-to-primitive equality conversion is not implemented",
                span,
            )),
            _ => Ok(false),
        }
    }

    fn add(
        &self,
        left: RawValue,
        right: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        if matches!(left, RawValue::Object(_) | RawValue::Function(_))
            || matches!(right, RawValue::Object(_) | RawValue::Function(_))
        {
            return Err(self.runtime_error(
                ErrorKind::TypeError,
                "object-to-primitive addition is not implemented",
                span,
            ));
        }
        if matches!(left, RawValue::String(_)) || matches!(right, RawValue::String(_)) {
            let left = self.to_string_primitive(left, span)?;
            let right = self.to_string_primitive(right, span)?;
            let value = format!("{left}{right}");
            return Ok(self
                .realm
                .state
                .borrow_mut()
                .heap
                .allocate_string(Arc::<str>::from(value)));
        }
        Ok(RawValue::Number(
            self.to_number(left, span)? + self.to_number(right, span)?,
        ))
    }

    fn relational(
        &self,
        operator: BinaryOperator,
        left: RawValue,
        right: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<RawValue> {
        let result = if let (RawValue::String(left), RawValue::String(right)) = (left, right) {
            let left = self.string_copy(left, span)?;
            let right = self.string_copy(right, span)?;
            match operator {
                BinaryOperator::LessThan => left < right,
                BinaryOperator::LessThanOrEqual => left <= right,
                BinaryOperator::GreaterThan => left > right,
                BinaryOperator::GreaterThanOrEqual => left >= right,
                _ => {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "non-relational operator reached relational evaluation",
                        span,
                    ));
                }
            }
        } else {
            let left = self.to_number(left, span)?;
            let right = self.to_number(right, span)?;
            if left.is_nan() || right.is_nan() {
                false
            } else {
                match operator {
                    BinaryOperator::LessThan => left < right,
                    BinaryOperator::LessThanOrEqual => left <= right,
                    BinaryOperator::GreaterThan => left > right,
                    BinaryOperator::GreaterThanOrEqual => left >= right,
                    _ => {
                        return Err(self.runtime_error(
                            ErrorKind::InternalError,
                            "non-relational operator reached relational evaluation",
                            span,
                        ));
                    }
                }
            }
        };
        Ok(RawValue::Boolean(result))
    }

    fn to_number(&self, value: RawValue, span: Option<SourceSpan>) -> EvalResult<f64> {
        match value {
            RawValue::Undefined => Ok(f64::NAN),
            RawValue::Null => Ok(0.0),
            RawValue::Boolean(value) => Ok(f64::from(value)),
            RawValue::Number(value) => Ok(value),
            RawValue::String(id) => self
                .string_copy(id, span)
                .map(|string| parse_numeric_string(&string)),
            RawValue::Object(_) | RawValue::Function(_) => Err(self.runtime_error(
                ErrorKind::TypeError,
                "object-to-number conversion is not implemented",
                span,
            )),
        }
    }

    fn to_boolean(&self, value: RawValue) -> bool {
        match value {
            RawValue::Undefined | RawValue::Null => false,
            RawValue::Boolean(value) => value,
            RawValue::Number(value) => value != 0.0 && !value.is_nan(),
            RawValue::String(id) => self
                .realm
                .state
                .borrow()
                .heap
                .string(id)
                .is_some_and(|string| !string.is_empty()),
            RawValue::Object(_) | RawValue::Function(_) => true,
        }
    }

    fn to_string_primitive(&self, value: RawValue, span: Option<SourceSpan>) -> EvalResult<String> {
        match value {
            RawValue::Undefined => Ok("undefined".to_owned()),
            RawValue::Null => Ok("null".to_owned()),
            RawValue::Boolean(value) => Ok(value.to_string()),
            RawValue::Number(value) => Ok(number_to_string(value)),
            RawValue::String(id) => self.string_copy(id, span),
            RawValue::Object(_) | RawValue::Function(_) => Err(self.runtime_error(
                ErrorKind::TypeError,
                "object-to-string conversion is not implemented",
                span,
            )),
        }
    }

    fn string_copy(
        &self,
        id: crate::heap::StringId,
        span: Option<SourceSpan>,
    ) -> EvalResult<String> {
        self.realm
            .state
            .borrow()
            .heap
            .string(id)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                self.runtime_error(ErrorKind::InternalError, "string handle is invalid", span)
            })
    }

    fn display_value(&self, value: RawValue) -> EvalResult<String> {
        match value {
            RawValue::Object(_) => Ok("[object Object]".to_owned()),
            RawValue::Function(id) => {
                let state = self.realm.state.borrow();
                let Some(function) = state.heap.function(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "function handle is invalid",
                        None,
                    ));
                };
                Ok(format!("[function {}]", function.display_name()))
            }
            _ => self.to_string_primitive(value, None),
        }
    }
}

fn same_value_category(left: RawValue, right: RawValue) -> bool {
    matches!(
        (left, right),
        (RawValue::Undefined, RawValue::Undefined)
            | (RawValue::Null, RawValue::Null)
            | (RawValue::Boolean(_), RawValue::Boolean(_))
            | (RawValue::Number(_), RawValue::Number(_))
            | (RawValue::String(_), RawValue::String(_))
            | (RawValue::Object(_), RawValue::Object(_))
            | (RawValue::Function(_), RawValue::Function(_))
    )
}

fn js_number_equal(left: f64, right: f64) -> bool {
    matches!(left.partial_cmp(&right), Some(std::cmp::Ordering::Equal))
}

fn canonical_array_index(property: &str) -> Option<u32> {
    if property.is_empty()
        || property.len() > 1 && property.starts_with('0')
        || !property.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let index = property.parse::<u32>().ok()?;
    (index != u32::MAX).then_some(index)
}

fn f64_to_u32_exact(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 || value > f64::from(u32::MAX) || value.fract() != 0.0 {
        return None;
    }
    if value == 0.0 {
        return Some(0);
    }
    value.to_string().parse().ok()
}

fn parse_numeric_string(value: &str) -> f64 {
    let value = value.trim();
    if value.is_empty() {
        return 0.0;
    }
    match value {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (radix, digits) = if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (16, Some(digits))
    } else if let Some(digits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        (2, Some(digits))
    } else if let Some(digits) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        (8, Some(digits))
    } else {
        (10, None)
    };
    if let Some(digits) = digits {
        if digits.is_empty() {
            return f64::NAN;
        }
        return digits
            .chars()
            .try_fold(0.0_f64, |value, character| {
                character
                    .to_digit(radix)
                    .map(|digit| value.mul_add(f64::from(radix), f64::from(digit)))
            })
            .unwrap_or(f64::NAN);
    }
    value.parse::<f64>().unwrap_or(f64::NAN)
}

fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        }
    } else if value == 0.0 {
        "0".to_owned()
    } else {
        value.to_string()
    }
}

fn usize_to_number(value: usize) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(f64::INFINITY)
}

#[cfg(test)]
mod identity_tests {
    use std::sync::atomic::AtomicU64;

    use super::{MonotonicId, allocate_realm_id};

    #[test]
    fn root_identity_allocator_never_wraps_or_reuses() {
        let mut identities = MonotonicId::new(u64::MAX - 1);
        assert_eq!(identities.allocate(), Some(u64::MAX - 1));
        assert_eq!(identities.allocate(), Some(u64::MAX));
        assert_eq!(identities.allocate(), None);
        assert_eq!(identities.allocate(), None);
    }

    #[test]
    fn realm_identity_allocator_refuses_atomic_wrap() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_realm_id(&counter).map(|id| id.0),
            Some(u64::MAX - 1)
        );
        assert_eq!(allocate_realm_id(&counter), None);
        assert_eq!(allocate_realm_id(&counter), None);
    }
}

#[cfg(test)]
mod collector_tests {
    use super::{CollectionErrorKind, Engine, RealmOptions};

    #[test]
    fn active_entry_in_one_context_blocks_collection_in_another() {
        let engine = Engine::default();
        let realm = engine.create_realm(RealmOptions::default());
        let mut executing = realm.context();
        let mut collector = realm.context();

        executing.begin_entry();
        let error = collector.collect_garbage().unwrap_err();
        assert_eq!(error.kind(), CollectionErrorKind::ActiveExecution);
        executing.end_entry();

        collector.collect_garbage().unwrap();
    }

    #[test]
    fn reentrant_collection_has_a_distinct_structured_failure() {
        let engine = Engine::default();
        let realm = engine.create_realm(RealmOptions::default());
        let mut context = realm.context();

        context.realm.collection_active.set(true);
        let error = context.collect_garbage().unwrap_err();
        assert_eq!(error.kind(), CollectionErrorKind::CollectionInProgress);
        context.realm.collection_active.set(false);

        context.collect_garbage().unwrap();
    }
}

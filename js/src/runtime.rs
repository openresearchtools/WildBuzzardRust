use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
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
    RawValue, ReclaimedCounts, ScriptFunction, TraceError,
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
        let raw = self
            .realm
            .state
            .borrow_mut()
            .heap
            .allocate_object(HashMap::new());
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
        let function = self
            .realm
            .state
            .borrow_mut()
            .heap
            .allocate_function(Callable::Host(HostFunctionRecord {
                name: name.clone(),
                arity,
                callback: Rc::new(callback),
            }));
        let rooted = self.root(function);
        self.define_global(name, &rooted, false)
    }

    /// Reads an ordinary or function object's own property.
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

    /// Writes an ordinary or function object's own property.
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
        self.set_property_raw(target, property.into(), new_value, None)
            .map_err(|thrown| self.thrown_to_error(thrown))
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

        let roots: Vec<_> = self.realm.roots.borrow().values.values().copied().collect();
        let mut state = self.realm.state.borrow_mut();
        let before = heap_statistics(&state.heap, roots.len());
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
        self.realm
            .state
            .borrow_mut()
            .heap
            .allocate_function(Callable::Script(ScriptFunction {
                function,
                closure,
                source_name,
            }))
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
                let mut object_properties = HashMap::new();
                for property in properties {
                    let value = self.evaluate_expression(&property.value, environment)?;
                    object_properties.insert(property.key.clone(), value);
                }
                Ok(self
                    .realm
                    .state
                    .borrow_mut()
                    .heap
                    .allocate_object(object_properties))
            }
            ExpressionKind::Member { object, property } => {
                let object = self.evaluate_expression(object, environment)?;
                let property = self.evaluate_member_property(property, environment)?;
                self.get_property_raw(object, &property, Some(expression.span))
            }
            ExpressionKind::Call { callee, arguments } => {
                self.evaluate_call(callee, arguments, environment, expression.span)
            }
        }
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
                self.set_property_raw(object, property, value, Some(span))?;
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
                let state = self.realm.state.borrow();
                let Some(object) = state.heap.object(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "object handle is invalid",
                        span,
                    ));
                };
                Ok(object
                    .properties
                    .get(property)
                    .copied()
                    .unwrap_or(RawValue::Undefined))
            }
            RawValue::Function(id) => {
                let metadata = {
                    let state = self.realm.state.borrow();
                    let Some(function) = state.heap.function(id) else {
                        return Err(self.runtime_error(
                            ErrorKind::InternalError,
                            "function handle is invalid",
                            span,
                        ));
                    };
                    if let Some(value) = function.properties.get(property).copied() {
                        return Ok(value);
                    }
                    match property {
                        "name" => Some((function.display_name().to_owned(), None)),
                        "length" => Some((String::new(), Some(function.arity()))),
                        _ => None,
                    }
                };
                match metadata {
                    Some((name, None)) => Ok(self
                        .realm
                        .state
                        .borrow_mut()
                        .heap
                        .allocate_string(Arc::<str>::from(name))),
                    Some((_, Some(arity))) => Ok(RawValue::Number(usize_to_number(arity))),
                    None => Ok(RawValue::Undefined),
                }
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

    fn set_property_raw(
        &mut self,
        target: RawValue,
        property: String,
        value: RawValue,
        span: Option<SourceSpan>,
    ) -> EvalResult<()> {
        let mut state = self.realm.state.borrow_mut();
        match target {
            RawValue::Object(id) => {
                let Some(object) = state.heap.object_mut(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "object handle is invalid",
                        span,
                    ));
                };
                object.properties.insert(property, value);
                Ok(())
            }
            RawValue::Function(id) => {
                let Some(function) = state.heap.function_mut(id) else {
                    return Err(self.runtime_error(
                        ErrorKind::InternalError,
                        "function handle is invalid",
                        span,
                    ));
                };
                function.properties.insert(property, value);
                Ok(())
            }
            _ => Err(self.runtime_error(
                ErrorKind::TypeError,
                format!("cannot set property on {}", target.type_name()),
                span,
            )),
        }
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
                state.heap.allocate_object(HashMap::from([
                    ("name".to_owned(), name),
                    ("message".to_owned(), message),
                ]))
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

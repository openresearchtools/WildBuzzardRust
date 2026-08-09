use std::{
    hash::{Hash, Hasher},
    marker::PhantomData,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    rc::Rc,
};

use rand::{SeedableRng, rngs::StdRng};
use timezone_provider::tzif::CompiledTzdbProvider;

use crate::{
    common::{
        constants::NANOSECONDS_IN_ONE_MILLISECOND,
        filesystem::FileNameReserver,
        numeric::Numeric,
        options::Options,
        serialized_heap::SerializedHeap,
        time::{get_current_unix_time_millis, get_current_unix_time_nanos},
        wtf_8::{Wtf8Str, Wtf8String},
    },
    eval_err, handle_scope, impl_hash_map_instance, must_a,
    parser::{
        ParseContext, analyze::analyze, parse_module, parse_script, print_program, source::Source,
    },
    runtime::{
        EvalResult, Handle, HeapPtr, SymbolValue, Value,
        alloc_error::AllocResult,
        array_properties::{ArrayProperties, DenseArrayProperties},
        builtin_names::{BuiltinNames, BuiltinSymbols},
        bytecode::{
            generator::{BytecodeProgramGenerator, BytecodeScript},
            vm::VM,
        },
        collections::{
            FastHasher, HashDosResistantHasher, HashMapInstance, VecInstance,
            hash_map::BsHashMapField, index_map::IndexMapInstance, vec::ValueVec,
        },
        common_shapes::CommonShape,
        error::BsResult,
        gc::{
            GarbageCollector, GcType, HandleScopeGuard, Heap, HeapItem, HeapRootsDeserializer,
            HeapVisitor,
        },
        interned_strings::InternedStrings,
        intrinsics::{intrinsics::Intrinsic, rust_runtime::RustRuntimeFunctionRegistry},
        module::{
            execute::execute_module,
            import_attributes::ImportAttributes,
            module::{DynModule, HeapDynModule},
            source_text_module::SourceTextModule,
        },
        object_value::{NamedPropertiesMap, ObjectValue},
        realm::Realm,
        shape::Shape,
        shape_registry::ShapeRegistry,
        string_value::{FlatString, StringValue},
        tasks::TaskQueue,
    },
};

/// Internal, non-owning pointer to the top-level JS runtime context.
/// Must never be moved, as there may be internal pointers held.
///
/// Includes properties from section Agent (https://tc39.es/ecma262/#sec-agents)
///
/// Contexts are always represented by a pointer to the Context itself. A mutable reference to a
/// Context can be obtained from any reference to the heap. To avoid breaking Rust's mutable
/// aliasing rules, we must pass around a Context pointer that allows deref access to its individual
/// fields instead of passing around a `&mut Context`. This allows us to safely interweave Context
/// mutations from different Context pointers.
///
/// This token does not own the allocation and must never outlive its [`OwnedContext`]. Wild
/// Buzzard's safe embedding API never hands this token out. It remains copyable only while the
/// upstream VM is migrated away from pervasive by-value context parameters.
///
/// # Safety boundary
///
/// Obtaining this token outside `brimstone_core` requires calling
/// [`OwnedContext::raw_context_unchecked`]. Once obtained, every copy must stay on the owning
/// thread, must not outlive the owner, and must not be used concurrently with another copy to
/// create overlapping mutable references. Violating any of these invariants is undefined
/// behavior.
#[derive(Copy, Clone)]
pub struct Context {
    ptr: NonNull<ContextCell>,
}

pub struct ContextCell {
    pub heap: Heap,
    pub names: BuiltinNames,
    pub symbols: BuiltinSymbols,
    pub shapes: ShapeRegistry,
    pub rust_runtime_functions: RustRuntimeFunctionRegistry,

    /// The virtual machine used to execute bytecode.
    pub vm: Option<Box<VM>>,

    /// Intrusive head of the native baseline-JIT root-frame chain.
    ///
    /// This exists only in the off-by-default contained JIT feature. Frames are linked through a
    /// lifetime-branded RAII owner and must be absent before the context is destroyed.
    #[cfg(feature = "baseline_jit")]
    jit_frame_head: *mut crate::runtime::jit::abi::JitShadowFrame,

    /// The initial realm for this context. Either provided by the host environment or set up during
    /// context initialization.
    initial_realm: HeapPtr<Realm>,

    /// The task queue of all pending tasks.
    task_queue: TaskQueue,

    // Canonical values
    undefined: Value,
    null: Value,
    empty: Value,
    true_: Value,
    false_: Value,
    zero: Value,
    one: Value,
    negative_one: Value,
    nan: Value,

    /// Canonical string values for strings that appear in the AST
    pub interned_strings: InternedStrings,

    /// All symbols that have been registered under a specific key with `Symbol.for`.
    global_symbol_registry: HeapPtr<GlobalSymbolRegistryMap>,

    /// Cache modules by their canonical absolute path and import attributes
    pub modules: HeapPtr<ModuleCacheMap>,

    /// An empty value vector (used as the initial value for named properties arrays)
    pub default_named_properties_array: HeapPtr<ValueVec>,

    /// An empty named properties map to use as the initial value for named properties maps
    pub default_named_properties_map: HeapPtr<NamedPropertiesMap>,

    /// An empty, dense array properties object to use as the initial value for array properties
    pub default_array_properties: HeapPtr<ArrayProperties>,

    /// Options passed to this program.
    pub options: Rc<Options>,

    /// Reserves unique file paths for any debug files written during this session.
    pub debug_file_name_reserver: FileNameReserver,

    /// Set once module resolution has been completed.
    pub has_finished_module_resolution: bool,

    /// Counter for the [[AsyncEvaluation]] slot of SourceTextModule
    pub async_evaluation_counter: NonZeroUsize,

    /// Random number generator used within this context.
    pub rand: StdRng,

    /// If set, this is the unix time in nanoseconds.
    mocked_unix_time_nanos: Option<u128>,

    /// Time zone provider used for Temporal operations.
    temporal_provider: CompiledTzdbProvider,
}

/// Exactly-once owner of a Brimstone runtime.
///
/// Moving this value does not move the pointed-to [`ContextCell`]. The `Rc` marker deliberately
/// makes the owner `!Send + !Sync`: VM state, handle scopes, and collector metadata are currently
/// thread-affine. Dropping the owner always destroys the context exactly once, including while
/// unwinding.
pub struct OwnedContext {
    raw: Context,
    _thread_affinity: PhantomData<Rc<()>>,
}

/// Lifetime-scoped authority for contained JIT work on one owned context.
///
/// The higher-ranked constructor on [`OwnedContext`] prevents this token, and therefore any
/// activation built from it, from escaping the owner borrow. It deliberately exposes no safe raw
/// context or frame constructor.
#[cfg(feature = "baseline_jit")]
pub(crate) struct JitContextScope<'scope> {
    raw: Context,
    _brand: PhantomData<&'scope mut OwnedContext>,
}

/// Lifetime-branded root scope for safe host integration.
///
/// Values rooted through this scope are backed by Brimstone's moving-GC handle stack. The brand is
/// introduced by [`OwnedContext::with_root_scope`], whose higher-ranked closure prevents a
/// [`Rooted`] value from escaping in safe Rust.
pub struct RootScope<'scope> {
    raw: Context,
    _guard: HandleScopeGuard,
    _brand: PhantomData<&'scope mut &'scope ()>,
}

/// A moving-GC root that cannot outlive the [`RootScope`] which allocated it.
///
/// Unlike the upstream raw handle, this type is neither `Copy` nor mutable-dereferenceable and
/// does not expose its storage pointer through safe APIs.
pub struct Rooted<'scope, T> {
    handle: Handle<T>,
    _brand: PhantomData<&'scope mut &'scope ()>,
}

impl Context {
    fn new(options: Rc<Options>) -> AllocResult<OwnedContext> {
        let cx_cell = Box::new(ContextCell {
            heap: Heap::new(options.min_heap_size),
            global_symbol_registry: HeapPtr::uninit(),
            names: BuiltinNames::uninit(),
            symbols: BuiltinSymbols::uninit(),
            shapes: ShapeRegistry::uninit_empty(),
            rust_runtime_functions: RustRuntimeFunctionRegistry::new(),
            vm: None,
            #[cfg(feature = "baseline_jit")]
            jit_frame_head: std::ptr::null_mut(),
            initial_realm: HeapPtr::uninit(),
            task_queue: TaskQueue::new(),
            undefined: Value::undefined(),
            null: Value::null(),
            empty: Value::empty(),
            true_: Value::bool(true),
            false_: Value::bool(false),
            zero: Value::smi(0),
            one: Value::smi(1),
            negative_one: Value::smi(-1),
            nan: Value::nan(),
            interned_strings: InternedStrings::uninit(),
            modules: HeapPtr::uninit(),
            default_named_properties_array: HeapPtr::uninit(),
            default_named_properties_map: HeapPtr::uninit(),
            default_array_properties: HeapPtr::uninit(),
            options: options.clone(),
            debug_file_name_reserver: FileNameReserver::new(),
            has_finished_module_resolution: false,
            async_evaluation_counter: NonZeroUsize::MIN,
            // We want the initial heap generation to be deterministic so use seeded PRNG. After
            // initial heap has been set up switch to a PRNG seeded from a random source.
            rand: StdRng::from_seed([0; 32]),
            mocked_unix_time_nanos: None,
            temporal_provider: CompiledTzdbProvider::default(),
        });

        let raw = Context { ptr: NonNull::from(Box::leak(cx_cell)) };
        // Establish ownership before any fallible initialization. This guarantees that a failed
        // initialization destroys the partially initialized context instead of leaking it.
        let owner = OwnedContext { raw, _thread_affinity: PhantomData };
        let mut cx = raw;

        cx.heap.info().set_context(cx);
        cx.vm = Some(Box::new(VM::new(cx)));

        if let Some(serialized_heap) = options.serialized_heap {
            cx.init_heap_from_serialized(serialized_heap);
        } else {
            cx.init_heap_allocated_context_fields()?;
        }

        // Stop using deterministic PRNG
        cx.rand = StdRng::from_entropy();

        Ok(owner)
    }

    fn init_heap_allocated_context_fields(&mut self) -> AllocResult<()> {
        let mut cx = *self;

        // Initialize all uninitialized fields
        handle_scope!(cx, {
            cx.shapes = ShapeRegistry::new(cx)?;
            ShapeRegistry::init(cx)?;
            InternedStrings::init(cx)?;

            cx.init_builtin_names()?;
            cx.init_builtin_symbols()?;

            cx.global_symbol_registry = GlobalSymbolRegistryMap::new_initial(cx)?;
            cx.modules = ModuleCacheMap::new_initial(cx)?;

            cx.default_named_properties_array = ValueVec::new(cx, 0)?;
            cx.default_named_properties_map = NamedPropertiesMap::new(cx, 0)?;
            cx.default_array_properties = DenseArrayProperties::new(cx, 0)?.cast();

            cx.initial_realm = *Realm::new(cx)?;

            Ok(())
        })?;

        // Stop allocating into the permanent heap
        cx.heap.mark_current_semispace_as_permanent();

        Ok(())
    }

    /// Initialize this context from a serialized heap.
    ///
    /// Deserializes heap including fixing up heap roots for all heap allocated fields in the
    /// context.
    fn init_heap_from_serialized(&mut self, serialized: &SerializedHeap) {
        let mut cx = *self;

        // Initialize all uninitialized fields
        cx.shapes = ShapeRegistry::uninit();

        // Deserialize the heap roots
        HeapRootsDeserializer::deserialize(cx, serialized);
        self.heap.init_from_serialized(cx, serialized);
    }

    pub fn vm(&mut self) -> &mut VM {
        self.vm.as_mut().unwrap()
    }

    pub fn task_queue(&mut self) -> &mut TaskQueue {
        &mut self.task_queue
    }

    #[inline]
    pub fn initial_realm_ptr(&self) -> HeapPtr<Realm> {
        self.initial_realm
    }

    #[inline]
    pub fn initial_realm(&self) -> Handle<Realm> {
        self.initial_realm_ptr().to_handle()
    }

    pub fn evaluate_script(&mut self, source: Rc<Source>) -> BsResult<()> {
        // Parse script and perform semantic analysis
        let pcx = ParseContext::new(source);
        let parse_result = parse_script(&pcx, self.options.clone())?;

        if self.options.print_ast {
            println!("{}", print_program(&parse_result));
        }

        if self.options.parse_stats {
            println!("{:#?}", pcx.stats());
        }

        let analyzed_result = analyze(parse_result)?;

        // Generate bytecode for the program
        let bytecode_script = BytecodeProgramGenerator::generate_from_parse_script_result(
            *self,
            &analyzed_result,
            self.initial_realm(),
        )?;

        // Execute in the bytecode interpreter
        self.run_script(bytecode_script)?;

        Ok(())
    }

    pub fn evaluate_module(&mut self, source: Rc<Source>) -> BsResult<()> {
        // Parse module and perform semantic analysis
        let pcx = ParseContext::new(source);
        let parse_result = parse_module(&pcx, self.options.clone())?;

        if self.options.print_ast {
            println!("{}", print_program(&parse_result));
        }

        if self.options.parse_stats {
            println!("{:#?}", pcx.stats());
        }

        let analyzed_result = analyze(parse_result)?;

        // Generate bytecode for the program
        let module = BytecodeProgramGenerator::generate_from_parse_module_result(
            *self,
            &analyzed_result,
            self.initial_realm(),
        )?;

        // Load modules and execute in the bytecode interpreter
        self.run_module(module)?;

        Ok(())
    }

    /// Execute a program, running until the task queue is empty.
    pub fn run_script(&mut self, bytecode_script: BytecodeScript) -> EvalResult<()> {
        self.with_initial_realm_stack_frame(self.initial_realm_ptr(), |mut cx| {
            cx.vm().execute_script(bytecode_script)
        })?;

        self.run_all_tasks()?;

        Ok(())
    }

    /// Execute a module, loading and executing all dependencies. Run until the task queue is empty.
    pub fn run_module(&mut self, module: Handle<SourceTextModule>) -> EvalResult<()> {
        // Loading, linking, and evaluation should all have a current realm set as some objects
        // needing a realm will be created.
        let promise = self
            .with_initial_realm_stack_frame(module.program_function_ptr().realm_ptr(), |cx| {
                Ok(execute_module(cx, module)?)
            })?;

        self.run_all_tasks()?;

        debug_assert!(!promise.is_pending());

        if let Some(value) = promise.rejected_value() {
            return eval_err!(value.to_handle(*self));
        }

        Ok(())
    }

    pub fn with_initial_realm_stack_frame<T>(
        &mut self,
        realm: HeapPtr<Realm>,
        f: impl FnOnce(Context) -> EvalResult<T>,
    ) -> EvalResult<T> {
        self.vm().debug_assert_stack_empty();

        let push_frame_result = self.vm().push_initial_realm_stack_frame(realm);
        assert!(push_frame_result.is_ok(), "Initial realm frame overflowed stack");

        // Always mark the top of the stack trace under the initial realm frame
        self.vm().mark_stack_trace_top();

        let result = f(*self);

        self.vm().pop_initial_realm_stack_frame();

        result
    }

    pub fn insert_module(
        &mut self,
        cache_key: ModuleCacheKey,
        module: DynModule,
    ) -> AllocResult<()> {
        ModuleCacheField
            .maybe_grow_for_insertion(*self)?
            .insert_without_growing(cache_key.into_heap(), module.to_heap());

        Ok(())
    }

    pub fn alloc_uninit<T>(&self) -> AllocResult<HeapPtr<T>> {
        Heap::alloc_uninit::<T>(*self)
    }

    pub fn alloc_uninit_with_size<T>(&self, size: usize) -> AllocResult<HeapPtr<T>> {
        Heap::alloc_uninit_with_size::<T>(*self, size)
    }

    #[inline]
    pub fn current_realm_ptr(&self) -> HeapPtr<Realm> {
        self.vm
            .as_ref()
            .unwrap()
            .closure()
            .function_ptr()
            .realm_ptr()
    }

    #[inline]
    pub fn current_realm(&self) -> Handle<Realm> {
        self.current_realm_ptr().to_handle()
    }

    /// Return an intrinsic for the current realm.
    #[inline]
    pub fn get_intrinsic_ptr(&self, intrinsic: Intrinsic) -> HeapPtr<ObjectValue> {
        self.current_realm().get_intrinsic_ptr(intrinsic)
    }

    #[inline]
    pub fn get_intrinsic(&self, intrinsic: Intrinsic) -> Handle<ObjectValue> {
        self.current_realm().get_intrinsic(intrinsic)
    }

    /// Whether an object is a particular intrinsic of the current realm.
    #[inline]
    pub fn is_intrinsic(&self, object: HeapPtr<ObjectValue>, intrinsic: Intrinsic) -> bool {
        object.ptr_eq(&self.get_intrinsic_ptr(intrinsic))
    }

    pub fn get_common_shape(&self, common_shape: CommonShape) -> AllocResult<Handle<Shape>> {
        self.current_realm().get_common_shape(*self, common_shape)
    }

    pub fn current_function(&mut self) -> Handle<ObjectValue> {
        self.vm().closure().to_handle().into()
    }

    pub fn current_new_target(&mut self) -> Option<Handle<ObjectValue>> {
        let new_target_index = self.vm().closure().function().new_target_index();
        if let Some(index) = new_target_index {
            let new_target = self.vm().get_register_at_index(index);
            if new_target.is_undefined() {
                return None;
            }

            debug_assert!(new_target.is_object());
            Some(new_target.as_object().to_handle())
        } else {
            None
        }
    }

    pub fn global_symbol_registry(&self) -> HeapPtr<GlobalSymbolRegistryMap> {
        self.global_symbol_registry
    }

    pub fn global_symbol_registry_field(&mut self) -> GlobalSymbolRegistryField {
        GlobalSymbolRegistryField
    }

    /// Returns the current unix time in milliseconds, which may be mocked.
    pub fn current_unix_time_millis(&self) -> u128 {
        if let Some(mocked_unix_time_nanos) = self.mocked_unix_time_nanos {
            mocked_unix_time_nanos / (NANOSECONDS_IN_ONE_MILLISECOND as u128)
        } else {
            get_current_unix_time_millis()
        }
    }

    /// Returns the current unix time in nanoseconds, which may be mocked.
    pub fn current_unix_time_nanos(&self) -> u128 {
        if let Some(mocked_unix_time_nanos) = self.mocked_unix_time_nanos {
            mocked_unix_time_nanos
        } else {
            get_current_unix_time_nanos()
        }
    }

    pub fn temporal_provider(&self) -> &CompiledTzdbProvider {
        &self.temporal_provider
    }

    pub fn print_or_add_to_dump_buffer(&self, str: &str) {
        if let Some(mut buffer) = self.options.dump_buffer() {
            if !buffer.is_empty() {
                buffer.push('\n');
            }

            buffer.push_str(str);
        } else {
            println!("{str}");
        }
    }

    #[inline]
    pub fn alloc_string_ptr(&mut self, str: &str) -> EvalResult<HeapPtr<FlatString>> {
        FlatString::from_wtf8(*self, str.as_bytes())
    }

    #[inline]
    pub fn alloc_static_string_ptr(
        &mut self,
        str: &'static str,
    ) -> AllocResult<HeapPtr<FlatString>> {
        // Assumes that all static strings are less than the maximum string length
        Ok(must_a!(self.alloc_string_ptr(str)))
    }

    #[inline]
    pub fn alloc_wtf8_string_ptr(&mut self, str: &Wtf8String) -> EvalResult<HeapPtr<FlatString>> {
        FlatString::from_wtf8(*self, str.as_bytes())
    }

    #[inline]
    pub fn alloc_wtf8_str_ptr(&mut self, str: &Wtf8Str) -> EvalResult<HeapPtr<FlatString>> {
        FlatString::from_wtf8(*self, str.as_bytes())
    }

    #[inline]
    pub fn alloc_static_wtf8_str_ptr(
        &mut self,
        str: &'static Wtf8Str,
    ) -> AllocResult<HeapPtr<FlatString>> {
        // Assumes that all static strings are less than the maximum string length
        Ok(must_a!(self.alloc_wtf8_str_ptr(str)))
    }

    #[inline]
    pub fn alloc_string(&mut self, str: &str) -> EvalResult<Handle<StringValue>> {
        Ok(self.alloc_string_ptr(str)?.as_string().to_handle())
    }

    #[inline]
    pub fn alloc_static_string(&mut self, str: &'static str) -> AllocResult<Handle<StringValue>> {
        Ok(self.alloc_static_string_ptr(str)?.as_string().to_handle())
    }

    #[inline]
    pub fn alloc_flat_string(&mut self, str: &str) -> EvalResult<Handle<FlatString>> {
        Ok(self.alloc_string_ptr(str)?.to_handle())
    }

    #[inline]
    pub fn alloc_wtf8_string(&mut self, str: &Wtf8String) -> EvalResult<Handle<FlatString>> {
        Ok(self.alloc_wtf8_string_ptr(str)?.to_handle())
    }

    #[inline]
    pub fn alloc_wtf8_str(&mut self, str: &Wtf8Str) -> EvalResult<Handle<FlatString>> {
        Ok(self.alloc_wtf8_str_ptr(str)?.to_handle())
    }

    #[inline]
    pub fn undefined(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.undefined)
    }

    #[inline]
    pub fn null(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.null)
    }

    #[inline]
    pub fn empty(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.empty)
    }

    #[inline]
    pub fn bool(&self, value: bool) -> Handle<Value> {
        if value {
            Handle::<Value>::from_fixed_non_heap_ptr(&self.true_)
        } else {
            Handle::<Value>::from_fixed_non_heap_ptr(&self.false_)
        }
    }

    #[inline]
    pub fn zero(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.zero)
    }

    #[inline]
    pub fn one(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.one)
    }

    #[inline]
    pub fn negative_one(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.negative_one)
    }

    #[inline]
    pub fn nan(&self) -> Handle<Value> {
        Handle::<Value>::from_fixed_non_heap_ptr(&self.nan)
    }

    #[inline]
    pub fn smi<T: Numeric>(&self, value: T) -> Handle<Value> {
        Value::smi(value).to_handle(*self)
    }

    #[inline]
    pub fn number<T: Numeric>(&self, value: T) -> Handle<Value> {
        Value::number(value).to_handle(*self)
    }

    /// Visit all heap roots for a garbage collection. This optionally visits pointers that are
    /// guaranteed to be in the permanent semispace.
    pub fn visit_roots_for_gc(&mut self, gc: &mut GarbageCollector) {
        self.visit_common_roots(gc);
        self.visit_post_initialization_roots(gc);

        // Only need to visit permanent roots if growing the heap, otherwise permanent space is
        // guaranteed to not move and .
        if gc.is_resizing() {
            self.visit_permanent_roots(gc);
        }
    }

    /// Visit all heap roots that are needed for heap serialization. This includes all pointers to
    /// the permanent semispace.
    pub fn visit_roots_for_serialization(&mut self, visitor: &mut impl HeapVisitor) {
        self.visit_common_roots(visitor);
        self.visit_permanent_roots(visitor);

        // Intentionally do not need to visit_post_initialization_roots
    }

    /// Visit all heap roots that should always be visited.
    fn visit_common_roots(&mut self, visitor: &mut impl HeapVisitor) {
        self.shapes.visit_common_roots(visitor);
        visitor.visit_pointer(&mut self.global_symbol_registry);
        self.interned_strings.visit_roots(visitor);
        visitor.visit_pointer(&mut self.modules);
    }

    /// Visit all heap roots that are guaranteed to point to the permanent semispace.
    fn visit_permanent_roots(&mut self, visitor: &mut impl HeapVisitor) {
        self.names.visit_roots(visitor);
        self.symbols.visit_roots(visitor);
        self.shapes.visit_permanent_roots(visitor);
        visitor.visit_pointer(&mut self.initial_realm);

        visitor.visit_pointer(&mut self.default_named_properties_array);
        visitor.visit_pointer(&mut self.default_named_properties_map);
        visitor.visit_pointer(&mut self.default_array_properties);
    }

    /// Visit all heap roots that can only actually contain roots after the context has been
    /// initialized.
    fn visit_post_initialization_roots(&mut self, visitor: &mut impl HeapVisitor) {
        self.heap.visit_roots(visitor);
        self.task_queue.visit_roots(visitor);

        if let Some(vm) = &mut self.vm {
            vm.visit_roots(visitor);
        }

        #[cfg(feature = "baseline_jit")]
        {
            // SAFETY: Only `ActivationOwner` can link a frame. It keeps the slot and immutable
            // metadata borrows alive, publishes a validated safepoint before any allocating
            // helper, and unlinks exactly LIFO on every exit path. Corruption fails closed inside
            // the walker rather than allowing collection through unchecked storage.
            unsafe {
                crate::runtime::jit::abi::visit_registered_roots(self.jit_frame_head, visitor)
            }
        }
    }

    /// Raw context identity stored in the private generated-code activation schema.
    #[cfg(feature = "baseline_jit")]
    pub(crate) fn jit_raw_identity(&self) -> *mut () {
        self.ptr.as_ptr().cast()
    }

    /// Recover a non-owning context token from a validated live activation.
    ///
    /// # Safety
    ///
    /// `ptr` must be the unchanged identity of a live `JitContextScope` on the owner thread, and
    /// the returned token must not outlive that scope or create overlapping mutable references.
    #[cfg(feature = "baseline_jit")]
    pub(crate) unsafe fn from_jit_raw_identity(ptr: *mut ()) -> Self {
        Self {
            // SAFETY: Required by this function's contract.
            ptr: unsafe { NonNull::new_unchecked(ptr.cast()) },
        }
    }

    #[cfg(feature = "baseline_jit")]
    pub(crate) fn jit_frame_head(&self) -> *mut crate::runtime::jit::abi::JitShadowFrame {
        self.jit_frame_head
    }

    /// Replace the intrusive native-frame head.
    ///
    /// # Safety
    ///
    /// Only the lifetime-branded activation owner may call this. `new_head` must either be null or
    /// identify its live borrowed frame, and callers must preserve exact LIFO linkage.
    #[cfg(feature = "baseline_jit")]
    pub(crate) unsafe fn set_jit_frame_head(
        &mut self,
        new_head: *mut crate::runtime::jit::abi::JitShadowFrame,
    ) {
        self.jit_frame_head = new_head;
    }

    #[cfg(feature = "gc_stress_test")]
    pub fn enable_gc_stress_test(&mut self) {
        self.heap.gc_stress_test = true;
    }
}

impl OwnedContext {
    /// Evaluate a script while keeping all raw context tokens scoped to this owner.
    pub fn evaluate_script(&mut self, source: Rc<Source>) -> BsResult<()> {
        let mut raw = self.raw;
        raw.evaluate_script(source)
    }

    /// Evaluate a module while keeping all raw context tokens scoped to this owner.
    pub fn evaluate_module(&mut self, source: Rc<Source>) -> BsResult<()> {
        let mut raw = self.raw;
        raw.evaluate_module(source)
    }

    /// Execute already generated script bytecode owned by this runtime.
    ///
    /// # Safety
    ///
    /// The bytecode and every embedded heap pointer must belong to this runtime and remain rooted.
    #[doc(hidden)]
    pub unsafe fn run_script_unchecked(
        &mut self,
        bytecode_script: BytecodeScript,
    ) -> EvalResult<()> {
        let mut raw = self.raw;
        raw.run_script(bytecode_script)
    }

    /// Execute an already generated module owned by this runtime.
    ///
    /// The module handle must have been created from this context and must still be rooted in an
    /// active handle scope. This low-level method remains primarily for upstream test tooling.
    ///
    /// # Safety
    ///
    /// The handle must belong to this runtime, be live in its active handle scope, and must not be
    /// used after this call destroys that scope.
    #[doc(hidden)]
    pub unsafe fn run_module_unchecked(
        &mut self,
        module: Handle<SourceTextModule>,
    ) -> EvalResult<()> {
        let mut raw = self.raw;
        raw.run_module(module)
    }

    /// Install globals selected by the embedding options into the initial realm.
    pub fn install_optional_globals(&mut self) -> AllocResult<()> {
        let raw = self.raw;
        raw.initial_realm().install_optional_globals(raw)
    }

    /// Options used to construct this runtime.
    pub fn options(&self) -> &Rc<Options> {
        &self.raw.options
    }

    /// Enter a moving-GC root scope.
    ///
    /// The higher-ranked lifetime is intentionally chosen by this method, not by the caller. As a
    /// result a `Rooted` value cannot be returned from `f` or stored outside the closure without
    /// unsafe code.
    pub fn with_root_scope<R>(
        &mut self,
        f: impl for<'scope> FnOnce(&mut RootScope<'scope>) -> R,
    ) -> R {
        let raw = self.raw;
        let mut scope = RootScope { raw, _guard: HandleScopeGuard::new(raw), _brand: PhantomData };
        f(&mut scope)
    }

    /// Enter a non-escaping authority scope for the contained, product-disabled JIT proof.
    #[cfg(feature = "baseline_jit")]
    #[allow(dead_code)]
    pub(crate) fn with_jit_context<R>(
        &mut self,
        f: impl for<'scope> FnOnce(&mut JitContextScope<'scope>) -> R,
    ) -> R {
        let mut scope = JitContextScope { raw: self.raw, _brand: PhantomData };
        f(&mut scope)
    }

    /// Enable collection at every allocation for rooting tests.
    #[cfg(feature = "gc_stress_test")]
    pub fn enable_gc_stress_test(&mut self) {
        let mut raw = self.raw;
        raw.enable_gc_stress_test();
    }

    /// Expose the legacy copyable context token for upstream internals and test infrastructure.
    ///
    /// New embedding code must use the safe methods on `OwnedContext`. This escape hatch exists
    /// only until the VM and GC APIs carry lifetime-branded runtime/root scopes.
    ///
    /// # Safety
    ///
    /// Every returned token and all values derived from it must remain on the current thread and
    /// must be destroyed before this owner is dropped. Callers must serialize access and must not
    /// create overlapping mutable references through token copies. Handles must not outlive their
    /// active handle scope.
    #[doc(hidden)]
    pub unsafe fn raw_context_unchecked(&self) -> Context {
        self.raw
    }

    #[cfg(feature = "handle_stats")]
    pub fn handle_stats(&self) -> crate::runtime::gc::HandleStats {
        self.raw.heap.info().handle_context().handle_stats()
    }
}

impl<'scope> RootScope<'scope> {
    /// Allocate and root a JavaScript string in this scope.
    pub fn alloc_string(&mut self, value: &str) -> EvalResult<Rooted<'scope, StringValue>> {
        let mut raw = self.raw;
        let handle = raw.alloc_string(value)?;
        Ok(Rooted { handle, _brand: PhantomData })
    }

    /// Force a normal moving collection. This is useful for host-binding and safepoint tests.
    pub fn collect(&mut self) {
        Heap::run_gc(self.raw, GcType::Normal);
    }
}

impl Rooted<'_, StringValue> {
    /// Copy the rooted string into host-owned WTF-8 storage.
    pub fn to_wtf8_string(&self) -> EvalResult<Wtf8String> {
        self.handle.to_wtf8_string()
    }

    #[cfg(test)]
    fn heap_address(&self) -> *mut StringValue {
        self.handle.as_ptr()
    }
}

impl Drop for OwnedContext {
    fn drop(&mut self) {
        #[cfg(feature = "baseline_jit")]
        if !self.raw.jit_frame_head().is_null() {
            // A live native frame contains borrows into caller-owned slots and metadata. Allowing
            // the context to disappear first would be immediate use-after-free at the next helper
            // or unlink, so release builds fail closed too.
            std::process::abort();
        }

        // `raw` was created from one `Box::leak` in `Context::new`, and `OwnedContext` is neither
        // `Copy` nor `Clone`. Therefore this is the unique exactly-once reconstruction.
        unsafe { drop(Box::from_raw(self.raw.ptr.as_ptr())) }
    }
}

#[cfg(feature = "baseline_jit")]
impl JitContextScope<'_> {
    pub(crate) fn raw(&self) -> Context {
        self.raw
    }

    /// Execute with the initial realm installed in an ordinary VM frame.
    ///
    /// This supplies the same current-realm lookup used by the interpreter's `NewObject` handler.
    /// The frame is removed by an RAII guard even if contained setup code unwinds.
    pub(crate) fn with_initial_realm<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> EvalResult<T>,
    ) -> EvalResult<T> {
        self.raw.vm().debug_assert_stack_empty();
        let initial_realm = self.raw.initial_realm_ptr();
        self.raw
            .vm()
            .push_initial_realm_stack_frame(initial_realm)?;
        self.raw.vm().mark_stack_trace_top();

        struct InitialRealmFrameGuard(Context);
        impl Drop for InitialRealmFrameGuard {
            fn drop(&mut self) {
                self.0.vm().pop_initial_realm_stack_frame();
            }
        }

        let guard = InitialRealmFrameGuard(self.raw);
        let result = f(self);
        drop(guard);
        result
    }

    #[cfg(test)]
    pub(crate) fn has_registered_jit_frame(&self) -> bool {
        !self.raw.jit_frame_head().is_null()
    }
}

impl Deref for Context {
    type Target = ContextCell;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.as_ref() }
    }
}

impl DerefMut for Context {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.ptr.as_mut() }
    }
}

pub struct ContextBuilder {
    options: Option<Rc<Options>>,
    mocked_unix_time_nanos: Option<u128>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        Self { options: None, mocked_unix_time_nanos: None }
    }

    pub fn build(self) -> AllocResult<OwnedContext> {
        // Create default options if none were provided
        let options = self.options.unwrap_or_else(|| Rc::new(Options::default()));

        // Create default realm if one was not provided
        let mut cx = Context::new(options)?;

        cx.raw.mocked_unix_time_nanos = self.mocked_unix_time_nanos;

        Ok(cx)
    }

    pub fn set_options(mut self, options: Rc<Options>) -> Self {
        self.options = Some(options);
        self
    }

    pub fn mock_unix_time_nanos(mut self, time: u128) -> Self {
        self.mocked_unix_time_nanos = Some(time);
        self
    }
}

/// Modules are cached by their canonical path and import attributes.
#[derive(Clone, Eq, PartialEq)]
pub struct HeapModuleCacheKey {
    path: String,
    attributes: Option<HeapPtr<ImportAttributes>>,
}

pub struct ModuleCacheKey {
    path: String,
    attributes: Option<Handle<ImportAttributes>>,
}

impl ModuleCacheKey {
    pub fn new(path: String, attributes: Option<Handle<ImportAttributes>>) -> Self {
        Self { path, attributes }
    }

    pub fn into_heap(self) -> HeapModuleCacheKey {
        HeapModuleCacheKey::new(self.path, self.attributes.map(|attr| *attr))
    }
}

impl HeapModuleCacheKey {
    pub fn new(path: String, attributes: Option<HeapPtr<ImportAttributes>>) -> Self {
        Self { path, attributes }
    }
}

impl Hash for HeapModuleCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Attributes are intentionally not included in hash
        self.path.hash(state);
    }
}

impl_hash_map_instance!(ModuleCacheMap, HeapModuleCacheKey, HeapDynModule, HashDosResistantHasher);

pub struct ModuleCacheField;

impl BsHashMapField<ModuleCacheMap> for ModuleCacheField {
    fn get(&self, cx: Context) -> HeapPtr<ModuleCacheMap> {
        cx.modules
    }

    fn set_new(
        &mut self,
        mut cx: Context,
        capacity: usize,
    ) -> AllocResult<HeapPtr<ModuleCacheMap>> {
        let map = ModuleCacheMap::new(cx, capacity)?;
        cx.modules = map;
        Ok(map)
    }
}

impl HeapItem for ModuleCacheMap {
    fn byte_size(map: HeapPtr<Self>) -> usize {
        Self::calculate_size_in_bytes(map.capacity())
    }

    fn visit_pointers(mut map: HeapPtr<Self>, visitor: &mut impl HeapVisitor) {
        map.visit_map_pointers(visitor);

        for (cache_key, module) in map.iter_mut_gc_unsafe() {
            visitor.visit_pointer_opt(&mut cache_key.attributes);
            module.visit_pointers(visitor);
        }
    }
}

impl_hash_map_instance!(
    GlobalSymbolRegistryMap,
    HeapPtr<FlatString>,
    HeapPtr<SymbolValue>,
    FastHasher
);

pub struct GlobalSymbolRegistryField;

impl BsHashMapField<GlobalSymbolRegistryMap> for GlobalSymbolRegistryField {
    fn get(&self, cx: Context) -> HeapPtr<GlobalSymbolRegistryMap> {
        cx.global_symbol_registry
    }

    fn set_new(
        &mut self,
        mut cx: Context,
        capacity: usize,
    ) -> AllocResult<HeapPtr<GlobalSymbolRegistryMap>> {
        let map = GlobalSymbolRegistryMap::new(cx, capacity)?;
        cx.global_symbol_registry = map;
        Ok(map)
    }
}

impl HeapItem for GlobalSymbolRegistryMap {
    fn byte_size(map: HeapPtr<Self>) -> usize {
        Self::calculate_size_in_bytes(map.capacity())
    }

    fn visit_pointers(mut map: HeapPtr<Self>, visitor: &mut impl HeapVisitor) {
        map.visit_map_pointers(visitor);

        for (key, value) in map.iter_mut_gc_unsafe() {
            visitor.visit_pointer(key);
            visitor.visit_pointer(value);
        }
    }
}

#[cfg(test)]
mod ownership_tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::common::{options::OptionsBuilder, wtf_8::Wtf8String};

    // Compile-time negative assertions without adding another dependency to the imported engine.
    // If the type starts implementing the forbidden trait, selecting `AmbiguousIfImpl<_>` becomes
    // ambiguous and compilation fails.
    macro_rules! assert_not_impl {
        ($type:ty, $trait:path) => {
            const _: fn() = || {
                trait AmbiguousIfImpl<A> {
                    fn marker() {}
                }
                impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
                struct Invalid;
                impl<T: ?Sized + $trait> AmbiguousIfImpl<Invalid> for T {}

                let _ = <$type as AmbiguousIfImpl<_>>::marker;
            };
        };
    }

    assert_not_impl!(OwnedContext, Copy);
    assert_not_impl!(OwnedContext, Clone);
    assert_not_impl!(OwnedContext, Send);
    assert_not_impl!(OwnedContext, Sync);
    assert_not_impl!(Rooted<'static, StringValue>, Copy);
    assert_not_impl!(Rooted<'static, StringValue>, Clone);

    fn fresh_context() -> OwnedContext {
        let options = OptionsBuilder::new().serialized_heap(None).build().unwrap();
        ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .unwrap()
    }

    fn source(contents: &str) -> Rc<Source> {
        Rc::new(Source::new_for_string("<ownership-test>", Wtf8String::from_str(contents)).unwrap())
    }

    fn expect_ok<T, E>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(_) => panic!("expected successful runtime operation"),
        }
    }

    #[test]
    fn owner_is_exactly_once_drop_type() {
        assert!(std::mem::needs_drop::<OwnedContext>());

        for _ in 0..16 {
            let mut cx = fresh_context();
            assert_eq!(Rc::strong_count(&cx.raw.options), 1);
            expect_ok(cx.evaluate_script(source("const answer = 6 * 7;")));
            assert_eq!(
                Rc::strong_count(&cx.raw.options),
                1,
                "parsing must not leak an Options reference into the bump arena"
            );
        }
    }

    #[test]
    fn owner_and_root_scope_clean_up_during_unwind() {
        let mut cx = fresh_context();
        let baseline_handles = cx.raw.heap.info().handle_context().handle_count();

        let panic_result = catch_unwind(AssertUnwindSafe(|| {
            cx.with_root_scope(|scope| {
                let _first = expect_ok(scope.alloc_string("first rooted value"));
                let _second = expect_ok(scope.alloc_string("second rooted value"));
                panic!("intentional unwind");
            });
        }));

        assert!(panic_result.is_err());
        assert_eq!(cx.raw.heap.info().handle_context().handle_count(), baseline_handles);

        // A fresh evaluation after the unwind proves that the owner and handle stack remain live.
        expect_ok(cx.evaluate_script(source("const stillAlive = true;")));

        let owner_panic = catch_unwind(AssertUnwindSafe(|| {
            let _owned = fresh_context();
            panic!("drop owner while unwinding");
        }));
        assert!(owner_panic.is_err());

        // Miri/ASan exercise this as a double-free/UAF sentinel after unwind cleanup.
        drop(fresh_context());
    }

    #[test]
    fn rooted_string_tracks_moving_collection() {
        let mut cx = fresh_context();

        cx.with_root_scope(|scope| {
            let rooted = expect_ok(scope.alloc_string("a non-permanent string that must move"));
            let before = rooted.heap_address();

            scope.collect();

            let after = rooted.heap_address();
            assert_ne!(before, after, "normal collection must relocate semispace data");
            assert_eq!(
                expect_ok(rooted.to_wtf8_string()).as_bytes(),
                b"a non-permanent string that must move"
            );
        });
    }

    #[test]
    fn rooted_string_and_handle_stack_survive_heap_resize() {
        let mut cx = fresh_context();
        let original_heap_size = cx.raw.heap.heap_size();

        cx.with_root_scope(|scope| {
            let rooted = expect_ok(scope.alloc_string("root retained across resize"));
            let before = rooted.heap_address();

            Heap::run_gc(scope.raw, GcType::Grow { alloc_size: None });

            assert!(scope.raw.heap.heap_size() > original_heap_size);
            assert_ne!(rooted.heap_address(), before);
            assert_eq!(
                expect_ok(rooted.to_wtf8_string()).as_bytes(),
                b"root retained across resize"
            );
        });

        expect_ok(cx.evaluate_script(source("const resizedHeapStillRuns = 1;")));
    }

    #[cfg(feature = "gc_stress_test")]
    #[test]
    fn safe_facade_survives_collection_at_every_allocation() {
        let mut cx = fresh_context();
        cx.enable_gc_stress_test();
        expect_ok(cx.evaluate_script(source(
            "let total = 0; for (let i = 0; i < 200; i++) { \
             const value = { index: i, text: 'root-' + i }; total += value.index; } \
             if (total !== 19900) throw new Error('bad total');",
        )));
    }
}

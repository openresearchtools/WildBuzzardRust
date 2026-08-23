//! Scoped host capability for the first bounded Brimstone-to-DOM task bridge.
//!
//! The JavaScript heap never stores a DOM pointer. Host nodes are represented by exact integer
//! tokens which are meaningful only to one caller-owned [`BrowserHostTask`]. The active host is
//! borrowed for one synchronous classic-script or microtask-checkpoint phase and erased behind a
//! private function table; an RAII guard removes that erased borrow before the higher-ranked realm
//! callback can return.
//!
//! This is deliberately an internal binding vocabulary rather than a WebIDL surface. It proves a
//! rooted task seam for a small set of real DOM operations while WebIDL wrappers, DOMException,
//! events, custom-element reactions, mutation observers, and general page admission remain open.

use std::{marker::PhantomData, ptr::NonNull};

use crate::{
    handle_scope,
    runtime::{
        Context, EvalResult, Handle, PropertyDescriptor, PropertyKey, Value,
        abstract_operations::define_property_or_throw,
        builtin_function::BuiltinFunction,
        error::type_error,
        eval_result::EvalError,
        intrinsics::{intrinsics::Intrinsic, rust_runtime::RuntimeFunctionId},
        object_value::ObjectValue,
        ordinary_object::ObjectBuilder,
        to_string,
    },
    runtime_fn,
};

use super::browser_script::{ClassicScriptExecution, MicrotaskCheckpointExecution};

/// Largest integer which can make an exact round trip through a JavaScript `Number`.
pub const MAX_BROWSER_HOST_NODE_TOKEN: u64 = (1_u64 << 53) - 1;

/// Pointer-free identity of one host-root table entry in one exact browser task.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrowserHostNodeToken(u64);

impl BrowserHostNodeToken {
    /// Construct a nonzero token which is exactly representable as a JavaScript `Number`.
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 || value > MAX_BROWSER_HOST_NODE_TOKEN {
            None
        } else {
            Some(Self(value))
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Pointer-free identity of one exact host document state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserHostDocumentVersion {
    document_id: u64,
    revision: u64,
}

impl BrowserHostDocumentVersion {
    pub const fn new(document_id: u64, revision: u64) -> Self {
        Self { document_id, revision }
    }

    pub const fn document_id(self) -> u64 {
        self.document_id
    }

    pub const fn revision(self) -> u64 {
        self.revision
    }
}

/// Scalar evidence for the synchronous DOM calls completed during one script/checkpoint phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserHostPhaseCommit {
    before: BrowserHostDocumentVersion,
    after: BrowserHostDocumentVersion,
    commands: u32,
    created_nodes: u32,
}

impl BrowserHostPhaseCommit {
    pub const fn new(
        before: BrowserHostDocumentVersion,
        after: BrowserHostDocumentVersion,
        commands: u32,
        created_nodes: u32,
    ) -> Self {
        Self { before, after, commands, created_nodes }
    }

    pub const fn before(self) -> BrowserHostDocumentVersion {
        self.before
    }

    pub const fn after(self) -> BrowserHostDocumentVersion {
        self.after
    }

    pub const fn commands(self) -> u32 {
        self.commands
    }

    pub const fn created_nodes(self) -> u32 {
        self.created_nodes
    }
}

/// End-of-phase result returned by a concrete host task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserHostCommitOutcome {
    NoChanges(BrowserHostDocumentVersion),
    Committed(BrowserHostPhaseCommit),
}

/// Engine-neutral rejection from a concrete DOM task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserHostError {
    /// WebIDL-like argument conversion or a deliberately unsupported string representation.
    InvalidArgument,
    /// A token does not resolve to a live node of the required kind.
    InvalidNode,
    /// The requested tree/attribute/character-data operation is invalid.
    InvalidOperation,
    /// A token belongs to another browser task generation.
    StaleTask,
    /// The responsible document/navigation is no longer current.
    StaleDocument,
    /// The live document no longer has the exact version owned by this task.
    VersionMismatch,
    /// The fixed task command, creation, or string budget was exhausted.
    LimitExceeded,
    /// Fallible host bookkeeping could not reserve memory.
    Allocation,
    /// The browser cancelled the host task independently of the JS interrupt flag.
    Cancelled,
    /// The reserved binding name could not be installed in the exact initial realm.
    BindingCollision,
    /// A private registry or lifecycle invariant failed closed.
    Internal,
}

impl BrowserHostError {
    fn is_script_exception(self) -> bool {
        matches!(self, Self::InvalidArgument | Self::InvalidNode | Self::InvalidOperation)
    }

    fn script_message(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid bounded DOM host argument",
            Self::InvalidNode => "DOM host node is unavailable or has the wrong kind",
            Self::InvalidOperation => "bounded DOM host operation was rejected",
            _ => "bounded DOM host task failed",
        }
    }
}

/// One browser-owned DOM task. Implementations retain real host roots; Brimstone retains only the
/// scalar tokens they return. Returning a non-script error before disposition must leave the
/// current phase safe to abort. `finish_phase` is one-way: `Ok` commits the phase and forbids any
/// later abort, while `Err` rejects the finish and must leave exactly one abort fallback safe.
/// `abort_phase` must not allocate and permanently discards the phase after cancellation or a
/// runtime resource failure. Either callback may observe cancellation/deadline changes internally;
/// those changes apply only at the next phase/session poll and never relabel a returned disposition.
pub trait BrowserHostTask {
    /// Validate task/document liveness before any JavaScript or queued job in this phase runs.
    fn validate_phase(&mut self) -> Result<(), BrowserHostError>;
    fn document_node(&mut self) -> Result<BrowserHostNodeToken, BrowserHostError>;
    fn lookup_node(&mut self, slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError>;
    fn create_html_element(
        &mut self,
        local_name: &str,
    ) -> Result<BrowserHostNodeToken, BrowserHostError>;
    fn create_text(&mut self, data: &str) -> Result<BrowserHostNodeToken, BrowserHostError>;
    fn append_child(
        &mut self,
        parent: BrowserHostNodeToken,
        child: BrowserHostNodeToken,
    ) -> Result<(), BrowserHostError>;
    fn set_html_attribute(
        &mut self,
        element: BrowserHostNodeToken,
        local_name: &str,
        value: &str,
    ) -> Result<(), BrowserHostError>;
    fn set_character_data(
        &mut self,
        node: BrowserHostNodeToken,
        data: &str,
    ) -> Result<(), BrowserHostError>;
    fn finish_phase(&mut self) -> Result<BrowserHostCommitOutcome, BrowserHostError>;
    fn abort_phase(&mut self);
}

/// Host-side disposition paired with a classic-script or checkpoint result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserHostPhaseOutcome {
    NotStarted,
    Completed(BrowserHostCommitOutcome),
    Discarded,
    Failed(BrowserHostError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserHostClassicExecution {
    pub script: ClassicScriptExecution,
    pub host: BrowserHostPhaseOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserHostMicrotaskExecution {
    pub checkpoint: MicrotaskCheckpointExecution,
    pub host: BrowserHostPhaseOutcome,
}

#[derive(Clone, Copy)]
struct BrowserHostFunctionIds {
    document: RuntimeFunctionId,
    lookup: RuntimeFunctionId,
    create_element: RuntimeFunctionId,
    create_text: RuntimeFunctionId,
    append: RuntimeFunctionId,
    set_attribute: RuntimeFunctionId,
    set_text: RuntimeFunctionId,
}

type NodeNoArgFn = unsafe fn(*mut ()) -> Result<BrowserHostNodeToken, BrowserHostError>;
type LookupFn = unsafe fn(*mut (), u32) -> Result<BrowserHostNodeToken, BrowserHostError>;
type NodeStringFn = unsafe fn(*mut (), &str) -> Result<BrowserHostNodeToken, BrowserHostError>;
type TwoNodeFn =
    unsafe fn(*mut (), BrowserHostNodeToken, BrowserHostNodeToken) -> Result<(), BrowserHostError>;
type AttributeFn =
    unsafe fn(*mut (), BrowserHostNodeToken, &str, &str) -> Result<(), BrowserHostError>;
type NodeTextFn = unsafe fn(*mut (), BrowserHostNodeToken, &str) -> Result<(), BrowserHostError>;
type ValidateFn = unsafe fn(*mut ()) -> Result<(), BrowserHostError>;
type FinishFn = unsafe fn(*mut ()) -> Result<BrowserHostCommitOutcome, BrowserHostError>;
type AbortFn = unsafe fn(*mut ());

#[derive(Clone, Copy)]
struct ActiveBrowserHost {
    data: NonNull<()>,
    document: NodeNoArgFn,
    lookup: LookupFn,
    create_element: NodeStringFn,
    create_text: NodeStringFn,
    append: TwoNodeFn,
    set_attribute: AttributeFn,
    set_text: NodeTextFn,
    validate: ValidateFn,
    finish: FinishFn,
    abort: AbortFn,
}

/// Non-GC scoped host metadata embedded in `ContextCell`. The erased pointer in `active` is never
/// placed in the JavaScript heap and is always cleared by the branded guard before its borrowed
/// task can move or be dropped.
pub(crate) struct BrowserHostContextState {
    active: Option<ActiveBrowserHost>,
    /// Seals erased host dispatch before reconstructing the exclusive task borrow. A host which
    /// synchronously reenters the binding therefore fails before a second `&mut H` can exist.
    dispatch_busy: bool,
    function_ids: Option<BrowserHostFunctionIds>,
    bindings_installed: bool,
}

impl BrowserHostContextState {
    pub(crate) const fn new() -> Self {
        Self {
            active: None,
            dispatch_busy: false,
            function_ids: None,
            bindings_installed: false,
        }
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.active.is_some() || self.dispatch_busy
    }

    pub(crate) fn release_scope_or_abort(&mut self, data: NonNull<()>) {
        if self.dispatch_busy {
            std::process::abort();
        }
        let Some(active) = self.active.take() else {
            std::process::abort();
        };
        if active.data != data {
            std::process::abort();
        }
    }
}

pub(crate) struct BrowserHostScopeGuard<'host, H: BrowserHostTask> {
    raw: Context,
    data: NonNull<()>,
    _borrow: PhantomData<&'host mut H>,
}

impl<'host, H: BrowserHostTask> BrowserHostScopeGuard<'host, H> {
    pub(crate) fn install(mut raw: Context, host: &'host mut H) -> Result<Self, BrowserHostError> {
        raw.assert_owner_execution_live();
        if raw.browser_host.active.is_some() || raw.browser_host.dispatch_busy {
            return Err(BrowserHostError::Internal);
        }

        unsafe fn host_mut<'a, H: BrowserHostTask>(data: *mut ()) -> &'a mut H {
            // SAFETY: `BrowserHostScopeGuard` retains the exclusive source borrow and clears the
            // context slot before that borrow ends. Every call is synchronous on the owner thread,
            // and `BrowserHostDispatchGuard` seals reentry before this function is invoked.
            unsafe { &mut *data.cast::<H>() }
        }

        unsafe fn document<H: BrowserHostTask>(
            data: *mut (),
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            unsafe { host_mut::<H>(data) }.document_node()
        }
        unsafe fn lookup<H: BrowserHostTask>(
            data: *mut (),
            slot: u32,
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            unsafe { host_mut::<H>(data) }.lookup_node(slot)
        }
        unsafe fn create_element<H: BrowserHostTask>(
            data: *mut (),
            value: &str,
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            unsafe { host_mut::<H>(data) }.create_html_element(value)
        }
        unsafe fn create_text<H: BrowserHostTask>(
            data: *mut (),
            value: &str,
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            unsafe { host_mut::<H>(data) }.create_text(value)
        }
        unsafe fn append<H: BrowserHostTask>(
            data: *mut (),
            parent: BrowserHostNodeToken,
            child: BrowserHostNodeToken,
        ) -> Result<(), BrowserHostError> {
            unsafe { host_mut::<H>(data) }.append_child(parent, child)
        }
        unsafe fn set_attribute<H: BrowserHostTask>(
            data: *mut (),
            element: BrowserHostNodeToken,
            name: &str,
            value: &str,
        ) -> Result<(), BrowserHostError> {
            unsafe { host_mut::<H>(data) }.set_html_attribute(element, name, value)
        }
        unsafe fn set_text<H: BrowserHostTask>(
            data: *mut (),
            node: BrowserHostNodeToken,
            value: &str,
        ) -> Result<(), BrowserHostError> {
            unsafe { host_mut::<H>(data) }.set_character_data(node, value)
        }
        unsafe fn finish<H: BrowserHostTask>(
            data: *mut (),
        ) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
            unsafe { host_mut::<H>(data) }.finish_phase()
        }
        unsafe fn validate<H: BrowserHostTask>(data: *mut ()) -> Result<(), BrowserHostError> {
            unsafe { host_mut::<H>(data) }.validate_phase()
        }
        unsafe fn abort<H: BrowserHostTask>(data: *mut ()) {
            unsafe { host_mut::<H>(data) }.abort_phase();
        }

        let data = NonNull::from(host).cast::<()>();
        raw.browser_host.active = Some(ActiveBrowserHost {
            data,
            document: document::<H>,
            lookup: lookup::<H>,
            create_element: create_element::<H>,
            create_text: create_text::<H>,
            append: append::<H>,
            set_attribute: set_attribute::<H>,
            set_text: set_text::<H>,
            validate: validate::<H>,
            finish: finish::<H>,
            abort: abort::<H>,
        });
        Ok(Self { raw, data, _borrow: PhantomData })
    }
}

impl<H: BrowserHostTask> Drop for BrowserHostScopeGuard<'_, H> {
    fn drop(&mut self) {
        self.raw.release_browser_host_scope_for_cleanup(self.data);
    }
}

/// Owner-thread seal around one erased call. It is acquired before the function table can
/// reconstruct `&mut H` and released during ordinary or panic unwind before the host scope drops.
struct BrowserHostDispatchGuard {
    raw: Context,
}

impl BrowserHostDispatchGuard {
    fn enter(mut raw: Context) -> Result<Self, BrowserHostError> {
        if raw.browser_host.active.is_none() || raw.browser_host.dispatch_busy {
            return Err(BrowserHostError::Internal);
        }
        raw.browser_host.dispatch_busy = true;
        Ok(Self { raw })
    }
}

impl Drop for BrowserHostDispatchGuard {
    fn drop(&mut self) {
        if self.raw.browser_host.active.is_none() || !self.raw.browser_host.dispatch_busy {
            std::process::abort();
        }
        self.raw.browser_host.dispatch_busy = false;
    }
}

impl Context {
    fn active_browser_host(self) -> Result<ActiveBrowserHost, BrowserHostError> {
        self.browser_host.active.ok_or(BrowserHostError::Internal)
    }

    fn browser_host_document(self) -> Result<BrowserHostNodeToken, BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        // SAFETY: The branded scope guard owns the erased exclusive borrow for this synchronous
        // call, the dispatch guard rejects synchronous reentry before another exclusive borrow can
        // be reconstructed, and the data identity came from the still-installed context slot.
        unsafe { (active.document)(active.data.as_ptr()) }
    }

    fn browser_host_lookup(self, slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.lookup)(active.data.as_ptr(), slot) }
    }

    fn browser_host_create_element(
        self,
        value: &str,
    ) -> Result<BrowserHostNodeToken, BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.create_element)(active.data.as_ptr(), value) }
    }

    fn browser_host_create_text(
        self,
        value: &str,
    ) -> Result<BrowserHostNodeToken, BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.create_text)(active.data.as_ptr(), value) }
    }

    fn browser_host_append(
        self,
        parent: BrowserHostNodeToken,
        child: BrowserHostNodeToken,
    ) -> Result<(), BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.append)(active.data.as_ptr(), parent, child) }
    }

    fn browser_host_set_attribute(
        self,
        element: BrowserHostNodeToken,
        name: &str,
        value: &str,
    ) -> Result<(), BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.set_attribute)(active.data.as_ptr(), element, name, value) }
    }

    fn browser_host_set_text(
        self,
        node: BrowserHostNodeToken,
        value: &str,
    ) -> Result<(), BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.set_text)(active.data.as_ptr(), node, value) }
    }

    pub(crate) fn finish_browser_host_phase(
        self,
    ) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.finish)(active.data.as_ptr()) }
    }

    pub(crate) fn validate_browser_host_phase(self) -> Result<(), BrowserHostError> {
        let active = self.active_browser_host()?;
        let _dispatch_guard = BrowserHostDispatchGuard::enter(self)?;
        unsafe { (active.validate)(active.data.as_ptr()) }
    }

    pub(crate) fn abort_browser_host_phase(self) {
        let active = self
            .active_browser_host()
            .unwrap_or_else(|_| std::process::abort());
        let _dispatch_guard =
            BrowserHostDispatchGuard::enter(self).unwrap_or_else(|_| std::process::abort());
        unsafe { (active.abort)(active.data.as_ptr()) };
    }
}

#[derive(Clone, Copy)]
pub(crate) enum BrowserHostInstallError {
    #[cfg(feature = "alloc_error")]
    Allocation,
    BindingCollision,
    Internal,
}

pub(crate) fn install_browser_host_bindings(
    mut cx: Context,
) -> Result<(), BrowserHostInstallError> {
    if cx.browser_host.bindings_installed {
        return Ok(());
    }
    if browser_host_binding_exists(cx)? {
        return Err(BrowserHostInstallError::BindingCollision);
    }

    let ids = match cx.browser_host.function_ids {
        Some(ids) => ids,
        None => {
            let mut register = |function| {
                cx.rust_runtime_functions
                    .register(function)
                    .ok_or(BrowserHostInstallError::Internal)
            };
            let ids = BrowserHostFunctionIds {
                document: register(document)?,
                lookup: register(lookup)?,
                create_element: register(create_element)?,
                create_text: register(create_text)?,
                append: register(append)?,
                set_attribute: register(set_attribute)?,
                set_text: register(set_text)?,
            };
            cx.browser_host.function_ids = Some(ids);
            ids
        }
    };

    let result = install_browser_host_bindings_inner(cx, ids);
    match result {
        Ok(()) => {
            cx.browser_host.bindings_installed = true;
            Ok(())
        }
        Err(EvalError::Value(_)) => Err(BrowserHostInstallError::Internal),
        #[cfg(feature = "alloc_error")]
        Err(EvalError::Alloc(_)) => Err(BrowserHostInstallError::Allocation),
    }
}

fn browser_host_binding_exists(mut cx: Context) -> Result<bool, BrowserHostInstallError> {
    let result: EvalResult<u32> = handle_scope!(cx, {
        let realm = cx.initial_realm();
        let global_name =
            PropertyKey::string_handle(cx, cx.alloc_static_string("__wildBuzzardDom")?)?;
        Ok(u32::from(
            realm
                .global_object()
                .as_object()
                .get_own_property(cx, global_name)?
                .is_some(),
        ))
    });
    match result {
        Ok(exists) => Ok(exists != 0),
        // The initial global is not a proxy and own-property lookup does not execute JavaScript.
        Err(EvalError::Value(_)) => Err(BrowserHostInstallError::Internal),
        #[cfg(feature = "alloc_error")]
        Err(EvalError::Alloc(_)) => Err(BrowserHostInstallError::Allocation),
    }
}

fn install_browser_host_bindings_inner(
    mut cx: Context,
    ids: BrowserHostFunctionIds,
) -> EvalResult<()> {
    handle_scope!(cx, {
        let realm = cx.initial_realm();
        let global = realm.global_object().as_object();
        let object_proto = realm.get_intrinsic(Intrinsic::ObjectPrototype);
        let mut object = ObjectBuilder::<ObjectValue>::new(cx)
            .proto(object_proto)
            .build()?
            .to_handle();

        install_method(cx, realm, object, "document", ids.document, 0)?;
        install_method(cx, realm, object, "lookup", ids.lookup, 1)?;
        install_method(cx, realm, object, "createElement", ids.create_element, 1)?;
        install_method(cx, realm, object, "createText", ids.create_text, 1)?;
        install_method(cx, realm, object, "append", ids.append, 2)?;
        install_method(cx, realm, object, "setAttribute", ids.set_attribute, 3)?;
        install_method(cx, realm, object, "setText", ids.set_text, 2)?;
        if !object.prevent_extensions(cx)? {
            // This is a freshly created ordinary object, not a proxy. Refusal would mean the
            // object model violated its non-extensibility contract and continuing is unsafe.
            std::process::abort();
        }

        // Publish last. If any earlier allocation fails, the partial object is unreachable and a
        // later bounded attempt can safely rebuild it using the already registered scalar ids.
        let global_name =
            PropertyKey::string_handle(cx, cx.alloc_static_string("__wildBuzzardDom")?)?;
        define_property_or_throw(
            cx,
            global,
            global_name,
            PropertyDescriptor::frozen(object.as_value()),
        )?;
        Ok(())
    })
}

fn install_method(
    mut cx: Context,
    realm: Handle<crate::runtime::Realm>,
    object: Handle<ObjectValue>,
    name: &'static str,
    id: RuntimeFunctionId,
    length: u32,
) -> EvalResult<()> {
    let name_key = PropertyKey::string_handle(cx, cx.alloc_static_string(name)?)?;
    let function = BuiltinFunction::create_custom(cx, id, length, name_key, realm, None)?;
    define_property_or_throw(cx, object, name_key, PropertyDescriptor::frozen(function.as_value()))
}

fn host_result<T>(cx: Context, result: Result<T, BrowserHostError>) -> EvalResult<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.is_script_exception() => type_error(cx, error.script_message()),
        Err(error) => cx.browser_host_terminate(error),
    }
}

fn token_argument(cx: Context, value: Handle<Value>) -> EvalResult<BrowserHostNodeToken> {
    if !value.is_number() {
        return type_error(cx, "bounded DOM node token must be a number");
    }
    let number = value.as_number();
    if !number.is_finite() || number.fract() != 0.0 || number <= 0.0 {
        return type_error(cx, "bounded DOM node token must be an exact positive integer");
    }
    let integer = number as u64;
    match BrowserHostNodeToken::new(integer).filter(|token| token.get() as f64 == number) {
        Some(token) => Ok(token),
        None => type_error(cx, "bounded DOM node token is outside the exact range"),
    }
}

fn slot_argument(cx: Context, value: Handle<Value>) -> EvalResult<u32> {
    if !value.is_number() {
        return type_error(cx, "bounded DOM node slot must be a number");
    }
    let number = value.as_number();
    if !number.is_finite() || number.fract() != 0.0 || number < 0.0 || number > u32::MAX as f64 {
        return type_error(cx, "bounded DOM node slot is outside the exact u32 range");
    }
    Ok(number as u32)
}

fn string_argument(cx: Context, value: Handle<Value>) -> EvalResult<String> {
    let value = to_string(cx, value)?;
    // Flattening is owned by Brimstone and may move the string. Iterate only after it returns, and
    // drop the raw flat-string iterator before any TypeError allocation can trigger another GC.
    let (utf8_len, has_lone_surrogate) = {
        let code_points = value.iter_code_points()?;
        let mut utf8_len = 0_usize;
        let mut has_lone_surrogate = false;
        for code_point in code_points {
            let Some(character) = char::from_u32(code_point) else {
                has_lone_surrogate = true;
                break;
            };
            utf8_len = utf8_len
                .checked_add(character.len_utf8())
                .unwrap_or_else(|| cx.browser_host_terminate(BrowserHostError::Allocation));
        }
        (utf8_len, has_lone_surrogate)
    };
    if has_lone_surrogate {
        return type_error(cx, "bounded DOM strings do not yet support lone surrogates");
    }

    let mut result = String::new();
    if result.try_reserve_exact(utf8_len).is_err() {
        cx.browser_host_terminate(BrowserHostError::Allocation);
    }
    for code_point in value.iter_code_points()? {
        let character = char::from_u32(code_point)
            .unwrap_or_else(|| cx.browser_host_terminate(BrowserHostError::Internal));
        result.push(character);
    }
    if result.len() != utf8_len {
        cx.browser_host_terminate(BrowserHostError::Internal);
    }
    Ok(result)
}

runtime_fn! {
fn document(cx, _, _) {
    cx.browser_script_poll_phase();
    let token = host_result(cx, cx.browser_host_document())?;
    cx.browser_script_poll_phase();
    Ok(cx.number(token.get() as f64))
}}

runtime_fn! {
fn lookup(cx, _, arguments) {
    cx.browser_script_poll_phase();
    let slot = slot_argument(cx, arguments.get(cx, 0))?;
    cx.browser_script_poll_phase();
    let token = host_result(cx, cx.browser_host_lookup(slot))?;
    cx.browser_script_poll_phase();
    Ok(cx.number(token.get() as f64))
}}

runtime_fn! {
fn create_element(cx, _, arguments) {
    cx.browser_script_poll_phase();
    let name = string_argument(cx, arguments.get(cx, 0))?;
    cx.browser_script_poll_phase();
    let token = host_result(cx, cx.browser_host_create_element(&name))?;
    cx.browser_script_poll_phase();
    Ok(cx.number(token.get() as f64))
}}

runtime_fn! {
fn create_text(cx, _, arguments) {
    cx.browser_script_poll_phase();
    let data = string_argument(cx, arguments.get(cx, 0))?;
    cx.browser_script_poll_phase();
    let token = host_result(cx, cx.browser_host_create_text(&data))?;
    cx.browser_script_poll_phase();
    Ok(cx.number(token.get() as f64))
}}

runtime_fn! {
fn append(cx, _, arguments) {
    cx.browser_script_poll_phase();
    let parent = token_argument(cx, arguments.get(cx, 0))?;
    let child = token_argument(cx, arguments.get(cx, 1))?;
    cx.browser_script_poll_phase();
    host_result(cx, cx.browser_host_append(parent, child))?;
    cx.browser_script_poll_phase();
    Ok(cx.undefined())
}}

runtime_fn! {
fn set_attribute(cx, _, arguments) {
    cx.browser_script_poll_phase();
    let element = token_argument(cx, arguments.get(cx, 0))?;
    let name = string_argument(cx, arguments.get(cx, 1))?;
    let value = string_argument(cx, arguments.get(cx, 2))?;
    cx.browser_script_poll_phase();
    host_result(cx, cx.browser_host_set_attribute(element, &name, &value))?;
    cx.browser_script_poll_phase();
    Ok(cx.undefined())
}}

runtime_fn! {
fn set_text(cx, _, arguments) {
    cx.browser_script_poll_phase();
    let node = token_argument(cx, arguments.get(cx, 0))?;
    let value = string_argument(cx, arguments.get(cx, 1))?;
    cx.browser_script_poll_phase();
    host_result(cx, cx.browser_host_set_text(node, &value))?;
    cx.browser_script_poll_phase();
    Ok(cx.undefined())
}}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        env, fs,
        os::unix::process::ExitStatusExt,
        panic::{AssertUnwindSafe, catch_unwind},
        process::{Command, Stdio},
        rc::Rc,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use crate::{
        common::options::OptionsBuilder,
        runtime::{
            BrowserScriptRealm, ClassicScriptLimits, ClassicScriptOutcome, ClassicScriptRequest,
            Context, ContextBuilder, InterruptReason, MicrotaskCheckpointOutcome, OwnedContext,
            ScriptInterruptHandle, ScriptValueSummary,
        },
    };

    use super::*;

    struct MockHost {
        next_token: u64,
        revision: u64,
        phase_before: u64,
        phase_commands: u32,
        phase_created: u32,
        finishes: u32,
        calls: Vec<&'static str>,
        aborts: u32,
        create_error: Option<BrowserHostError>,
        panic_on_document: bool,
        panic_on_abort: bool,
        finish_interrupt: Option<(u32, ScriptInterruptHandle)>,
        finish_delay: Option<(u32, Duration)>,
        abort_interrupt: Option<ScriptInterruptHandle>,
        abort_delay: Option<Duration>,
        last_text: Option<String>,
        observed_calls: Rc<RefCell<Vec<&'static str>>>,
    }

    impl MockHost {
        fn new() -> Self {
            Self {
                next_token: 1,
                revision: 0,
                phase_before: 0,
                phase_commands: 0,
                phase_created: 0,
                finishes: 0,
                calls: Vec::new(),
                aborts: 0,
                create_error: None,
                panic_on_document: false,
                panic_on_abort: false,
                finish_interrupt: None,
                finish_delay: None,
                abort_interrupt: None,
                abort_delay: None,
                last_text: None,
                observed_calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn record(&mut self, call: &'static str) {
            self.calls.push(call);
            self.observed_calls.borrow_mut().push(call);
        }

        fn token(&mut self) -> BrowserHostNodeToken {
            let token = BrowserHostNodeToken::new(self.next_token).unwrap();
            self.next_token += 1;
            token
        }

        fn mutation(&mut self, call: &'static str, created: bool) {
            self.record(call);
            self.revision += 1;
            self.phase_commands += 1;
            self.phase_created += u32::from(created);
        }
    }

    impl BrowserHostTask for MockHost {
        fn validate_phase(&mut self) -> Result<(), BrowserHostError> {
            Ok(())
        }

        fn document_node(&mut self) -> Result<BrowserHostNodeToken, BrowserHostError> {
            if self.panic_on_document {
                panic!("injected DOM host panic");
            }
            self.record("document");
            Ok(self.token())
        }

        fn lookup_node(&mut self, _slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError> {
            self.record("lookup");
            Ok(self.token())
        }

        fn create_html_element(
            &mut self,
            _local_name: &str,
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            if let Some(error) = self.create_error {
                return Err(error);
            }
            self.mutation("create_element", true);
            Ok(self.token())
        }

        fn create_text(&mut self, data: &str) -> Result<BrowserHostNodeToken, BrowserHostError> {
            self.last_text = Some(data.to_owned());
            self.mutation("create_text", true);
            Ok(self.token())
        }

        fn append_child(
            &mut self,
            _parent: BrowserHostNodeToken,
            _child: BrowserHostNodeToken,
        ) -> Result<(), BrowserHostError> {
            self.mutation("append", false);
            Ok(())
        }

        fn set_html_attribute(
            &mut self,
            _element: BrowserHostNodeToken,
            _local_name: &str,
            _value: &str,
        ) -> Result<(), BrowserHostError> {
            self.mutation("set_attribute", false);
            Ok(())
        }

        fn set_character_data(
            &mut self,
            _node: BrowserHostNodeToken,
            data: &str,
        ) -> Result<(), BrowserHostError> {
            self.last_text = Some(data.to_owned());
            self.mutation("set_text", false);
            Ok(())
        }

        fn finish_phase(&mut self) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
            self.record("finish");
            let before = BrowserHostDocumentVersion::new(7, self.phase_before);
            let after = BrowserHostDocumentVersion::new(7, self.revision);
            let outcome = if self.phase_commands == 0 {
                BrowserHostCommitOutcome::NoChanges(after)
            } else {
                BrowserHostCommitOutcome::Committed(BrowserHostPhaseCommit::new(
                    before,
                    after,
                    self.phase_commands,
                    self.phase_created,
                ))
            };
            self.phase_before = self.revision;
            self.phase_commands = 0;
            self.phase_created = 0;
            self.finishes += 1;
            if let Some((target, interrupt)) = &self.finish_interrupt
                && *target == self.finishes
            {
                interrupt.request_interrupt();
            }
            if let Some((target, delay)) = self.finish_delay
                && target == self.finishes
            {
                thread::sleep(delay);
            }
            Ok(outcome)
        }

        fn abort_phase(&mut self) {
            if self.panic_on_abort {
                panic!("injected abort panic before host retirement");
            }
            self.aborts += 1;
            if let Some(interrupt) = &self.abort_interrupt {
                interrupt.request_interrupt();
            }
            if let Some(delay) = self.abort_delay {
                thread::sleep(delay);
            }
        }
    }

    struct ReentrantHost {
        raw: Context,
        nested_error: Option<BrowserHostError>,
        aborts: u32,
    }

    impl BrowserHostTask for ReentrantHost {
        fn validate_phase(&mut self) -> Result<(), BrowserHostError> {
            Ok(())
        }

        fn document_node(&mut self) -> Result<BrowserHostNodeToken, BrowserHostError> {
            match self.raw.browser_host_document() {
                Ok(token) => Ok(token),
                Err(error) => {
                    self.nested_error = Some(error);
                    Err(error)
                }
            }
        }

        fn lookup_node(&mut self, _slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn create_html_element(
            &mut self,
            _local_name: &str,
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn create_text(&mut self, _data: &str) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn append_child(
            &mut self,
            _parent: BrowserHostNodeToken,
            _child: BrowserHostNodeToken,
        ) -> Result<(), BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn set_html_attribute(
            &mut self,
            _element: BrowserHostNodeToken,
            _local_name: &str,
            _value: &str,
        ) -> Result<(), BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn set_character_data(
            &mut self,
            _node: BrowserHostNodeToken,
            _data: &str,
        ) -> Result<(), BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn finish_phase(&mut self) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
            Err(BrowserHostError::Internal)
        }

        fn abort_phase(&mut self) {
            self.aborts += 1;
        }
    }

    fn context() -> OwnedContext {
        let options = OptionsBuilder::new().serialized_heap(None).build().unwrap();
        ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .unwrap()
    }

    fn host_run(
        realm: &mut BrowserScriptRealm<'_>,
        host: &mut impl BrowserHostTask,
        source: &str,
    ) -> BrowserHostClassicExecution {
        realm.execute_classic_with_host(
            host,
            ClassicScriptRequest::new(source, "https://example.test/host.js"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        )
    }

    #[test]
    fn script_throw_is_observed_before_host_aware_checkpoint() {
        let mut cx = context();
        let mut host = MockHost::new();
        cx.with_browser_script_realm(|realm| {
            let script = host_run(
                realm,
                &mut host,
                "const dom = __wildBuzzardDom;\n\
                 globalThis.hostDocument = dom.document();\n\
                 globalThis.hostElement = dom.createElement('section');\n\
                 dom.append(hostDocument, hostElement);\n\
                 Promise.resolve().then(() => {\n\
                   dom.setAttribute(hostElement, 'data-phase', 'microtask');\n\
                 });\n\
                 throw 19;",
            );
            assert_eq!(
                script.script.outcome,
                ClassicScriptOutcome::Thrown(ScriptValueSummary::Number(19.0))
            );
            assert!(matches!(
                script.host,
                BrowserHostPhaseOutcome::Completed(BrowserHostCommitOutcome::Committed(_))
            ));
            assert_eq!(host.calls, vec!["document", "create_element", "append", "finish"]);
            assert!(script.script.report.pending_jobs_at_exit() >= 1);

            // The embedding can report the primary exception here. No promise DOM operation has
            // run until it explicitly enters the checkpoint below.
            let checkpoint = realm.perform_microtask_checkpoint_with_host(
                &mut host,
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(checkpoint.checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
            assert!(matches!(
                checkpoint.host,
                BrowserHostPhaseOutcome::Completed(BrowserHostCommitOutcome::Committed(_))
            ));
            assert_eq!(
                host.calls,
                vec![
                    "document",
                    "create_element",
                    "append",
                    "finish",
                    "set_attribute",
                    "finish"
                ]
            );
        });
    }

    #[test]
    fn document_budget_preserves_pre_primary_post_order_across_host_scripts() {
        let mut cx = context();
        let mut host = MockHost::new();
        let observed_calls = host.observed_calls.clone();
        let interrupt = ScriptInterruptHandle::new();
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        cx.with_browser_script_realm(|realm| {
            realm
                .with_hosted_document_script_budget(&mut host, limits, &interrupt, |realm| {
                    let pre = realm.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(pre.checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert_eq!(&*observed_calls.borrow(), &["finish"]);

                    let first = realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                        "const dom = __wildBuzzardDom;\n\
                             globalThis.documentToken = dom.document();\n\
                             globalThis.documentElement = dom.createElement('section');\n\
                             dom.append(documentToken, documentElement);\n\
                             globalThis.documentPhase = 'primary';\n\
                             Promise.resolve().then(() => {\n\
                               documentPhase = 'post';\n\
                               dom.setAttribute(documentElement, 'data-phase', documentPhase);\n\
                             });\n\
                             throw 29;",
                        "inline-host-1.js",
                    ));
                    assert_eq!(
                        first.script.outcome,
                        ClassicScriptOutcome::Thrown(ScriptValueSummary::Number(29.0))
                    );
                    assert_eq!(
                        &*observed_calls.borrow(),
                        &["finish", "document", "create_element", "append", "finish"]
                    );

                    // Primary error reporting belongs here. The promise host mutation is still
                    // absent until the explicit post-script checkpoint.
                    let post = realm.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(post.checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert_eq!(
                        &*observed_calls.borrow(),
                        &[
                            "finish",
                            "document",
                            "create_element",
                            "append",
                            "finish",
                            "set_attribute",
                            "finish",
                        ]
                    );

                    let pre_second = realm.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(pre_second.checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
                    let second = realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                        "if (documentPhase !== 'post') throw 'post checkpoint missing';\n\
                             __wildBuzzardDom.setAttribute(\n\
                               documentElement, 'data-second', 'visible');",
                        "inline-host-2.js",
                    ));
                    assert!(matches!(second.script.outcome, ClassicScriptOutcome::Success(_)));
                    assert_eq!(realm.document_script_candidates(), Some(2));
                    assert_eq!(first.script.report.jit_native_entries, 0);
                    assert_eq!(second.script.report.jit_native_entries, 0);
                })
                .unwrap();
        });
    }

    #[test]
    fn hosted_document_session_rejects_replacement_host_before_job_dequeue() {
        let mut cx = context();
        let mut host_a = MockHost::new();
        let host_a_calls = host_a.observed_calls.clone();
        let mut host_b = MockHost::new();
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();

        cx.with_browser_script_realm(|realm| {
            realm
                .with_hosted_document_script_budget(
                    &mut host_a,
                    limits,
                    &ScriptInterruptHandle::new(),
                    |realm| {
                        let script =
                            realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                                "const dom = __wildBuzzardDom;\n\
                                 globalThis.hostOwnerNode = dom.createText('host-a-primary');\n\
                                 Promise.resolve().then(() => {\n\
                                   dom.setText(hostOwnerNode, 'host-a-job');\n\
                                 });",
                                "host-owner.js",
                            ));
                        assert!(matches!(script.script.outcome, ClassicScriptOutcome::Success(_)));
                        assert!(script.script.report.pending_jobs_at_exit() >= 1);

                        let host_free = realm.perform_document_microtask_checkpoint();
                        assert_eq!(host_free.outcome, MicrotaskCheckpointOutcome::RuntimeBusy);

                        let replacement = realm.perform_microtask_checkpoint_with_host(
                            &mut host_b,
                            ClassicScriptLimits::default(),
                            &ScriptInterruptHandle::new(),
                        );
                        assert_eq!(
                            replacement.checkpoint.outcome,
                            MicrotaskCheckpointOutcome::RuntimeBusy
                        );
                        assert_eq!(replacement.host, BrowserHostPhaseOutcome::NotStarted);
                        assert!(host_b.calls.is_empty());
                        assert_eq!(host_b.aborts, 0);

                        let checkpoint = realm.perform_hosted_document_microtask_checkpoint();
                        assert_eq!(
                            checkpoint.checkpoint.outcome,
                            MicrotaskCheckpointOutcome::Complete
                        );
                        assert!(matches!(checkpoint.host, BrowserHostPhaseOutcome::Completed(_)));
                        assert_eq!(
                            &*host_a_calls.borrow(),
                            &["create_text", "finish", "set_text", "finish"]
                        );
                    },
                )
                .unwrap();
        });

        assert_eq!(host_a.last_text.as_deref(), Some("host-a-job"));
        assert!(host_b.calls.is_empty());
        assert_eq!(host_b.aborts, 0);
    }

    fn disposition_document_limits(wall_time: Duration) -> ClassicScriptLimits {
        ClassicScriptLimits::new(200_000, 8 * 1024 * 1024, 64, 16, wall_time).unwrap()
    }

    fn run_classic_finish_control(deadline: bool) {
        let mut cx = context();
        let mut host = MockHost::new();
        let interrupt = ScriptInterruptHandle::new();
        let wall_time = if deadline {
            host.finish_delay = Some((1, Duration::from_millis(150)));
            Duration::from_millis(100)
        } else {
            host.finish_interrupt = Some((1, interrupt.clone()));
            Duration::from_secs(1)
        };

        cx.with_browser_script_realm(|realm| {
            let result = realm.with_hosted_document_script_budget(
                &mut host,
                disposition_document_limits(wall_time),
                &interrupt,
                |document| {
                    let execution = document.execute_hosted_document_classic(
                        ClassicScriptRequest::new("1 + 1;", "finish-control-classic.js"),
                    );
                    assert!(matches!(execution.script.outcome, ClassicScriptOutcome::Success(_)));
                    assert!(matches!(execution.host, BrowserHostPhaseOutcome::Completed(_)));
                },
            );
            assert_eq!(
                result,
                Err(ClassicScriptOutcome::Interrupted(if deadline {
                    InterruptReason::Deadline
                } else {
                    InterruptReason::ExternalRequest
                }))
            );
            assert_eq!(host.finishes, 1);
            assert_eq!(host.aborts, 0, "completed phase must never be aborted later");

            let recovered = host_run(realm, &mut host, "if (6 * 7 !== 42) throw 'recovery';");
            assert!(matches!(recovered.script.outcome, ClassicScriptOutcome::Success(_)));
            assert!(matches!(recovered.host, BrowserHostPhaseOutcome::Completed(_)));
            assert_eq!(host.aborts, 0);
        });
    }

    #[test]
    fn classic_finish_disposition_survives_callback_cancellation_and_deadline() {
        run_classic_finish_control(false);
        run_classic_finish_control(true);
    }

    fn run_checkpoint_finish_control(deadline: bool) {
        let mut cx = context();
        let mut host = MockHost::new();
        let interrupt = ScriptInterruptHandle::new();
        let wall_time = if deadline {
            host.finish_delay = Some((2, Duration::from_millis(150)));
            Duration::from_millis(100)
        } else {
            host.finish_interrupt = Some((2, interrupt.clone()));
            Duration::from_secs(1)
        };

        cx.with_browser_script_realm(|realm| {
            let result = realm.with_hosted_document_script_budget(
                &mut host,
                disposition_document_limits(wall_time),
                &interrupt,
                |document| {
                    let script =
                        document.execute_hosted_document_classic(ClassicScriptRequest::new(
                            "Promise.resolve().then(() => 1);",
                            "finish-control-checkpoint-setup.js",
                        ));
                    assert!(matches!(script.host, BrowserHostPhaseOutcome::Completed(_)));
                    let checkpoint = document.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(checkpoint.checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert!(matches!(checkpoint.host, BrowserHostPhaseOutcome::Completed(_)));
                },
            );
            assert_eq!(
                result,
                Err(ClassicScriptOutcome::Interrupted(if deadline {
                    InterruptReason::Deadline
                } else {
                    InterruptReason::ExternalRequest
                }))
            );
            assert_eq!(host.finishes, 2);
            assert_eq!(host.aborts, 0);

            let recovered = host_run(realm, &mut host, "40 + 2;");
            assert!(matches!(recovered.host, BrowserHostPhaseOutcome::Completed(_)));
            assert_eq!(host.aborts, 0);
        });
    }

    #[test]
    fn checkpoint_finish_disposition_survives_callback_cancellation_and_deadline() {
        run_checkpoint_finish_control(false);
        run_checkpoint_finish_control(true);
    }

    fn run_classic_abort_control(deadline: bool) {
        let mut cx = context();
        let mut host = MockHost::new();
        host.create_error = Some(BrowserHostError::Allocation);
        let interrupt = ScriptInterruptHandle::new();
        let wall_time = if deadline {
            host.abort_delay = Some(Duration::from_millis(150));
            Duration::from_millis(100)
        } else {
            host.abort_interrupt = Some(interrupt.clone());
            Duration::from_secs(1)
        };

        cx.with_browser_script_realm(|realm| {
            let result = realm.with_hosted_document_script_budget(
                &mut host,
                disposition_document_limits(wall_time),
                &interrupt,
                |document| {
                    let execution =
                        document.execute_hosted_document_classic(ClassicScriptRequest::new(
                            "__wildBuzzardDom.createElement('div');",
                            "abort-control-classic.js",
                        ));
                    assert_eq!(
                        execution.script.outcome,
                        ClassicScriptOutcome::HostFailure(BrowserHostError::Allocation)
                    );
                    assert_eq!(execution.host, BrowserHostPhaseOutcome::Discarded);
                },
            );
            assert_eq!(
                result,
                Err(ClassicScriptOutcome::HostFailure(BrowserHostError::Allocation))
            );
            assert_eq!(host.aborts, 1, "abort disposition must execute exactly once");
            assert_eq!(host.finishes, 0);
            if !deadline {
                assert!(interrupt.is_interrupt_requested());
            }

            host.create_error = None;
            let recovered = host_run(realm, &mut host, "21 * 2;");
            assert!(matches!(recovered.host, BrowserHostPhaseOutcome::Completed(_)));
            assert_eq!(host.aborts, 1);
        });
    }

    #[test]
    fn classic_abort_disposition_survives_callback_cancellation_and_deadline() {
        run_classic_abort_control(false);
        run_classic_abort_control(true);
    }

    fn run_checkpoint_abort_control(deadline: bool) {
        let mut cx = context();
        let mut host = MockHost::new();
        host.create_error = Some(BrowserHostError::Allocation);
        let interrupt = ScriptInterruptHandle::new();
        let wall_time = if deadline {
            host.abort_delay = Some(Duration::from_millis(150));
            Duration::from_millis(100)
        } else {
            host.abort_interrupt = Some(interrupt.clone());
            Duration::from_secs(1)
        };

        cx.with_browser_script_realm(|realm| {
            let result = realm.with_hosted_document_script_budget(
                &mut host,
                disposition_document_limits(wall_time),
                &interrupt,
                |document| {
                    let script =
                        document.execute_hosted_document_classic(ClassicScriptRequest::new(
                            "Promise.resolve().then(() => __wildBuzzardDom.createElement('div'));",
                            "abort-control-checkpoint-setup.js",
                        ));
                    assert!(matches!(script.host, BrowserHostPhaseOutcome::Completed(_)));
                    let checkpoint = document.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(
                        checkpoint.checkpoint.outcome,
                        MicrotaskCheckpointOutcome::HostFailure(BrowserHostError::Allocation)
                    );
                    assert_eq!(checkpoint.host, BrowserHostPhaseOutcome::Discarded);
                },
            );
            assert_eq!(
                result,
                Err(ClassicScriptOutcome::HostFailure(BrowserHostError::Allocation))
            );
            assert_eq!(host.finishes, 1);
            assert_eq!(host.aborts, 1);
            if !deadline {
                assert!(interrupt.is_interrupt_requested());
            }

            host.create_error = None;
            let recovered = host_run(realm, &mut host, "84 / 2;");
            assert!(matches!(recovered.host, BrowserHostPhaseOutcome::Completed(_)));
            assert_eq!(host.aborts, 1);
        });
    }

    #[test]
    fn checkpoint_abort_disposition_survives_callback_cancellation_and_deadline() {
        run_checkpoint_abort_control(false);
        run_checkpoint_abort_control(true);
    }

    #[test]
    fn fatal_host_failure_discards_jobs_without_poisoning_reusable_runtime() {
        let mut cx = context();
        let mut failed_host = MockHost::new();
        failed_host.create_error = Some(BrowserHostError::Allocation);
        cx.with_browser_script_realm(|realm| {
            let failed = host_run(
                realm,
                &mut failed_host,
                "Promise.resolve().then(() => { globalThis.mustNotRun = true; });\n\
                 __wildBuzzardDom.createElement('div');",
            );
            assert_eq!(
                failed.script.outcome,
                ClassicScriptOutcome::HostFailure(BrowserHostError::Allocation)
            );
            assert_eq!(failed.host, BrowserHostPhaseOutcome::Discarded);
            assert_eq!(failed_host.aborts, 1);
            assert_eq!(failed.script.report.pending_jobs_at_exit(), 0);

            let mut fresh_host = MockHost::new();
            let recovered = host_run(
                realm,
                &mut fresh_host,
                "if (6 * 7 !== 42) throw 'runtime did not recover';",
            );
            assert_eq!(
                recovered.script.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn host_string_conversion_preserves_pairs_and_rejects_lone_surrogates_before_dispatch() {
        let mut cx = context();
        let mut host = MockHost::new();
        cx.with_browser_script_realm(|realm| {
            let pair = host_run(realm, &mut host, "__wildBuzzardDom.createText('\\uD83D\\uDE00');");
            assert_eq!(
                pair.script.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            assert_eq!(host.last_text.as_deref(), Some("😀"));
            assert_eq!(
                host.calls,
                vec!["create_text", "finish"],
                "the paired value must reach the host exactly once"
            );

            let calls_before = host.calls.len();
            let lone = host_run(realm, &mut host, "__wildBuzzardDom.createText('\\uD800');");
            assert!(matches!(lone.script.outcome, ClassicScriptOutcome::Thrown(_)));
            assert_eq!(
                &host.calls[calls_before..],
                &["finish"],
                "invalid UTF-16 must throw before host dispatch"
            );
        });
    }

    #[test]
    fn preexisting_reserved_global_fails_closed_without_overwriting_page_state() {
        let mut cx = context();
        let mut host = MockHost::new();
        cx.with_browser_script_realm(|realm| {
            let setup = realm.execute_classic(
                ClassicScriptRequest::new(
                    "globalThis.__wildBuzzardDom = 17;",
                    "binding-collision-setup.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(setup.outcome, ClassicScriptOutcome::Success(ScriptValueSummary::Undefined));

            let failed = host_run(realm, &mut host, "throw 'must not execute';");
            assert_eq!(
                failed.script.outcome,
                ClassicScriptOutcome::HostFailure(BrowserHostError::BindingCollision)
            );
            assert_eq!(
                failed.host,
                BrowserHostPhaseOutcome::Failed(BrowserHostError::BindingCollision)
            );
            assert_eq!(host.aborts, 1);
            assert!(host.calls.is_empty());

            let preserved = realm.execute_classic(
                ClassicScriptRequest::new(
                    "if (globalThis.__wildBuzzardDom !== 17) throw 'binding overwritten';",
                    "binding-collision-check.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                preserved.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn checkpoint_binding_failure_discards_preexisting_jobs_and_retires_host() {
        let mut cx = context();
        let mut host = MockHost::new();
        cx.with_browser_script_realm(|realm| {
            let setup = realm.execute_classic(
                ClassicScriptRequest::new(
                    "globalThis.__wildBuzzardDom = 17;\n\
                     Promise.resolve().then(() => { globalThis.mustNotRun = true; });",
                    "checkpoint-collision-setup.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(setup.outcome, ClassicScriptOutcome::Success(ScriptValueSummary::Undefined));
            assert!(setup.report.pending_jobs_at_exit() >= 1);

            let failed = realm.perform_microtask_checkpoint_with_host(
                &mut host,
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                failed.checkpoint.outcome,
                MicrotaskCheckpointOutcome::HostFailure(BrowserHostError::BindingCollision)
            );
            assert_eq!(
                failed.host,
                BrowserHostPhaseOutcome::Failed(BrowserHostError::BindingCollision)
            );
            assert_eq!(failed.checkpoint.report.pending_jobs_at_exit(), 0);
            assert_eq!(host.aborts, 1);

            let discarded = realm.execute_classic(
                ClassicScriptRequest::new(
                    "if ('mustNotRun' in globalThis) throw 'discarded job ran';",
                    "checkpoint-collision-check.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                discarded.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn synchronous_host_reentry_fails_before_reconstructing_a_second_exclusive_borrow() {
        let mut cx = context();
        // SAFETY: The test keeps this legacy token on the owner thread, uses it only for the
        // serialized adversarial callback below, and drops the host before the context owner.
        let raw = unsafe { cx.raw_context_unchecked() };
        let mut host = ReentrantHost { raw, nested_error: None, aborts: 0 };

        cx.with_browser_script_realm(|realm| {
            let failed = host_run(realm, &mut host, "__wildBuzzardDom.document();");
            assert_eq!(
                failed.script.outcome,
                ClassicScriptOutcome::HostFailure(BrowserHostError::Internal)
            );
            assert_eq!(failed.host, BrowserHostPhaseOutcome::Discarded);
            assert_eq!(host.nested_error, Some(BrowserHostError::Internal));
            assert_eq!(host.aborts, 1);

            let recovered = realm.execute_classic(
                ClassicScriptRequest::new(
                    "if (21 * 2 !== 42) throw 'runtime did not recover';",
                    "after-reentry.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                recovered.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn unexpected_host_panic_clears_scope_and_permanently_poisons_browser_admission() {
        let mut cx = context();
        // SAFETY: Saved only to probe safe legacy-token methods after owner poison.
        let mut raw = unsafe { cx.raw_context_unchecked() };
        let mut host = MockHost::new();
        host.panic_on_document = true;
        cx.with_browser_script_realm(|realm| {
            let failed = host_run(
                realm,
                &mut host,
                "Promise.resolve().then(() => __wildBuzzardDom.document());\n\
                 __wildBuzzardDom.document();",
            );
            assert_eq!(failed.script.outcome, ClassicScriptOutcome::EnginePanic);
            assert_eq!(failed.host, BrowserHostPhaseOutcome::Discarded);
            assert_eq!(host.aborts, 1);

            let rejected = realm.execute_classic(
                ClassicScriptRequest::new("throw 'must not execute';", "poisoned.js"),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(rejected.outcome, ClassicScriptOutcome::RuntimePoisoned);
            assert_eq!(rejected.report.opcodes_executed(), 0);
            assert_eq!(rejected.report.managed_allocation_bytes(), 0);
        });

        let diagnostics = cx.poisoned_owner_diagnostics();
        assert!(diagnostics.0);
        assert!(diagnostics.1 >= 1, "queued host job must remain sealed after engine panic");
        let calls_before = host.calls.len();
        let aborts_before = host.aborts;
        let callback_ran = std::cell::Cell::new(false);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                cx.with_browser_script_realm(|_| callback_ran.set(true));
            }))
            .is_err()
        );
        assert!(!callback_ran.get());
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.run_all_tasks();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.browser_host_document();
            }))
            .is_err()
        );
        assert_eq!(host.calls.len(), calls_before);
        assert_eq!(host.aborts, aborts_before);
        drop(cx);
    }

    #[test]
    fn abort_phase_panic_aborts_process_before_host_or_runtime_reuse() {
        const CHILD_ENV: &str = "W9_A2V_ABORT_PHASE_PANIC_CHILD";
        const MARKER_ENV: &str = "W9_A2V_ABORT_PHASE_PANIC_MARKER";
        const TEST_NAME: &str = "runtime::browser_host::tests::abort_phase_panic_aborts_process_before_host_or_runtime_reuse";

        if env::var_os(CHILD_ENV).is_some() {
            let marker = env::var_os(MARKER_ENV).expect("parent supplied marker path");
            let limits =
                ClassicScriptLimits::new(100_000, 8 * 1024 * 1024, 64, 4, Duration::from_secs(2))
                    .unwrap();
            let mut cx = context();
            let mut host = MockHost::new();
            host.panic_on_document = true;
            host.panic_on_abort = true;
            cx.with_browser_script_realm(|realm| {
                let _ = realm.with_hosted_document_script_budget(
                    &mut host,
                    limits,
                    &ScriptInterruptHandle::new(),
                    |document| {
                        let _ =
                            document.execute_hosted_document_classic(ClassicScriptRequest::new(
                                "Promise.resolve().then(() => 1);\n\
                                 __wildBuzzardDom.document();",
                                "abort-panic-child.js",
                            ));
                    },
                );
            });

            // Unreachable if abort retirement is fail-closed. Deliberately attempt both same-host
            // and fresh-owner reuse before publishing the marker so a returned path fails proof.
            let mut fresh = context();
            fresh.with_browser_script_realm(|realm| {
                let _ = host_run(realm, &mut host, "__wildBuzzardDom.document();");
            });
            fs::write(marker, b"host or runtime reused after abort panic").unwrap();
            panic!("abort_phase panic returned to safe Rust");
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = env::temp_dir()
            .join(format!("w9-a2v-abort-phase-{}-{nonce}.marker", std::process::id()));
        assert!(!marker.exists());
        let test_binary = env::current_exe().unwrap();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("ulimit -c 0; exec \"$1\" --exact \"$2\" --nocapture")
            .arg("w9-a2v-abort-child")
            .arg(test_binary)
            .arg(TEST_NAME)
            .env(CHILD_ENV, "1")
            .env(MARKER_ENV, &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("abort regression child exceeded ten-second bound");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(6), "child must terminate with SIGABRT");
        assert!(!marker.exists(), "no post-abort host/runtime action may execute");
    }

    #[test]
    fn pre_requested_interrupt_wins_before_binding_install_and_directly_retires_host() {
        let mut cx = context();
        // SAFETY: Serialized owner-thread inspection only; no handles are derived.
        let raw = unsafe { cx.raw_context_unchecked() };
        let mut host = MockHost::new();
        let interrupt = ScriptInterruptHandle::new();
        interrupt.request_interrupt();
        cx.with_browser_script_realm(|realm| {
            let setup = realm.execute_classic(
                ClassicScriptRequest::new(
                    "globalThis.__wildBuzzardDom = 17;",
                    "cancelled-setup.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert!(matches!(setup.outcome, ClassicScriptOutcome::Success(_)));
            let execution = realm.execute_classic_with_host(
                &mut host,
                ClassicScriptRequest::new("__wildBuzzardDom.document();", "cancelled.js"),
                ClassicScriptLimits::new(10_000, 4 * 1024 * 1024, 32, 32, Duration::from_secs(1))
                    .unwrap(),
                &interrupt,
            );
            assert!(matches!(execution.script.outcome, ClassicScriptOutcome::Interrupted(_)));
            assert_eq!(execution.host, BrowserHostPhaseOutcome::Discarded);
            assert!(host.calls.is_empty());
            assert_eq!(host.aborts, 1);
            assert!(raw.browser_host.function_ids.is_none());
            assert!(!raw.browser_host.bindings_installed);

            let preserved = realm.execute_classic(
                ClassicScriptRequest::new(
                    "if (globalThis.__wildBuzzardDom !== 17) throw 'binding changed';",
                    "cancelled-check.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert!(matches!(preserved.outcome, ClassicScriptOutcome::Success(_)));
        });
    }

    #[test]
    fn pre_requested_checkpoint_interrupt_discards_jobs_without_starting_host() {
        let mut cx = context();
        // SAFETY: Serialized owner-thread inspection only; no handles are derived.
        let raw = unsafe { cx.raw_context_unchecked() };
        let mut host = MockHost::new();
        cx.with_browser_script_realm(|realm| {
            let setup = realm.execute_classic(
                ClassicScriptRequest::new(
                    "globalThis.__wildBuzzardDom = 17;\n\
                     Promise.resolve().then(() => { globalThis.mustNotRun = true; });",
                    "cancelled-checkpoint-setup.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert!(setup.report.pending_jobs_at_exit() >= 1);

            let interrupt = ScriptInterruptHandle::new();
            interrupt.request_interrupt();
            let execution = realm.perform_microtask_checkpoint_with_host(
                &mut host,
                ClassicScriptLimits::default(),
                &interrupt,
            );
            assert!(matches!(
                execution.checkpoint.outcome,
                MicrotaskCheckpointOutcome::Interrupted(_)
            ));
            assert_eq!(execution.host, BrowserHostPhaseOutcome::Discarded);
            assert_eq!(execution.checkpoint.report.pending_jobs_at_exit(), 0);
            assert!(host.calls.is_empty());
            assert_eq!(host.aborts, 1);
            assert!(raw.browser_host.function_ids.is_none());

            let discarded = realm.execute_classic(
                ClassicScriptRequest::new(
                    "if ('mustNotRun' in globalThis) throw 'cancelled job ran';\n\
                     if (globalThis.__wildBuzzardDom !== 17) throw 'binding changed';",
                    "cancelled-checkpoint-check.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert!(matches!(discarded.outcome, ClassicScriptOutcome::Success(_)));
        });
    }

    #[cfg(feature = "gc_stress_test")]
    #[test]
    fn active_host_capability_survives_forced_moving_gc() {
        let mut cx = context();
        cx.enable_gc_stress_test();
        let mut host = MockHost::new();
        cx.with_browser_script_realm(|realm| {
            let result = host_run(
                realm,
                &mut host,
                "const dom = __wildBuzzardDom;\n\
                 const parent = dom.document();\n\
                 for (let i = 0; i < 20; i++) {\n\
                   const garbage = { text: 'moving-' + i, nested: { i } };\n\
                   const child = dom.createText(garbage.text);\n\
                   dom.append(parent, child);\n\
                 }",
            );
            assert_eq!(
                result.script.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            assert!(matches!(result.host, BrowserHostPhaseOutcome::Completed(_)));
            assert_eq!(
                host.calls
                    .iter()
                    .filter(|call| **call == "create_text")
                    .count(),
                20
            );
        });
    }

    #[cfg(feature = "gc_stress_test")]
    #[test]
    fn document_budget_keeps_realm_jobs_and_host_tokens_rooted_through_moving_gc() {
        let mut cx = context();
        cx.enable_gc_stress_test();
        let mut host = MockHost::new();
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(5)).unwrap();
        cx.with_browser_script_realm(|realm| {
            let result = realm.with_hosted_document_script_budget(
                &mut host,
                limits,
                &ScriptInterruptHandle::new(),
                |realm| {
                    assert_eq!(
                        realm
                            .perform_hosted_document_microtask_checkpoint()
                            .checkpoint
                            .outcome,
                        MicrotaskCheckpointOutcome::Complete
                    );
                    let first = realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                        "const dom = __wildBuzzardDom;\n\
                             globalThis.movingState = { marker: 'alive' };\n\
                             globalThis.movingParent = dom.document();\n\
                             globalThis.movingChild = dom.createText('before');\n\
                             dom.append(movingParent, movingChild);\n\
                             Promise.resolve().then(() => {\n\
                               movingState.marker = 'after';\n\
                               dom.setText(movingChild, movingState.marker);\n\
                             });",
                        "moving-document-1.js",
                    ));
                    assert!(matches!(first.script.outcome, ClassicScriptOutcome::Success(_)));
                    assert_eq!(
                        realm
                            .perform_hosted_document_microtask_checkpoint()
                            .checkpoint
                            .outcome,
                        MicrotaskCheckpointOutcome::Complete
                    );
                    let second = realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                        "if (movingState.marker !== 'after') throw 'moving root lost';\n\
                             __wildBuzzardDom.setText(movingChild, 'second');",
                        "moving-document-2.js",
                    ));
                    assert!(matches!(second.script.outcome, ClassicScriptOutcome::Success(_)));
                    assert_eq!(realm.document_script_candidates(), Some(2));
                },
            );
            assert!(result.is_ok());
            assert_eq!(host.last_text.as_deref(), Some("second"));
        });
    }
}

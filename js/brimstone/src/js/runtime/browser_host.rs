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
/// scalar tokens they return. Returning any non-script error must leave the current phase safe to
/// abort. `abort_phase` must not allocate and permanently retires the task after cancellation or a
/// runtime resource failure.
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
}

pub(crate) struct BrowserHostScopeGuard<'host, H: BrowserHostTask> {
    raw: Context,
    data: NonNull<()>,
    _borrow: PhantomData<&'host mut H>,
}

impl<'host, H: BrowserHostTask> BrowserHostScopeGuard<'host, H> {
    pub(crate) fn install(mut raw: Context, host: &'host mut H) -> Result<Self, BrowserHostError> {
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
        if self.raw.browser_host.dispatch_busy {
            std::process::abort();
        }
        let Some(active) = self.raw.browser_host.active.take() else {
            std::process::abort();
        };
        if active.data != self.data {
            std::process::abort();
        }
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
    use std::{rc::Rc, time::Duration};

    use crate::{
        common::options::OptionsBuilder,
        runtime::{
            BrowserScriptRealm, ClassicScriptLimits, ClassicScriptOutcome, ClassicScriptRequest,
            Context, ContextBuilder, MicrotaskCheckpointOutcome, OwnedContext,
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
        calls: Vec<&'static str>,
        aborts: u32,
        create_error: Option<BrowserHostError>,
        panic_on_document: bool,
        last_text: Option<String>,
    }

    impl MockHost {
        fn new() -> Self {
            Self {
                next_token: 1,
                revision: 0,
                phase_before: 0,
                phase_commands: 0,
                phase_created: 0,
                calls: Vec::new(),
                aborts: 0,
                create_error: None,
                panic_on_document: false,
                last_text: None,
            }
        }

        fn token(&mut self) -> BrowserHostNodeToken {
            let token = BrowserHostNodeToken::new(self.next_token).unwrap();
            self.next_token += 1;
            token
        }

        fn mutation(&mut self, call: &'static str, created: bool) {
            self.calls.push(call);
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
            self.calls.push("document");
            Ok(self.token())
        }

        fn lookup_node(&mut self, _slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError> {
            self.calls.push("lookup");
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
            _data: &str,
        ) -> Result<(), BrowserHostError> {
            self.mutation("set_text", false);
            Ok(())
        }

        fn finish_phase(&mut self) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
            self.calls.push("finish");
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
            Ok(outcome)
        }

        fn abort_phase(&mut self) {
            self.aborts += 1;
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
        let mut host = MockHost::new();
        host.panic_on_document = true;
        cx.with_browser_script_realm(|realm| {
            let failed = host_run(realm, &mut host, "__wildBuzzardDom.document();");
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
}

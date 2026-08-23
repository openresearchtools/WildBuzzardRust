use std::collections::VecDeque;

use crate::{
    completion_value, eval_err, handle_scope,
    runtime::{
        Context, EvalResult, HeapItemKind, HeapPtr, Realm, Value,
        abstract_operations::{call, call_object},
        async_generator_object::{AsyncGeneratorObject, async_generator_resume},
        builtin_generator::BuiltinGenerator,
        gc::{AnyHeapItem, HeapVisitor},
        generator_object::{GeneratorCompletionType, GeneratorObject},
        intrinsics::promise_constructor::execute_then,
        object_value::ObjectValue,
        promise_object::{PromiseCapability, PromiseObject, PromiseReactionKind},
    },
};

pub struct TaskQueue {
    tasks: VecDeque<Task>,
    browser_pending_cap: Option<BrowserPendingTaskCap>,
}

#[derive(Clone, Copy)]
struct BrowserPendingTaskCap {
    limit: usize,
    overflowed: bool,
    peak_len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrowserPendingTaskRetirement {
    pub(crate) retired: usize,
    pub(crate) overflowed: bool,
    pub(crate) peak_len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrowserPendingTaskCapInstallError {
    Invariant,
    Allocation,
}

pub enum Task {
    Callback1(Callback1Task),
    AwaitResume(AwaitResumeTask),
    PromiseThenReaction(PromiseThenReactionTask),
    PromiseThenSettle(PromiseThenSettleTask),
}

impl TaskQueue {
    pub fn new() -> Self {
        Self { tasks: VecDeque::new(), browser_pending_cap: None }
    }

    pub fn enqueue(&mut self, task: Task) {
        if let Some(cap) = self.browser_pending_cap.as_mut() {
            if cap.overflowed || self.tasks.len() >= cap.limit {
                cap.overflowed = true;
                return;
            }
        }
        self.tasks.push_back(task);
        if let Some(cap) = self.browser_pending_cap.as_mut() {
            cap.peak_len = cap.peak_len.max(self.tasks.len());
        }
    }

    pub fn enqueue_callback_1_task(&mut self, func: Value, arg: Value) {
        self.enqueue(Task::Callback1(Callback1Task::new(func, arg)));
    }

    pub fn enqueue_await_resume_task(
        &mut self,
        kind: PromiseReactionKind,
        generator: HeapPtr<AnyHeapItem>,
        result: Value,
    ) {
        self.enqueue(Task::AwaitResume(AwaitResumeTask::new(kind, generator, result)));
    }

    pub fn enqueue_promise_then_reaction_task(
        &mut self,
        kind: PromiseReactionKind,
        handler: Option<HeapPtr<ObjectValue>>,
        capability: Option<HeapPtr<PromiseCapability>>,
        result: Value,
        realm: Option<HeapPtr<Realm>>,
    ) {
        self.enqueue(Task::PromiseThenReaction(PromiseThenReactionTask::new(
            kind, handler, capability, result, realm,
        )));
    }

    pub fn enqueue_promise_then_settle_task(
        &mut self,
        then_function: HeapPtr<ObjectValue>,
        resolution: HeapPtr<ObjectValue>,
        promise: HeapPtr<PromiseObject>,
        realm: HeapPtr<Realm>,
    ) {
        self.enqueue(Task::PromiseThenSettle(PromiseThenSettleTask::new(
            then_function,
            resolution,
            promise,
            realm,
        )));
    }

    pub fn visit_roots(&mut self, visitor: &mut impl HeapVisitor) {
        for task in &mut self.tasks {
            match task {
                Task::Callback1(Callback1Task { func, arg }) => {
                    visitor.visit_value(func);
                    visitor.visit_value(arg);
                }
                Task::AwaitResume(AwaitResumeTask { generator, result, .. }) => {
                    visitor.visit_pointer(generator);
                    visitor.visit_value(result);
                }
                Task::PromiseThenReaction(PromiseThenReactionTask {
                    kind: _,
                    handler,
                    capability,
                    result,
                    realm,
                }) => {
                    visitor.visit_pointer_opt(handler);
                    visitor.visit_pointer_opt(capability);
                    visitor.visit_value(result);
                    visitor.visit_pointer_opt(realm);
                }
                Task::PromiseThenSettle(PromiseThenSettleTask {
                    then_function,
                    resolution,
                    promise,
                    realm,
                }) => {
                    visitor.visit_pointer(then_function);
                    visitor.visit_pointer(resolution);
                    visitor.visit_pointer(promise);
                    visitor.visit_pointer(realm);
                }
            }
        }
    }

    pub(crate) fn browser_script_is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(crate) fn browser_script_len(&self) -> usize {
        self.tasks.len()
    }

    pub(crate) fn clear_browser_script_tasks(&mut self) {
        self.tasks.clear();
    }

    pub(crate) fn install_browser_pending_cap(
        &mut self,
        limit: usize,
    ) -> Result<(), BrowserPendingTaskCapInstallError> {
        if limit == 0 || self.browser_pending_cap.is_some() || self.tasks.len() > limit {
            return Err(BrowserPendingTaskCapInstallError::Invariant);
        }
        self.tasks
            .try_reserve_exact(limit - self.tasks.len())
            .map_err(|_| BrowserPendingTaskCapInstallError::Allocation)?;
        self.browser_pending_cap =
            Some(BrowserPendingTaskCap { limit, overflowed: false, peak_len: self.tasks.len() });
        Ok(())
    }

    pub(crate) fn browser_pending_cap_overflow(&self) -> Option<usize> {
        self.browser_pending_cap
            .filter(|cap| cap.overflowed)
            .map(|cap| cap.limit)
    }

    #[cfg(test)]
    pub(crate) fn browser_pending_cap_is_active(&self) -> bool {
        self.browser_pending_cap.is_some()
    }

    #[cfg(test)]
    pub(crate) fn browser_pending_cap_peak_len(&self) -> Option<usize> {
        self.browser_pending_cap.map(|cap| cap.peak_len)
    }

    /// Retire one exact scoped browser queue. The precondition checks are constant-time and the
    /// loop executes at most `expected_limit` iterations. `poll` cannot stop retirement: it lets
    /// the document owner observe cancellation/deadline state while cleanup continues to empty.
    pub(crate) fn retire_browser_pending_cap(
        &mut self,
        expected_limit: usize,
        mut poll: impl FnMut(),
    ) -> BrowserPendingTaskRetirement {
        let Some(cap) = self.browser_pending_cap else {
            std::process::abort();
        };
        if cap.limit != expected_limit || self.tasks.len() > cap.limit {
            std::process::abort();
        }

        let mut retired = 0;
        poll();
        while self.tasks.pop_front().is_some() {
            retired += 1;
            if retired > cap.limit {
                std::process::abort();
            }
            poll();
        }
        let removed = self
            .browser_pending_cap
            .take()
            .unwrap_or_else(|| std::process::abort());
        if !self.tasks.is_empty() || removed.limit != expected_limit {
            std::process::abort();
        }
        BrowserPendingTaskRetirement {
            retired,
            overflowed: removed.overflowed,
            peak_len: removed.peak_len,
        }
    }

    /// Retire a queue which predates document admission without scanning more than `hard_limit`
    /// entries. A larger queue is rejected in constant time and must permanently poison its owner.
    pub(crate) fn retire_foreign_browser_tasks_bounded(
        &mut self,
        hard_limit: usize,
        mut poll: impl FnMut(),
    ) -> Option<usize> {
        if self.browser_pending_cap.is_some() || self.tasks.len() > hard_limit {
            return None;
        }
        let expected = self.tasks.len();
        let mut retired = 0;
        poll();
        while self.tasks.pop_front().is_some() {
            retired += 1;
            if retired > hard_limit {
                std::process::abort();
            }
            poll();
        }
        if retired != expected || !self.tasks.is_empty() {
            std::process::abort();
        }
        Some(retired)
    }
}

impl Task {
    fn execute(self, cx: Context) -> EvalResult<()> {
        match self {
            Task::Callback1(task) => task.execute(cx),
            Task::AwaitResume(task) => task.execute(cx),
            Task::PromiseThenReaction(task) => task.execute(cx),
            Task::PromiseThenSettle(task) => task.execute(cx),
        }
    }
}

impl Context {
    /// Run all tasks until the task queue is empty.
    pub fn run_all_tasks(&mut self) -> EvalResult<()> {
        self.assert_owner_execution_live();
        while let Some(task) = self.task_queue().tasks.pop_front() {
            handle_scope!(*self, task.execute(*self))?;
        }

        Ok(())
    }

    /// Drain exactly one browser microtask checkpoint under the active admission policy.
    pub(crate) fn run_browser_script_tasks(&mut self) -> EvalResult<()> {
        self.assert_owner_execution_live();
        while !self.task_queue().tasks.is_empty() {
            self.browser_script_before_job();
            let task = self
                .task_queue()
                .tasks
                .pop_front()
                .unwrap_or_else(|| std::process::abort());
            handle_scope!(*self, task.execute(*self))?;
        }

        Ok(())
    }
}

/// Call a function with a single argument.
pub struct Callback1Task {
    func: Value,
    arg: Value,
}

impl Callback1Task {
    fn new(func: Value, arg: Value) -> Self {
        Self { func, arg }
    }

    fn execute(&self, mut cx: Context) -> EvalResult<()> {
        let func = self.func.to_handle(cx);
        let arg = self.arg.to_handle(cx);

        // Realm is only used to create errors before setting up the stack frame (e.g. non-callable
        // a stack overflow). Create these errors in the default realm for this context.
        let default_realm = cx.initial_realm_ptr();

        cx.with_initial_realm_stack_frame(default_realm, |cx| {
            call(cx, func, cx.undefined(), &[arg])?;
            Ok(())
        })
    }
}

/// Resume an async function that was paused at an `await` expression.
pub struct AwaitResumeTask {
    /// Whether the awaited promise was resolved or rejected.
    kind: PromiseReactionKind,
    /// The suspended async function that should be resumed with the provided completion.
    /// - For regular async functions this is a GeneratorObject
    /// - For async generators this is an AsyncGeneratorObject
    /// - For builtin functions this is a BuiltinGenerator
    generator: HeapPtr<AnyHeapItem>,
    /// The value the await expression completes to, whether a normal value or thrown error.
    result: Value,
}

impl AwaitResumeTask {
    fn new(kind: PromiseReactionKind, generator: HeapPtr<AnyHeapItem>, result: Value) -> Self {
        Self { kind, generator, result }
    }

    fn execute(&self, mut cx: Context) -> EvalResult<()> {
        let generator = self.generator.to_handle();
        let completion_value = self.result.to_handle(cx);
        let completion_type = match self.kind {
            PromiseReactionKind::Fulfill => GeneratorCompletionType::Normal,
            PromiseReactionKind::Reject => GeneratorCompletionType::Throw,
        };

        match generator.shape().kind() {
            HeapItemKind::GeneratorObject => {
                let generator = generator.cast::<GeneratorObject>();
                let realm = generator.closure_ptr().function_ptr().realm_ptr();
                cx.with_initial_realm_stack_frame(realm, |mut cx| {
                    cx.vm()
                        .resume_generator(generator, completion_value, completion_type)?;
                    Ok(())
                })
            }
            HeapItemKind::AsyncGeneratorObject => {
                let async_generator = generator.cast::<AsyncGeneratorObject>();

                // Must execute in the realm of the async generator since AsyncGeneratorResume may need
                // to drain the async queue when the VM stack is empty.
                cx.with_initial_realm_stack_frame(async_generator.realm_ptr(), |cx| {
                    async_generator_resume(cx, async_generator, completion_value, completion_type)?;
                    Ok(())
                })
            }
            HeapItemKind::BuiltinGenerator => {
                let builtin_generator = generator.cast::<BuiltinGenerator>();

                let completion_result = match self.kind {
                    PromiseReactionKind::Fulfill => Ok(completion_value),
                    PromiseReactionKind::Reject => eval_err!(completion_value),
                };

                cx.with_initial_realm_stack_frame(builtin_generator.realm_ptr(), |cx| {
                    builtin_generator.resume(cx, completion_result)?;
                    Ok(())
                })
            }
            _ => panic!("Unexpected generator type"),
        }
    }
}

pub struct PromiseThenReactionTask {
    /// Whether the promise was resolved or rejected.
    kind: PromiseReactionKind,
    /// A function to call on the result value.
    handler: Option<HeapPtr<ObjectValue>>,
    /// A promise capability to resolve or reject with the result of the handler function.
    capability: Option<HeapPtr<PromiseCapability>>,
    /// The value that the promise was resolved or rejected with.
    result: Value,
    /// The realm to set as the topmost execution context before executing the handler.
    realm: Option<HeapPtr<Realm>>,
}

impl PromiseThenReactionTask {
    fn new(
        kind: PromiseReactionKind,
        handler: Option<HeapPtr<ObjectValue>>,
        capability: Option<HeapPtr<PromiseCapability>>,
        result: Value,
        realm: Option<HeapPtr<Realm>>,
    ) -> Self {
        Self { kind, handler, capability, result, realm }
    }

    fn execute(&self, mut cx: Context) -> EvalResult<()> {
        // A null realm indicates there is no handler and no user code will be executed. However
        // we still set the initial realm to the current realm to be safe in case any intrinsics
        // need to be accessed (e.g. for creating errors).
        let realm = self.realm.unwrap_or_else(|| cx.initial_realm_ptr());

        cx.with_initial_realm_stack_frame(realm, |cx| {
            let result = self.result.to_handle(cx);
            let capability = self.capability.map(|c| c.to_handle());

            // Call the handler if it exists on the result value
            let handler_result = if let Some(handler) = self.handler {
                let handler = handler.to_handle();
                call_object(cx, handler, cx.undefined(), &[result])
            } else {
                // If no handler was provided treat the handler result as a default normal or throw
                match self.kind {
                    PromiseReactionKind::Fulfill => Ok(result),
                    PromiseReactionKind::Reject => eval_err!(result),
                }
            };

            if let Some(capability) = capability {
                // Resolve or reject the capability with the result of the handler
                match completion_value!(handler_result) {
                    Ok(handler_result) => {
                        let resolve = capability.resolve();
                        call_object(cx, resolve, cx.undefined(), &[handler_result])?;
                    }
                    Err(handler_result) => {
                        let reject = capability.reject();
                        call_object(cx, reject, cx.undefined(), &[handler_result])?;
                    }
                }
            } else {
                debug_assert!(handler_result.is_ok());
            };

            Ok(())
        })
    }
}

/// Call a `then` function with new `resolve` and `reject` functions in order to settle a promise.
pub struct PromiseThenSettleTask {
    /// The `then` function that should be called.
    then_function: HeapPtr<ObjectValue>,
    /// The object that contains the `then` function, used as the `this` value when calling `then`.
    resolution: HeapPtr<ObjectValue>,
    /// The promise that will be settled by calling `then`.
    promise: HeapPtr<PromiseObject>,
    /// The realm to set as the topmost execution context before executing the `then` function.
    realm: HeapPtr<Realm>,
}

impl PromiseThenSettleTask {
    fn new(
        then_function: HeapPtr<ObjectValue>,
        resolution: HeapPtr<ObjectValue>,
        promise: HeapPtr<PromiseObject>,
        realm: HeapPtr<Realm>,
    ) -> Self {
        Self { then_function, resolution, promise, realm }
    }

    fn execute(&self, mut cx: Context) -> EvalResult<()> {
        cx.with_initial_realm_stack_frame(self.realm, |cx| {
            let then_function = self.then_function.to_handle();
            let resolution = self.resolution.to_handle().into();
            let promise = self.promise.to_handle();

            execute_then(cx, then_function, resolution, promise)?;

            Ok(())
        })
    }
}

#[cfg(test)]
mod browser_pending_cap_tests {
    use std::cell::Cell;

    use super::*;

    fn inert_task() -> Task {
        Task::Callback1(Callback1Task::new(Value::undefined(), Value::undefined()))
    }

    #[test]
    fn scoped_pending_cap_drops_overflow_monotonically_and_retires_with_bounded_polls() {
        let mut queue = TaskQueue::new();
        assert_eq!(queue.install_browser_pending_cap(2), Ok(()));
        queue.enqueue(inert_task());
        queue.enqueue(inert_task());
        let capacity_at_limit = queue.tasks.capacity();
        queue.enqueue(inert_task());
        assert_eq!(queue.browser_script_len(), 2);
        assert_eq!(queue.tasks.capacity(), capacity_at_limit);
        assert_eq!(queue.browser_pending_cap_overflow(), Some(2));

        let _ = queue.tasks.pop_front();
        queue.enqueue(inert_task());
        assert_eq!(queue.browser_script_len(), 1, "overflow must never reopen admission");

        let polls = Cell::new(0);
        let retirement = queue.retire_browser_pending_cap(2, || polls.set(polls.get() + 1));
        assert_eq!(
            retirement,
            BrowserPendingTaskRetirement { retired: 1, overflowed: true, peak_len: 2 }
        );
        assert_eq!(polls.get(), 2);
        assert!(queue.browser_script_is_empty());
        assert_eq!(queue.browser_pending_cap_overflow(), None);

        assert_eq!(queue.install_browser_pending_cap(1), Ok(()));
        queue.enqueue(inert_task());
        let clean = queue.retire_browser_pending_cap(1, || {});
        assert_eq!(
            clean,
            BrowserPendingTaskRetirement { retired: 1, overflowed: false, peak_len: 1 }
        );
    }

    #[test]
    fn foreign_retirement_rejects_over_limit_without_touching_queue() {
        let mut queue = TaskQueue::new();
        for _ in 0..3 {
            queue.enqueue(inert_task());
        }
        let polls = Cell::new(0);
        assert_eq!(
            queue.retire_foreign_browser_tasks_bounded(2, || polls.set(polls.get() + 1)),
            None
        );
        assert_eq!(polls.get(), 0);
        assert_eq!(queue.browser_script_len(), 3);

        assert_eq!(
            queue.retire_foreign_browser_tasks_bounded(3, || polls.set(polls.get() + 1)),
            Some(3)
        );
        assert_eq!(polls.get(), 4);
        assert!(queue.browser_script_is_empty());
    }
}

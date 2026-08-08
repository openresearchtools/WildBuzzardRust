# wild_buzzard_runtime

This crate provides executor-independent cancellation tokens, a monotonic lifecycle state machine,
a bounded FIFO event queue, and a manually driven task queue. It creates no thread, reads no clock,
and selects no async runtime. Future Linux adapters can drive these primitives from Wayland/X11,
I/O, or application event loops.

All producer paths are bounded and return explicit backpressure or shutdown errors. Cancellation and
lifecycle transitions use atomics; concurrent callers deterministically elect one transition winner.
Queued work accepted before shutdown drains in FIFO order, while late dispatch is rejected.
Task IDs are assigned in the same queue-owned critical section as acceptance, so full or closed
dispatch attempts never consume an ID.

Firefox ESR153 reference paths inspected at
`c19b7e89270787889495688244ec6ee8e79288a1`:

- `xpcom/threads/nsIEventTarget.idl`
- `xpcom/threads/nsICancelableRunnable.h`
- `xpcom/threads/TaskController.h`
- `xpcom/threads/TaskController.cpp`
- `xpcom/base/AppShutdown.h`
- `xpcom/base/ShutdownPhase.h`
- `xpcom/tests/gtest/TestTaskController.cpp`
- `xpcom/tests/gtest/TestTargetShutdownTask.cpp`

Their full path history was inspected, including fallible-dispatch work and shutdown-task tests. This
wave does not implement threads, timers, delayed dispatch, priorities, process launch, OS shutdown,
or an async executor. Concrete integration targets only Linux x86_64; no Windows, macOS, Android, or
mobile adapter is planned.

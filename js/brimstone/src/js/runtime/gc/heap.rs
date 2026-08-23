use std::{
    alloc::Layout,
    cell::{Cell, RefCell},
    mem::size_of,
    ops::Range,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(feature = "alloc_error")]
use crate::runtime::alloc_error::AllocError;
use crate::{
    common::{constants::MAX_HEAP_SIZE, serialized_heap::SerializedHeap},
    const_assert,
    runtime::{
        Context,
        alloc_error::AllocResult,
        gc::{
            GcType, HeapPtr, HeapVisitor,
            garbage_collector::GarbageCollector,
            handle::HandleContext,
            heap_serializer::{HeapSpaceDeserializer, calculate_extra_offset},
        },
    },
    static_assert,
};

/// Heap Layout:
///
/// | HeapInfo | Permanent | Semispace 1 | Semispace 2 |
///
/// Permanent is an optional region used for objects that are never collected such as some builtins.
/// The rest of the heap is split into two semispaces which are used as the main heap.
///
/// Heap is aligned to a 1GB boundary and contains a reference to the context. This allows any heap
/// pointer to be masked to find the start of the heap along with its context.
pub struct Heap {
    /// Pointer to the start of the heap, where HeapInfo is stored
    heap_start: *const u8,
    /// Pointer to the end of the heap
    heap_end: *const u8,
    /// Pointer to the start of the permanent region
    permanent_start: *const u8,
    /// Pointer to the end of the permanent region
    permanent_end: *const u8,
    /// Pointer to the start of the current semispace
    start: *const u8,
    /// Pointer to where the next heap allocation will occur, grows as more allocations occur
    current: *const u8,
    /// Pointer to the end of the current semispace
    end: *const u8,
    // Pointer to the start of the next semispace
    next_heap_start: *const u8,
    // Pointer to the end of the next semispace
    next_heap_end: *const u8,
    layout: Layout,

    /// Never-reused identity for this exact aligned allocation's authority-range registration.
    /// This prevents an address which is later reused from satisfying an older heap's teardown.
    authority_registration: u64,

    #[cfg(feature = "gc_stress_test")]
    pub gc_stress_test: bool,
}

/// Heap must be aligned to max heap size so that we can always mask heap pointers to find the start
/// of the heap.
const HEAP_ALIGNMENT: usize = MAX_HEAP_SIZE;
static_assert!(HEAP_ALIGNMENT.is_power_of_two());

/// Owner authority for live aligned Brimstone heap allocations on this owner thread.
///
/// Heap-item pointers normally recover `HeapInfo` by alignment masking, but serializer copies are
/// intentionally traversed from ordinary `Vec` allocations. The exact-range registry distinguishes
/// those non-live copies without reading an arbitrary masked address. Every live heap range is
/// registered before it can contain objects and bound once to the same monotone owner poison bit.
#[derive(Clone, Copy)]
struct HeapAuthorityRange {
    start: usize,
    end: usize,
    registration: u64,
    owner: Option<Context>,
}

static NEXT_HEAP_AUTHORITY_REGISTRATION: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// A destructor-free TLS cell points to deliberately process-lifetime registry storage.
    ///
    /// A caller's `thread_local! OwnedContext` may begin initialization before this key and is then
    /// destroyed after an ordinary destructible TLS value would already be unavailable. Leaking the
    /// small owner-thread registry keeps exact-range removal and validation available throughout
    /// TLS teardown. Missing key access still aborts; it is never interpreted as detached serializer
    /// authority.
    static HEAP_AUTHORITY_REGISTRY: Cell<*mut RefCell<Vec<HeapAuthorityRange>>> = const {
        Cell::new(std::ptr::null_mut())
    };
}

#[cfg(test)]
#[derive(Clone, Default)]
struct TestHeapAuthorityAudit {
    thread_id: Option<std::thread::ThreadId>,
    registered: Vec<u64>,
    retired: Vec<u64>,
}

#[cfg(test)]
static TEST_HEAP_AUTHORITY_AUDITS: std::sync::Mutex<Vec<TestHeapAuthorityAudit>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn with_test_heap_authority_audit(
    thread_id: std::thread::ThreadId,
    f: impl FnOnce(&mut TestHeapAuthorityAudit),
) {
    let Ok(mut audits) = TEST_HEAP_AUTHORITY_AUDITS.lock() else {
        std::process::abort();
    };
    let index = audits
        .iter()
        .position(|audit| audit.thread_id == Some(thread_id))
        .unwrap_or_else(|| {
            audits.push(TestHeapAuthorityAudit {
                thread_id: Some(thread_id),
                ..TestHeapAuthorityAudit::default()
            });
            audits.len() - 1
        });
    f(&mut audits[index]);
}

#[cfg(test)]
fn record_test_heap_authority_registration(registration: u64) {
    with_test_heap_authority_audit(std::thread::current().id(), |audit| {
        assert!(!audit.registered.contains(&registration));
        assert!(!audit.retired.contains(&registration));
        audit.registered.push(registration);
    });
}

#[cfg(test)]
fn record_test_heap_authority_retirement(registration: u64) {
    with_test_heap_authority_audit(std::thread::current().id(), |audit| {
        assert!(audit.registered.contains(&registration));
        assert!(!audit.retired.contains(&registration));
        audit.retired.push(registration);
    });
}

#[cfg(test)]
fn test_heap_authority_audit(thread_id: std::thread::ThreadId) -> TestHeapAuthorityAudit {
    let Ok(audits) = TEST_HEAP_AUTHORITY_AUDITS.lock() else {
        std::process::abort();
    };
    let mut audit = audits
        .iter()
        .find(|audit| audit.thread_id == Some(thread_id))
        .cloned()
        .unwrap_or_default();
    audit.registered.sort_unstable();
    audit.retired.sort_unstable();
    audit
}

fn with_heap_authority_ranges<R>(f: impl FnOnce(&RefCell<Vec<HeapAuthorityRange>>) -> R) -> R {
    HEAP_AUTHORITY_REGISTRY
        .try_with(|slot| {
            let mut registry = slot.get();
            if registry.is_null() {
                registry = Box::into_raw(Box::new(RefCell::new(Vec::new())));
                slot.set(registry);
            }

            // SAFETY: The box is intentionally leaked for the process lifetime. This thread-local
            // cell is the only place that stores its address, and `RefCell` serializes reentry.
            f(unsafe { &*registry })
        })
        .unwrap_or_else(|_| std::process::abort())
}

fn next_heap_authority_registration() -> u64 {
    NEXT_HEAP_AUTHORITY_REGISTRATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| next.checked_add(1))
        .unwrap_or_else(|_| std::process::abort())
}

fn register_heap_authority_range(start: *const u8, end: *const u8) -> u64 {
    let registration = next_heap_authority_registration();
    with_heap_authority_ranges(|ranges| {
        let Ok(mut ranges) = ranges.try_borrow_mut() else {
            std::process::abort();
        };
        let start = start as usize;
        let end = end as usize;
        if start >= end
            || ranges
                .iter()
                .any(|range| start < range.end && range.start < end)
        {
            std::process::abort();
        }
        ranges.push(HeapAuthorityRange { start, end, registration, owner: None });
    });
    #[cfg(test)]
    record_test_heap_authority_registration(registration);
    registration
}

fn bind_heap_authority_owner(start: *const u8, owner: Context) {
    with_heap_authority_ranges(|ranges| {
        let Ok(mut ranges) = ranges.try_borrow_mut() else {
            std::process::abort();
        };
        let Some(range) = ranges
            .iter_mut()
            .find(|range| range.start == start as usize)
        else {
            std::process::abort();
        };
        if range.owner.is_some() {
            std::process::abort();
        }
        range.owner = Some(owner);
    });
}

fn unregister_heap_authority_range(start: *const u8, end: *const u8, registration: u64) {
    with_heap_authority_ranges(|ranges| {
        let Ok(mut ranges) = ranges.try_borrow_mut() else {
            std::process::abort();
        };
        let Some(index) = ranges.iter().position(|range| {
            range.start == start as usize
                && range.end == end as usize
                && range.registration == registration
        }) else {
            std::process::abort();
        };
        ranges.swap_remove(index);
    });
    #[cfg(test)]
    record_test_heap_authority_retirement(registration);
}

fn heap_authority_owner_for_pointer<T>(ptr: *const T) -> Option<Option<Context>> {
    with_heap_authority_ranges(|ranges| {
        let Ok(ranges) = ranges.try_borrow() else {
            std::process::abort();
        };
        let ptr = ptr as usize;
        ranges
            .iter()
            .find(|range| range.start <= ptr && ptr < range.end)
            .map(|range| range.owner)
    })
}

/// All heap items are aligned to 8-byte boundaries
type HeapItemAlignmentType = u64;
const HEAP_ITEM_ALIGNMENT: usize = std::mem::size_of::<HeapItemAlignmentType>();

impl Heap {
    pub(crate) fn new(initial_size: usize) -> Heap {
        // Create uninitialized buffer of memory for heap
        unsafe {
            let layout = Layout::from_size_align(initial_size, HEAP_ALIGNMENT).unwrap();
            let heap_start = std::alloc::alloc(layout);
            if heap_start.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            let heap_end = heap_start.add(initial_size);

            let semispace_size = (initial_size - size_of::<HeapInfo>()) / 2;

            // Leave room for heap info struct at start of heap
            let start = heap_start.add(size_of::<HeapInfo>());
            let end = start.add(semispace_size);

            // Find bounds of other heap part
            let next_heap_start = end;
            let next_heap_end = end.add(semispace_size);

            // The allocation contains raw bytes, so initialize `HeapInfo` with a pointer write
            // before creating a reference to it.
            heap_start.cast::<HeapInfo>().write(HeapInfo::new());

            let authority_registration = register_heap_authority_range(heap_start, heap_end);
            Heap {
                heap_start,
                heap_end,
                // Permanent region is empty to start
                permanent_start: NonNull::dangling().as_ptr(),
                permanent_end: NonNull::dangling().as_ptr(),
                start,
                current: start,
                end,
                next_heap_start,
                next_heap_end,
                layout,
                authority_registration,

                #[cfg(feature = "gc_stress_test")]
                gc_stress_test: false,
            }
        }
    }

    /// Initialize an uninitialized heap with a serialized heap. This will copy the serialized heap
    /// over and fix all pointers within the new heap.
    ///
    /// Pointers were encoded as offsets and can be restored by adding to the new heap base pointer.
    pub fn init_from_serialized(&mut self, cx: Context, serialized: &SerializedHeap) {
        // Size of the `HeapInfo` struct can be different from the serialized heap, e.g. due to
        // feature flags. We must apply an additional offset to account for this.
        let extra_offset = calculate_extra_offset(serialized);
        let new_permanent_start_offset = extra_offset + serialized.permanent_space.start_offset;
        let new_current_start_offset = extra_offset + serialized.current_space.start_offset;

        // Ensure actual heap has enough room for the serialized heap
        let permanent_size = serialized.permanent_space.bytes.len();
        let semispace_size = (self.heap_size() - new_current_start_offset) / 2;
        if serialized.current_space.bytes.len() > semispace_size {
            panic!("Serialized heap is larger than the actual heap");
        }

        // Find bounds of permanent space
        self.permanent_start = unsafe { self.heap_start.add(new_permanent_start_offset) };
        self.permanent_end = unsafe { self.permanent_start.add(permanent_size) };

        // Copy permanent semispace into the actual heap
        self.permanent_heap_mut()
            .copy_from_slice(serialized.permanent_space.bytes);

        // Rewrite offsets in the permanent space to pointers in the new heap
        HeapSpaceDeserializer::deserialize(cx, self.permanent_heap_mut(), extra_offset);

        // Find bounds of used part of current semispace
        self.start = unsafe { self.heap_start.add(new_current_start_offset) };
        self.current = unsafe { self.start.add(serialized.current_space.bytes.len()) };

        // Copy used portion of current semispace into the actual heap
        self.current_used_heap_mut()
            .copy_from_slice(serialized.current_space.bytes);

        // Rewrite offsets in the current semispace to pointers in the new heap
        HeapSpaceDeserializer::deserialize(cx, self.current_used_heap_mut(), extra_offset);

        // Write the end of the current semispace
        self.end = unsafe { self.start.add(semispace_size) };

        // Write the bounds of the next semispace, making sure it is aligned
        self.next_heap_start = align_pointer_up(self.end, HEAP_ITEM_ALIGNMENT);
        self.next_heap_end = unsafe { self.next_heap_start.add(semispace_size) };

        self.debug_assert_heap_well_formed();
    }

    /// Create a new heap for a resizing GC, provided the old heap and the new heap size.
    pub fn new_for_resize(prev_heap: &Heap, new_size: usize) -> Heap {
        debug_assert!(new_size.is_power_of_two());

        let mut new_heap = Self::new(new_size);
        // Collector traversal dereferences objects in this to-space before active handle metadata
        // is transferred. Bind the exact owner now; `transfer_info_from` later swaps two
        // independently initialized `HeapInfo` values for that same owner.
        new_heap.info().set_context(prev_heap.info().cx());

        // Set up bounds of the new permanent semispace
        new_heap.permanent_start = unsafe {
            let offset = prev_heap.permanent_start.offset_from(prev_heap.heap_start);
            new_heap.heap_start.offset(offset)
        };
        new_heap.permanent_end = unsafe {
            new_heap
                .permanent_start
                .add(prev_heap.permanent_bytes_allocated())
        };

        // Calculate size of each new semispace. Make sure that we can evenly divide the remaining
        // heap into semispaces with the correct alignment and the exact same size.
        let semispaces_start = align_pointer_up(new_heap.permanent_end, HEAP_ITEM_ALIGNMENT * 2);
        let semispaces_start_offset = semispaces_start as usize - new_heap.heap_start as usize;
        let semispace_size = (new_heap.heap_size() - semispaces_start_offset) / 2;

        // Set up bounds of the new semispaces
        new_heap.start = semispaces_start;
        new_heap.current = new_heap.start;
        new_heap.end = unsafe { semispaces_start.add(semispace_size) };

        new_heap.next_heap_start = align_pointer_up(new_heap.end, HEAP_ITEM_ALIGNMENT);
        new_heap.next_heap_end = unsafe { new_heap.next_heap_start.add(semispace_size) };

        // Set additional GC flags
        #[cfg(feature = "gc_stress_test")]
        {
            new_heap.gc_stress_test = prev_heap.gc_stress_test;
        }

        new_heap.debug_assert_heap_well_formed();

        new_heap
    }

    fn debug_assert_heap_well_formed(&self) {
        // The semispaces must not extend beyond the end of the heap
        debug_assert!(self.end <= self.heap_end);
        debug_assert!(self.next_heap_end <= self.heap_end);

        // The semispaces must be aligned to the heap item alignment
        debug_assert!(is_heap_item_aligned(self.start));
        debug_assert!(is_heap_item_aligned(self.next_heap_start));

        // The semispaces must have the exact same size
        debug_assert_eq!(
            self.end as usize - self.start as usize,
            self.next_heap_end as usize - self.next_heap_start as usize
        );

        // The current pointer must be within the bounds of the current semispace
        debug_assert!(self.current >= self.start && self.current < self.end);
    }

    #[inline]
    pub(crate) fn info<'a>(&self) -> &'a mut HeapInfo {
        unsafe { &mut *(self.heap_start as *const _ as *mut HeapInfo) }
    }

    pub fn alloc_uninit<T>(cx: Context) -> AllocResult<HeapPtr<T>> {
        Self::alloc_uninit_with_size::<T>(cx, size_of::<T>())
    }

    /// Allocate an object of a given type with the specified size in bytes. When called directly,
    /// is used to allocate dynamically sized objects.
    ///
    /// Allocation will have at least the given size and is guaranteed to have 8-byte alignment.
    #[inline]
    pub fn alloc_uninit_with_size<T>(mut cx: Context, size: usize) -> AllocResult<HeapPtr<T>> {
        // Statically ensure that type is compatible with the heap's alignment.
        const_assert!(align_of::<T>() <= HEAP_ITEM_ALIGNMENT);

        let alloc_size = Self::alloc_size_for_request_size(size);

        // Run a GC on every allocation in stress test mode
        #[cfg(feature = "gc_stress_test")]
        if cx.heap.gc_stress_test {
            Self::run_gc(cx, GcType::Normal);
        }

        unsafe {
            let start = cx.heap.current;

            // Calculate where the current will be after this allocation, checking if there is room
            let next_current = start.add(alloc_size);
            if (next_current as usize) > (cx.heap.end as usize) {
                // If there is not room run a gc cycle

                // Resize the heap
                if cx.heap.heap_size() < cx.options.max_heap_size {
                    Self::run_gc(cx, GcType::Grow { alloc_size: Some(alloc_size) });
                } else {
                    Self::run_gc(cx, GcType::Normal);
                }

                // Make sure there is enough space for allocation after gc, otherwise we are out of
                // heap memory.
                if !cx.heap.has_room_for_alloc(alloc_size) {
                    #[cfg(feature = "alloc_error")]
                    {
                        return Err(AllocError::oom());
                    }

                    #[cfg(not(feature = "alloc_error"))]
                    {
                        panic!("Ran out of heap memory");
                    }
                }

                return Self::alloc_uninit_with_size(cx, alloc_size);
            }

            // Update end pointer and write into memory
            // Charge only the final allocation attempt, immediately before it becomes visible.
            // An attempt which first triggers GC recurses above and is therefore not double-counted.
            cx.browser_script_before_managed_allocation(alloc_size);
            cx.heap.current = next_current;
            let start = start.cast_mut().cast();

            Ok(HeapPtr::from_ptr(start))
        }
    }

    pub fn run_gc(cx: Context, type_: GcType) {
        GarbageCollector::run(cx, type_)
    }

    fn has_room_for_alloc(&self, alloc_size: usize) -> bool {
        unsafe { self.current.add(alloc_size) <= self.end }
    }

    pub fn heap_size(&self) -> usize {
        self.heap_end as usize - self.heap_start as usize
    }

    pub fn heap_start(&self) -> *const u8 {
        self.heap_start
    }

    pub fn current_heap_bounds(&self) -> Range<*const u8> {
        self.start..self.end
    }

    pub fn next_heap_bounds(&self) -> Range<*const u8> {
        self.next_heap_start..self.next_heap_end
    }

    pub fn permanent_heap_bounds(&self) -> Range<*const u8> {
        self.permanent_start..self.permanent_end
    }

    pub fn permanent_heap(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(self.permanent_start, self.permanent_bytes_allocated())
        }
    }

    pub fn permanent_heap_mut(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.permanent_start.cast_mut(),
                self.permanent_bytes_allocated(),
            )
        }
    }

    pub fn current_used_heap_bounds(&self) -> (*const u8, *const u8) {
        (self.start, self.current)
    }

    pub fn current_used_heap(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.start, self.bytes_allocated()) }
    }

    pub fn current_used_heap_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.start.cast_mut(), self.bytes_allocated()) }
    }

    pub fn bytes_allocated(&self) -> usize {
        self.current as usize - self.start as usize
    }

    pub fn permanent_bytes_allocated(&self) -> usize {
        self.permanent_end as usize - self.permanent_start as usize
    }

    /// Returns true if the current semispace is before the next semispace in memory.
    pub fn is_current_before_next(&self) -> bool {
        self.start < self.next_heap_start
    }

    pub fn swap_heaps(&mut self, free_space_start_ptr: *const u8) {
        let old_start = self.start;
        let old_end = self.end;

        self.start = self.next_heap_start;
        self.end = self.next_heap_end;

        self.next_heap_start = old_start;
        self.next_heap_end = old_end;

        self.current = free_space_start_ptr;
    }

    pub fn finish_resized_heap(&mut self, free_space_start_ptr: *const u8) {
        self.current = free_space_start_ptr;
        self.debug_assert_heap_well_formed();
    }

    /// Move the context and handle-stack metadata from the old heap into a resized heap.
    ///
    /// Both heaps start with a fully initialized `HeapInfo`. Swapping transfers all active handles
    /// to the new address while leaving the old heap with the new heap's empty metadata, which can
    /// then be dropped normally. This avoids both bitwise-copy aliasing and leaked handle blocks.
    pub(crate) fn transfer_info_from(&mut self, previous: &mut Heap) {
        if !self
            .info()
            .cx()
            .has_same_owner_identity(previous.info().cx())
        {
            std::process::abort();
        }
        unsafe {
            std::ptr::swap(
                self.heap_start.cast_mut().cast::<HeapInfo>(),
                previous.heap_start.cast_mut().cast::<HeapInfo>(),
            );
        }
    }

    pub fn alloc_size_for_request_size(request_byte_size: usize) -> usize {
        align_up(request_byte_size, HEAP_ITEM_ALIGNMENT)
    }

    pub fn visit_roots(&self, visitor: &mut impl HeapVisitor) {
        let info = self.info();
        let owner = info.cx();
        info.handle_context().visit_roots(owner, visitor)
    }

    /// Mark the current semispace as permanent, claiming it for the permanent region. Redistribute
    /// the remaining heap space between the two semispaces.
    pub fn mark_current_semispace_as_permanent(&mut self) {
        // The permanent region is before the two semispaces, so if the current semispace is the
        // last one then copy its allocated contents to the start of the first semispace, where the
        // permanent region will start.
        if self.start < self.next_heap_start {
            self.permanent_start = self.start;
            self.permanent_end = self.current;
        } else {
            let bytes_allocated = self.bytes_allocated();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.start,
                    self.next_heap_start.cast_mut(),
                    bytes_allocated,
                );
            }

            self.permanent_start = self.next_heap_start;
            self.permanent_end = unsafe { self.next_heap_start.add(bytes_allocated) };
        }

        // Make sure that we can evenly divide the remaining heap into semispaces with the correct
        // alignment and the exact same size.
        let semispaces_start_ptr = align_pointer_up(self.permanent_end, HEAP_ITEM_ALIGNMENT * 2);
        let semispaces_end_ptr = if self.end < self.next_heap_end {
            self.next_heap_end
        } else {
            self.end
        };

        // Calculate the remaining size for each semispace
        let available_size = semispaces_end_ptr as usize - semispaces_start_ptr as usize;
        let semispace_size = available_size / 2;

        // Write bounds of new semispaces
        self.start = semispaces_start_ptr;
        self.end = unsafe { semispaces_start_ptr.add(semispace_size) };

        self.next_heap_start = self.end;
        self.next_heap_end = semispaces_end_ptr;

        // Reset current pointer to start of newly empty start semispace
        self.current = self.start;

        self.debug_assert_heap_well_formed();
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        unsafe {
            // `HeapInfo` owns the handle blocks which are allocated separately from the semispace.
            // Drop it before releasing the raw aligned heap allocation.
            std::ptr::drop_in_place(self.heap_start.cast_mut().cast::<HeapInfo>());
            unregister_heap_authority_range(
                self.heap_start,
                self.heap_end,
                self.authority_registration,
            );
            std::alloc::dealloc(self.heap_start as *mut u8, self.layout);
        }
    }
}

// Align a number up, rounding down to zero
fn align_up(ptr_bits: usize, alignment: usize) -> usize {
    (ptr_bits + (alignment - 1)) & !(alignment - 1)
}

// Align a heap pointer, rounding up to infinity
fn align_pointer_up(ptr: *const u8, alignment: usize) -> *const u8 {
    align_up(ptr as usize, alignment) as *const u8
}

fn is_heap_item_aligned(ptr: *const u8) -> bool {
    ptr.cast::<HeapItemAlignmentType>().is_aligned()
}

/// Heap data stored at the beginning of the heap
pub struct HeapInfo {
    /// Reference to the context that holds this heap.
    context: Option<Context>,
    handle_context: HandleContext,
}

impl HeapInfo {
    fn new() -> Self {
        Self { context: None, handle_context: HandleContext::new() }
    }

    pub fn set_context(&mut self, cx: Context) {
        bind_heap_authority_owner(self as *mut HeapInfo as *const u8, cx);
        self.context = Some(cx);
    }

    #[inline]
    pub(crate) fn from_raw_heap_ptr<'a, T>(heap_ptr: *const T) -> &'a mut HeapInfo {
        const HEAP_BASE_MASK: usize = !(HEAP_ALIGNMENT - 1);
        let heap_base = ((heap_ptr as usize) & HEAP_BASE_MASK) as *mut HeapInfo;

        unsafe { &mut *heap_base }
    }

    pub(crate) fn cx(&self) -> Context {
        self.context.expect("heap context must be initialized")
    }

    /// Validate ordinary heap-item access. Registered live ranges must have a live owner. Detached
    /// serializer copies deliberately use this separate access-only path: they are unregistered and
    /// may be traversed by the internal serializer, but can never be admitted to a live root by
    /// [`Self::live_owner_for_root`].
    pub(crate) fn assert_pointer_access_authorized<T>(ptr: *const T) {
        match heap_authority_owner_for_pointer(ptr) {
            Some(Some(owner)) => owner.assert_owner_execution_live(),
            Some(None) => panic!("Brimstone heap owner authority is not initialized"),
            None => {}
        }
    }

    /// Recover the exact live owner required to create or rewrite a moving-GC root. Unlike detached
    /// serializer traversal, stale, unregistered, and not-yet-bound pointers always fail closed.
    pub(crate) fn live_owner_for_root<T>(ptr: *const T) -> Context {
        let owner = match heap_authority_owner_for_pointer(ptr) {
            Some(Some(owner)) => owner,
            Some(None) => panic!("Brimstone heap owner authority is not initialized"),
            None => panic!("pointer is not in an exact live Brimstone heap range"),
        };
        owner.assert_owner_execution_live();
        owner
    }

    /// Require a heap-bearing root value to belong to one exact live context owner.
    pub(crate) fn assert_pointer_has_exact_live_owner<T>(ptr: *const T, expected: Context) {
        let actual = Self::live_owner_for_root(ptr);
        assert!(
            actual.has_same_owner_identity(expected),
            "heap-bearing root value belongs to a different Brimstone owner"
        );
    }

    pub(crate) fn handle_context(&mut self) -> &mut HandleContext {
        &mut self.handle_context
    }
}

#[cfg(test)]
mod authority_tests {
    use std::{cell::RefCell, panic::catch_unwind, rc::Rc, sync::mpsc, thread};

    use crate::{
        common::options::OptionsBuilder,
        runtime::{ContextBuilder, OwnedContext},
    };

    use super::*;

    fn context() -> OwnedContext {
        let options = OptionsBuilder::new().serialized_heap(None).build().unwrap();
        ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .unwrap()
    }

    #[test]
    fn resize_to_space_has_exact_revocable_owner_before_metadata_transfer() {
        let owner = context();
        // SAFETY: The token stays on this thread, no aliasing references are retained, and the
        // temporary heap is destroyed before the owner.
        let mut raw = unsafe { owner.raw_context_unchecked() };
        let next_size = raw.heap.heap_size() * 2;
        let next_heap = Heap::new_for_resize(&raw.heap, next_size);
        let inherited_owner = next_heap.info().cx();

        assert!(inherited_owner.has_same_owner_identity(raw));
        HeapInfo::assert_pointer_access_authorized(next_heap.start);

        raw.poison_owner_execution();
        assert!(inherited_owner.owner_execution_is_poisoned());
        assert!(
            catch_unwind(|| HeapInfo::assert_pointer_access_authorized(next_heap.start)).is_err()
        );

        drop(next_heap);
        drop(owner);
    }

    #[test]
    fn caller_tls_owner_drops_after_destructor_free_registry_and_retires_each_range_once() {
        for nested_owner_count in [1, 2, 4, 8] {
            let (sender, receiver) = mpsc::channel();
            let join = thread::spawn(move || {
                thread_local! {
                    // This caller key starts initializing before heap construction first touches the
                    // internal authority key. Its values therefore drop during the ordering that
                    // caused the reviewed TLS AccessError.
                    static OWNERS: RefCell<Vec<OwnedContext>> = const {
                        RefCell::new(Vec::new())
                    };
                }

                let thread_id = thread::current().id();
                OWNERS.with(|owners| {
                    let mut owners = owners.borrow_mut();
                    for _ in 0..nested_owner_count {
                        owners.push(context());
                    }
                });
                sender.send(thread_id).unwrap();
                // Pure safe caller exit: TLS owns and destroys every `OwnedContext`.
            });

            let thread_id = receiver.recv().unwrap();
            join.join().unwrap();
            let audit = test_heap_authority_audit(thread_id);
            assert_eq!(audit.registered.len(), nested_owner_count);
            assert_eq!(audit.retired.len(), nested_owner_count);
            assert_eq!(audit.registered, audit.retired);
        }
    }
}

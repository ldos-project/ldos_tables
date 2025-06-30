//! A SPSC table implementation with support for strong and weak observation with both static and dynamic configurability.
//!
//! This is implemented as a lock-free ring buffer with additional head pointers to support strong observers. Strong
//! observers are otherwise identical to the consumer.
//!
//! Weak observers and implemented using a 64-bit state word on each element in the ring buffer. These words store:
//!
//!  1. 1-bit for each weak observer which is set while that observer is reading that element. There are 40 such bits,
//!     allowing for a maximum of 40 weak observers. (This could be expanded, but that seems very unlikely to be
//!     needed.)
//!  2. A valid bit which is set when the matching element in the buffer is valid, and cleared otherwise.
//!  3. A 23-bit generation value used to protect the weak_observer word from ABA issues. 23-bits can roll over in
//!     ~167ms in the worst case. This means that a weak observer would need to be delayed by that amount in the middle
//!     of a read and hit the exact generation from the previous iteration. Even if this does happen, the weak observer
//!     will simply get the data from the wrong time. The data structure will not be corrupted in any way.

// TODO: Implement lazily updated minimum head and tail. This would track the strong observers without the need for
// additional reads on every put. Instead, the reads would only occur when a put fails making the main head jump forward
// when needed. This will only help significantly for longer queues.

// TODO: Implement a multi-consumer and/or multi-producer variant. Potentially with cases for just one or the other if
// those are faster. If there is no advantage to the fully SPSC case, the MPMC case could be the only implementation.

use alloc::{boxed::Box, sync::Arc};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
};
use crossbeam_utils::CachePadded;
use std::{
    any::type_name,
    cell::Cell,
    env,
    marker::PhantomData,
    mem::MaybeUninit,
    sync::{atomic::AtomicU64, Mutex},
    usize,
};

use crate::{
    stubs::yield_thread,
    table_ipc::{
        Consumer, Cursor, Producer, StrongObserver, Table, TableAttachError, WeakObserver,
    },
};

/// A single element (slot for storing a value) in a ring buffer.
#[derive(Debug)]
struct Element<T> {
    /// The data stored in this element of the ring buffer. This is value is initialized if either the valid bit in
    /// `weak_reader_states` is set or this element is between the head (read) and tail (write) indexes of the ring
    /// buffer. This assumes correct synchronization using the various atomic values used by the ring buffer.
    data: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Element<T> {
    fn uninit() -> Element<T> {
        Element {
            data: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

impl<T: Default> Default for Element<T> {
    fn default() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

/// A state word for handling weak observer accesses.
///
/// `WEAK_OBSERVER_WORDS` is 0 or 1 to select if weak observers are supported. A table could theoretically support
/// multiple state words per element to support more than 40 weak observers.
#[derive(Default)]
struct ElementWeakState {
    /// The state for the weak readers on this element. You can think of this as a breakable lock on element, readers
    /// can manipulate it to reliably detect if a write occurred during a read without the writer ever having to wait.
    ///
    /// Bit fields:
    ///
    ///   - 64..41 The 23-bits of the global index immediately after the len_mask. This can be viewed as the
    ///     "generation" as it will always be the number of times the head and tail have moved around the ring (mod
    ///     2^23).
    ///   - 40     The valid bit. If this is not set, the data in this element is currently invalid.
    ///   - 39..0  A bit mask with one bit per weak observer. These bits are set while the observer is reading and
    ///     cleared otherwise. Each weak observer is uniquely assigned one bit.
    ///
    /// The weak observers will:
    ///   - Set their bit using an atomic "or" operation with acquire ordering and store the result. If the valid bit is
    ///     0, abort; the element is in the gap of the buffer that is currently written.
    ///   - Read the element from the buffer.
    ///   - Clear their bit using an atomic "and" operation using acquire-release ordering. (The acquire ordering is
    ///     required so make sure the checks below cannot occur on data as it existed before bit clear.)
    ///   - Check that the "generation" and valid bit are the same as the initial read and this weak observers "reading"
    ///     bit was set. If they are not, abort; the element was written while we were reading it.
    ///
    /// At this point the data read from the buffer is valid and was not written during the read process.
    ///
    /// To make this safe, the writer must:
    ///   - Before it writes an element, swap, with acquire ordering, the new index with a 0 valid bit, and all observer
    ///     bits 0. A swap is used instead of a write to allow acquire ordering.
    ///   - Write to element.
    ///   - After writing data, write, with release ordering, the new index/generation with a 1 valid bid, and all
    ///     observer bits 0.
    ///
    /// NOTE: This uses u64 instead of usize because this is used as multiple bit fields instead of a single count.
    weak_readers: AtomicU64,
}

/// See [`SpscTable::attachment_state`].
struct SpscTableAttachmentState {
    /// A list of strong observer head indexes which are not in use, so they can be allocated to a new strong observer.
    /// When a value is taken from this list, it can safely be used as an index into `strong_observer_heads`.
    free_strong_observer_heads: Vec<usize>,
    /// A list of weak observer bit indexes which are not in use, so they can be allocated to a new weak observer. When
    /// a value is taken from this list, it can safely be used as a bit index in the weak reader state words.
    free_weak_observer_bits: Vec<usize>,
    /// True iff there is already a consumer.
    has_consumer: bool,
    /// True iff there is already a producer
    has_producer: bool,
}

/// A table supporting a producer, a consumer, multiple strong observers, and multiple weak observers. The latter two
/// being optional dynamically. See [`super::spsc`].
///
/// ## Memory usage
///
/// This implementation is performance optimized at the cost of memory. This is mainly in the form of lots of padding to
/// separate atomically accessed data. This includes *128*-byte padding for atomics. This is based on some intel
/// micro-archs operating on pairs of 64-byte cache lines (see the source of [`crossbeam_utils::CachePadded`]).
///
/// **TODO:OPTIMIZATION:** This padding isn't even always a benefit. Should it be optional via a type parameter?
///
/// Enabling weak observers at construction causes an additional padded atomic word to be allocated per element of the
/// ring buffer. This is to guarantee no false sharing between the weak observer state words of different elements *and*
/// no false sharing between the state word and the element data itself.
pub type SpscTable<T> = SpscTableCustom<T>;

/// A statically-customizable table supporting a producer, a consumer, multiple strong observers, and multiple weak
/// observers. The latter two being optional both statically and dynamically. See [`SpscTable`] and [`super::spsc`].
///
/// ## Configurations
///
/// This type takes two configuration parameters:
///  
/// * `STRONG_OBSERVERS` which specifies if strong observers should be supported. If this is `false`, the strong
///   observer checks are omitted statically.
/// * `WEAK_OBSERVERS` which specifies if weak observers should supported, similarly to above.
///
/// Both are `true` by default and should *not* be disabled without benchmarking. They have little to no overhead as
/// long as there are no attached observers, so leaving them enabled provides more dynamic flexibility.
pub struct SpscTableCustom<T, const STRONG_OBSERVERS: bool = true, const WEAK_OBSERVERS: bool = true> {
    // INVARIANTS:
    //
    // * All reading occurs within `[min(heads) % len, tail_index % len)` where `heads` is the set of head indices
    //   including both the consumer (`head_index`) and the observers (`strong_observer_heads`).
    // * All writing occurs within `[tail_index % len, min(heads) % len)`.
    // * All writes occur while the valid bit associated with that element is 0 (if WEAK_OBSERVERS is set of course)
    //
    // Notable states:
    // * An empty queue has head_index == tail_index; interpreted as, there are no values between the head and the tail
    //   to there are no values to read.
    // * A full queue has head_index == tail_index + 1; interpreted as, there are len-1 value between the head and the
    //   tail (mod len).
    //
    // All indexes are stored as global indexes (indexes into an abstract infinite buffer). This makes ABA problems
    // impossible.

    // # Hot-path data that is constant over the life of the table, so can be safely shared between all threads. The
    // pointers to and sizes of the various buffers are constant, even though the buffer contents are not.
    /// The number of elements in the buffer. This code uses the term "slot" to refer to specific indexes into the
    /// buffer, to distinguish them from global indexes into the abstract infinite buffer.
    len: usize,
    /// The number of bits that make up the index into the buffer. E.i., log2(buffer size).
    len_bits: u32,
    /// The mask to get a slot in the buffer from a global index.
    len_mask: usize,
    /// The internal data buffer.
    buffer: Box<[Element<T>]>,
    /// The weak observer state words for each slot in the buffer. This will be `None` if this table was not created
    /// with support weak observers (dynamically or statically).
    weak_observer_states: Option<Box<[CachePadded<ElementWeakState>]>>,
    /// The heads used by each strong observer.
    strong_observer_heads: Box<[CachePadded<AtomicUsize>]>,

    // # Hot-path data that is read and written by different threads and so should avoid false sharing.
    /// The index from which the next element will be read.
    head_index: CachePadded<AtomicUsize>,
    /// The index of the next element to write in the buffer.
    tail_index: CachePadded<AtomicUsize>,

    // # Cold-path data that is accessed infrequently and for which false sharing is acceptable.
    /// The state used to attach to this table.
    attachment_state: Mutex<SpscTableAttachmentState>,
}

// SAFETY: SpscTable's implementation guarantees that elements of buffer are never accessed unsafely from more than one
// thread at a time.
unsafe impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Sync
    for SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
}
unsafe impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Send
    for SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
}

impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool>
    SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    /// Create a new [`SpscTable`] with the specified number of strong and weak observer slots.
    pub fn new(size: usize, max_strong_observers: usize, max_weak_observers: usize) -> Arc<Self> {
        if !STRONG_OBSERVERS {
            assert_eq!(max_strong_observers, 0);
        }
        assert!(max_weak_observers <= Self::MAX_WEAK_OBSERVERS);
        if !WEAK_OBSERVERS {
            assert_eq!(max_weak_observers, 0);
        }
        assert!(size.is_power_of_two());

        let weak_reader_states = if max_weak_observers > 0 {
            Some((0..size).map(|_| Default::default()).collect())
        } else {
            None
        };
        let ret = SpscTableCustom {
            buffer: (0..size).map(|_| Element::uninit()).collect(),
            head_index: Default::default(),
            strong_observer_heads: (0..max_strong_observers)
                .map(|_| CachePadded::new(AtomicUsize::new(usize::MAX)))
                .collect(),
            tail_index: Default::default(),
            len: size,
            len_mask: size - 1,
            len_bits: size.trailing_zeros(),
            weak_observer_states: weak_reader_states,
            attachment_state: Mutex::new(SpscTableAttachmentState {
                free_strong_observer_heads: (0..max_strong_observers).collect(),
                free_weak_observer_bits: (0..max_weak_observers).collect(),
                has_consumer: false,
                has_producer: false,
            }),
        };
        if cfg!(debug_assertions) && env::var("LDOS_DUMP_DATA_SIZES").is_ok_and(|s| s.len() > 0) {
            println!("Sizes:");
            println!("  overall: {}", std::mem::size_of_val(&ret));
            println!("  buffer: {}", std::mem::size_of_val(ret.buffer.as_ref()));
            println!("    element: {}", std::mem::size_of_val(&ret.buffer[0]));
            // println!("    data: {}", std::mem::size_of_val(&ret.buffer[0].data));
            println!(
                "  strong_observer_heads: {}",
                std::mem::size_of_val(ret.strong_observer_heads.as_ref())
            );
            if let Some(weak_reader_states) = ret.weak_observer_states.as_ref() {
                println!(
                    "  weak_reader_states: {}",
                    std::mem::size_of_val(weak_reader_states.as_ref())
                );
            }
        }
        Arc::new(ret)
    }

    /// Compute the given `index % buffer.len()`. Internally, this uses a bit-mask computed during construction of the
    /// table.
    fn mod_len(&self, index: usize) -> usize {
        index & self.len_mask
    }

    /// Get the generation (number of times around the buffer) from an index. This is used to protect against ABA issues
    /// in weak observers.
    fn get_generation_for_index(&self, index: usize) -> u64 {
        (index >> self.len_bits) as u64
    }

    /// SAFETY: Must only be called from a single thread at a time. E.i., no object calling this can be Sync, but it may
    /// be Send.
    #[inline(always)]
    unsafe fn try_put(&self, data: T) -> Option<T>
    where
        T: Send,
    {
        // Get the tail index. We can use relaxed ordering since there can't be any other thread writing this value.
        let current_tail = self.tail_index.load(Ordering::Relaxed);
        let current_tail_slot = self.mod_len(current_tail);
        // Get the head index. This must be acquire ordering to guarantee writes into the buffer slot below cannot be
        // reordered above this read making them potentially come before a previous read in the consumer.
        let current_head = self.head_index.load(Ordering::Acquire);
        let current_head_slot = self.mod_len(current_head);

        // If moving the tail forward would cause it to overrun the head (invalidation the empty/full state
        // distinction), we fail. Once this check is completed it can never be invalidated because head can only move
        // forward. This checks both the consumer head as well as strong observer heads.
        //
        // TODO:OPTIMIZATION: It would be possible to maintain a lazily update a minimum of all the heads. This could be
        // faster since it can amortize the cost of atomic reads which may contend with consumers and observers. See
        // https://www.linuxjournal.com/content/lock-free-multi-producer-multi-consumer-queue-ring-buffer.
        let next_tail = self.mod_len(current_tail + 1);
        if next_tail == current_head_slot
            || (STRONG_OBSERVERS
                && self.strong_observer_heads.iter().any(|h| {
                    let current_h = h.load(Ordering::Acquire);
                    current_h != usize::MAX && next_tail == self.mod_len(current_h)
                }))
        {
            // The buffer is full. Fail by passing the data back to the caller.
            return Some(data);
        }

        let slot_cell = &self.buffer[current_tail_slot];

        // The generation shifted as required by the weak observer state word.
        let new_generation_field =
            self.get_generation_for_index(current_tail) << (Self::MAX_WEAK_OBSERVERS + 1);
        if WEAK_OBSERVERS && let Some(weak_reader_states) = &self.weak_observer_states {
            // Clear the valid and weak observer specific "reading" bits and update the generation. This uses
            // acquire-release ordering to guarantee that any writes in the buffer below are ordered after any read
            // occurring in [`Self::weak_observe`]. This guarantees that if the state update in that function succeeds
            // then the read was correct.
            let _pre_readers = weak_reader_states[current_tail_slot]
                .weak_readers
                .swap(new_generation_field, Ordering::AcqRel);
        }

        // SAFETY: This slot is between current_tail_slot and current_head_slot, so it cannot be read. We are the only
        // writer, so no other thread can be doing this. Moving data into the buffer is safe because T is Send.
        unsafe {
            (&mut *slot_cell.data.get()).write(data);
        }

        if WEAK_OBSERVERS && let Some(weak_reader_states) = &self.weak_observer_states {
            // Set the valid bit (and clear any stray "reading" bits). This must have release ordering to make sure the
            // write above completes before the valid bit is observed.
            weak_reader_states[current_tail_slot]
                .weak_readers
                .store(new_generation_field | Self::VALID_MASK, Ordering::Release);
        }

        // Increment tail index. We are the only writer and the consumer will only assume this value never decreases.
        // Release ordering guarantees that observing the index means data was fully written.
        self.tail_index.store(current_tail + 1, Ordering::Release);

        return None;
    }

    /// Try to consume a value. Returning `None` if there is none available.
    ///
    /// SAFETY: Must only be called from a single thread at a time. E.i., no object calling this can be Sync, but it may
    /// be Send.
    #[inline(always)]
    unsafe fn try_take(&self) -> Option<T>
    where
        T: Copy + Send,
    {
        self.try_take_for_head(&self.head_index)
    }

    /// Try to observe a value. Returning `None` if the observer has reached the most recent value in the queue.
    ///
    /// SAFETY: Must only be called from a single thread at a time with a given observer index. E.i., no object calling
    /// this can be Sync, but it may be Send.
    #[inline(always)]
    unsafe fn try_strong_observe(&self, observer_index: usize) -> Option<T>
    where
        T: Copy + Send,
    {
        let head = &self.strong_observer_heads[observer_index];
        self.try_take_for_head(head)
    }

    /// The implementation of both `try_take` and `try_strong_observe`. It takes a reference to the atomic index into
    /// the buffer.
    ///
    /// There is no real distinction between consume and observe. However having them split guarantees that the consumer
    /// pointer doesn't require a pointer indirection. This isn't required at all, but might make performance more
    /// predictable for the most important reader.    
    fn try_take_for_head(&self, head: &AtomicUsize) -> Option<T>
    where
        T: Copy + Send,
    {
        // Read the head. This is relaxed because there is no other writer to the head.
        let current_head = head.load(Ordering::Relaxed);
        let current_head_slot = self.mod_len(current_head);
        // Read the tail. This must be acquire ordering to guarantee that the read from the buffer below will observe
        // fully written data.
        let current_tail = self.tail_index.load(Ordering::Acquire);
        let current_tail_slot = self.mod_len(current_tail);

        // Check for an empty buffer.
        if current_head_slot == current_tail_slot {
            debug_assert_eq!(current_head, current_tail);
            return None;
        }

        let slot_cell = &self.buffer[current_head_slot];

        // SAFETY: There is no other reader, and this slot is between current_head_slot and current_tail_slot. Reading
        // the bytes directly is safe because T is Copy. Using that data on this thread is safe because T is Send.
        let data = unsafe { std::ptr::read(slot_cell.data.get()).assume_init() };

        // Update the head. This must be release ordering so guarantee that the above read completes before the new head
        // can be observer by a producer.
        head.store(current_head + 1, Ordering::Release);

        Some(data)
    }

    const MAX_WEAK_OBSERVERS: usize = 40;
    const VALID_MASK: u64 = 0x1 << 40;

    /// Get the valid bit in a weak observer state word.
    fn get_valid_bit(v: u64) -> bool {
        v & Self::VALID_MASK == Self::VALID_MASK
    }

    /// Get the generation from a weak observer state word.
    fn get_generation(v: u64) -> u64 {
        // Check that this matches the documentation. If this changes, also change the documentation on
        // ElementWeakState.
        debug_assert!(Self::MAX_WEAK_OBSERVERS + 1 == 41);
        v >> (Self::MAX_WEAK_OBSERVERS + 1)
    }

    /// Observe a value in the queue based on a cursor. If value isn't available return `None`. This can happen if
    /// either the value is outside the current range covered by the buffer or the value is overwritten while being
    /// observed. This is all best effort and there are no specific guarantees about the availability of values.
    ///
    /// SAFETY: Must only be called from a single thread at a time with a given observer index. E.i., no object calling
    /// this can be Sync, but it may be Send.
    unsafe fn weak_observe(&self, observer_index: usize, cursor: Cursor) -> Option<T>
    where
        T: Copy + Send,
    {
        debug_assert!(observer_index < Self::MAX_WEAK_OBSERVERS);
        assert!(WEAK_OBSERVERS);
        let weak_observer_states = self.weak_observer_states.as_ref().unwrap();
        let Cursor(index) = cursor;

        let slot = self.mod_len(index);

        // Get the cell and weak observer state for the slot where the would be stored.
        let slot_cell = &self.buffer[slot];
        let slot_state = &weak_observer_states[slot].weak_readers;

        // The mask for the observer bit in the state.
        let mask = 0x1 << observer_index;

        // Check that the requested value is within the valid range of the buffer. This is needed to reduce range of
        // values that the generation needs to protect us from. Without this, the ABA "window" would be between getting
        // the cursor and calling this function. With this check, the window is from this check until the final
        // generation check at the bottom of this function.
        //
        // This read is relaxed ordering since it only narrows an ABA window and does not need to be precise.
        let tail_index = self.tail_index.load(Ordering::Relaxed);
        if index < tail_index.saturating_sub(self.len) || tail_index < index {
            return None;
        }

        // This is acquire ordering to guarantee that the read below comes after any setting of the valid bit and
        // generation.
        let state_before_read = slot_state.fetch_or(mask, Ordering::Acquire);
        let state_generation_before_read = Self::get_generation(state_before_read);
        // Check that the slot is valid and has the generation matching the index we want to read.
        //
        // Note: our own bit may already be set if we failed a read in this slot since the slot was reused. This can
        // happen if there are reads in multiple generations.
        if !Self::get_valid_bit(state_before_read)
            || state_generation_before_read != self.get_generation_for_index(index)
        {
            // We do not clear our own bit since we do not need to and doing so could contend with other atomics.
            return None;
        }

        // SAFETY: There may be concurrent writes, but we do not do not use the read data. MaybeUninit is required by
        // the rust safety model because it tells the compiler that the bytes of data may not actually be a valid object
        // of type T (this can occur due to concurrent writes).
        let data = unsafe { std::ptr::read(slot_cell.data.get()) };

        // Clear our bit; this isn't required from an information transfer perspective, but provides for required
        // ordering. This is acquire-release ordering to guarantee that the read stays before any writes to this slot.
        // The producer will perform a matching acquire-release operation.
        let state_after_read = slot_state.fetch_and(!mask, Ordering::AcqRel);
        if !Self::get_valid_bit(state_after_read)
            || Self::get_generation(state_after_read) != state_generation_before_read
        {
            return None;
        }
        assert_eq!(state_after_read & mask, mask);

        // SAFETY: We have check that the data was not modified concurrently with the read, so the value is a valid copy
        // of the data in the buffer.
        Some(unsafe { data.assume_init() })
    }
}

impl<T: Copy + Sync + Send, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Table<T>
    for SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    type Producer = SpscProducer<T, STRONG_OBSERVERS, WEAK_OBSERVERS>;
    type Consumer = SpscConsumer<T, STRONG_OBSERVERS, WEAK_OBSERVERS>;
    type StrongObserver = SpscStrongObserver<T, STRONG_OBSERVERS, WEAK_OBSERVERS>;
    type WeakObserver = SpscWeakObserver<T, STRONG_OBSERVERS, WEAK_OBSERVERS>;

    fn attach_producer(self: &Arc<Self>) -> Result<Self::Producer, TableAttachError> {
        let mut state = self.attachment_state.lock().unwrap();
        if state.has_producer {
            Err(TableAttachError::AllocationFailed {
                table_type: type_name::<Self>().to_owned(),
                reason: "producer already attached".to_owned(),
            })
        } else {
            state.has_producer = true;
            Ok(SpscProducer {
                table: self.clone(),
                _phantom: PhantomData,
            })
        }
    }

    fn attach_consumer(self: &Arc<Self>) -> Result<Self::Consumer, TableAttachError> {
        let mut state = self.attachment_state.lock().unwrap();
        if state.has_consumer {
            Err(TableAttachError::AllocationFailed {
                table_type: type_name::<Self>().to_owned(),
                reason: "consumer already attached".to_owned(),
            })
        } else {
            state.has_consumer = true;
            Ok(SpscConsumer {
                table: self.clone(),
                _phantom: PhantomData,
            })
        }
    }

    fn attach_strong_observer(self: &Arc<Self>) -> Result<Self::StrongObserver, TableAttachError> {
        let mut state = self.attachment_state.lock().unwrap();
        let free_list = &mut state.free_strong_observer_heads;
        if let Some(slot) = free_list.pop() {
            self.strong_observer_heads[slot]
                .store(self.head_index.load(Ordering::Relaxed), Ordering::Release);
            Ok(SpscStrongObserver {
                table: self.clone(),
                observer_index: slot,
                _phantom: PhantomData,
            })
        } else {
            Err(TableAttachError::AllocationFailed {
                table_type: type_name::<Self>().to_owned(),
                reason: format!(
                    "only {} strong observers supported",
                    self.strong_observer_heads.len()
                ),
            })
        }
    }

    fn attach_weak_observer(self: &Arc<Self>) -> Result<Self::WeakObserver, TableAttachError> {
        let mut state = self.attachment_state.lock().unwrap();
        let free_list = &mut state.free_weak_observer_bits;
        if let Some(slot) = free_list.pop() {
            Ok(SpscWeakObserver {
                table: self.clone(),
                observer_index: slot,
                _phantom: PhantomData,
            })
        } else {
            Err(TableAttachError::AllocationFailed {
                table_type: type_name::<Self>().to_owned(),
                reason: "insufficient observers supported".to_owned(),
            })
        }
    }
}

/// The producer handle for [`SpscTable`].
pub struct SpscProducer<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> {
    table: Arc<SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>>,
    // Make this Send, but not Sync
    _phantom: PhantomData<Cell<()>>,
}

// Sanity checks because `PhantomData<Cell<()>>` does not overtly specify the expected auto-trait implementations.
static_assertions::assert_impl_any!(SpscProducer<(), true, true>: Send);
static_assertions::assert_not_impl_any!(SpscProducer<(), true, true>: Sync);

impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Drop
    for SpscProducer<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn drop(&mut self) {
        let mut state = self.table.attachment_state.lock().unwrap();
        state.has_producer = false;
    }
}

impl<T: Copy + Send, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Producer<T>
    for SpscProducer<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn put(&self, data: T) {
        let mut i = 0;
        let mut d = Some(data);
        // SAFETY: SpscProducer is Send, but not Sync, so this can only ever be called from a single thread at a time.
        while let Some(v) = unsafe { self.table.try_put(d.take().unwrap()) } {
            d = Some(v);
            if i > 1000 {
                yield_thread();
            }
            i += 1;
        }
    }
    #[inline(never)]
    fn try_put(&self, data: T) -> Option<T> {
        // SAFETY: SpscProducer is Send, but not Sync, so this can only ever be called from a single thread at a time.
        unsafe { self.table.try_put(data) }
    }
}

/// The consumer handle for [`SpscTable`].
pub struct SpscConsumer<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> {
    table: Arc<SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>>,
    // Make this Send, but not Sync
    _phantom: PhantomData<Cell<()>>,
}

// Sanity checks because `PhantomData<Cell<()>>` does not overtly specify the expected auto-trait implementations.
static_assertions::assert_impl_any!(SpscConsumer<(), true, true>: Send);
static_assertions::assert_not_impl_any!(SpscConsumer<(), true, true>: Sync);

impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Drop
    for SpscConsumer<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn drop(&mut self) {
        let mut state = self.table.attachment_state.lock().unwrap();
        state.has_consumer = false;
    }
}

impl<T: Copy + Send, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Consumer<T>
    for SpscConsumer<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn take(&self) -> T {
        let mut i = 0;
        loop {
            // SAFETY: SpscConsumer is Send, but not Sync, so this can only ever be called from a single thread at a time.
            if let Some(v) = unsafe { self.table.try_take() } {
                return v;
            }
            if i > 1000 {
                yield_thread();
            }
            i += 1;
        }
    }

    fn try_take(&self) -> Option<T> {
        // SAFETY: SpscConsumer is Send, but not Sync, so this can only ever be called from a single thread at a time.
        unsafe { self.table.try_take() }
    }
}

/// The strong-observer handle for [`SpscTable`].
pub struct SpscStrongObserver<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> {
    table: Arc<SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>>,
    observer_index: usize,
    // Make this Send, but not Sync
    _phantom: PhantomData<Cell<()>>,
}

// Sanity checks because `PhantomData<Cell<()>>` does not overtly specify the expected auto-trait implementations.
static_assertions::assert_impl_any!(SpscStrongObserver<(), true, true>: Send);
static_assertions::assert_not_impl_any!(SpscStrongObserver<(), true, true>: Sync);

impl<T: Copy + Send, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool>
    StrongObserver<T> for SpscStrongObserver<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn strong_observe(&self) -> T {
        let mut i = 0;
        loop {
            // SAFETY: SpscConsumer is Send, but not Sync, so this can only ever be called from a single thread at a time.
            if let Some(v) = self.try_strong_observe() {
                return v;
            }
            if i > 1000 {
                yield_thread();
            }
            i += 1;
        }
    }

    fn try_strong_observe(&self) -> Option<T> {
        // SAFETY: SpscConsumer is Send, but not Sync, so this can only ever be called from a single thread at a time.
        unsafe { self.table.try_strong_observe(self.observer_index) }
    }
}

impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Drop
    for SpscStrongObserver<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn drop(&mut self) {
        let mut state = self.table.attachment_state.lock().unwrap();
        let free_list = &mut state.free_strong_observer_heads;
        self.table.strong_observer_heads[self.observer_index].store(usize::MAX, Ordering::Release);
        free_list.push(self.observer_index);
    }
}

/// The weak-observer handle for [`SpscTable`].
pub struct SpscWeakObserver<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> {
    table: Arc<SpscTableCustom<T, STRONG_OBSERVERS, WEAK_OBSERVERS>>,
    observer_index: usize,
    // Make this Send, but not Sync
    _phantom: PhantomData<Cell<()>>,
}

// Sanity checks because `PhantomData<Cell<()>>` does not overtly specify the expected auto-trait implementations.
static_assertions::assert_impl_any!(SpscWeakObserver<(), true, true>: Send);
static_assertions::assert_not_impl_any!(SpscWeakObserver<(), true, true>: Sync);

impl<T, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> Drop
    for SpscWeakObserver<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn drop(&mut self) {
        let mut state = self.table.attachment_state.lock().unwrap();
        let free_list = &mut state.free_weak_observer_bits;
        free_list.push(self.observer_index);
    }
}

impl<T: Copy + Send, const STRONG_OBSERVERS: bool, const WEAK_OBSERVERS: bool> WeakObserver<T>
    for SpscWeakObserver<T, STRONG_OBSERVERS, WEAK_OBSERVERS>
{
    fn weak_observe(&self, cursor: Cursor) -> Option<T> {
        // SAFETY: SpscConsumer is Send, but not Sync, so this can only ever be called from a single thread at a time.
        unsafe { self.table.weak_observe(self.observer_index, cursor) }
    }

    fn recent_cursor(&self) -> Cursor {
        // This estimates the most recent element by looking at the tail (which is the next slot to write) and subtracting 1.
        Cursor(
            self.table
                .tail_index
                .load(Ordering::Acquire)
                .saturating_sub(1),
        )
    }

    fn oldest_cursor(&self) -> Cursor {
        let Cursor(i) = self.recent_cursor();
        // Return the most recent - the buffer size or zero if the buffer isn't full yet.
        if i < self.table.buffer.len() {
            Cursor(0)
        } else {
            Cursor(i - (self.table.buffer.len() - 1))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::table_ipc::generic_test;

    #[test]
    fn test_produce_consume() {
        generic_test::test_produce_consume(SpscTableCustom::<_>::new(2, 0, 0));
    }

    #[test]
    fn test_produce_strong_observe() {
        generic_test::test_produce_strong_observe(SpscTableCustom::<_>::new(2, 1, 0));
    }

    #[test]
    fn test_produce_weak_observe() {
        generic_test::test_produce_weak_observe(SpscTableCustom::<_>::new(2, 0, 1));
    }


    #[test]
    fn test_all() {
        generic_test::test_produce_consume(SpscTableCustom::<_>::new(2, 1, 1));
        generic_test::test_produce_strong_observe(SpscTableCustom::<_>::new(2, 1, 1));
        generic_test::test_produce_weak_observe(SpscTableCustom::<_>::new(2, 1, 1));
    }
}

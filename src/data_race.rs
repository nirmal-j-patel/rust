//! Implementation of a data-race detector using Lamport Timestamps / Vector-clocks
//! based on the Dyamic Race Detection for C++:
//! https://www.doc.ic.ac.uk/~afd/homepages/papers/pdfs/2017/POPL.pdf
//! which does not report false-positives when fences are used, and gives better
//! accuracy in presence of read-modify-write operations.
//!
//! This does not explore weak memory orders and so can still miss data-races
//! but should not report false-positives
//!
//! Data-race definiton from(https://en.cppreference.com/w/cpp/language/memory_model#Threads_and_data_races):
//! a data race occurs between two memory accesses if they are on different threads, at least one operation
//! is non-atomic, at least one operation is a write and neither access happens-before the other. Read the link
//! for full definition.
//! 
//! This re-uses vector indexes for threads that are known to be unable to report data-races, this is valid
//! because it only re-uses vector indexes once all currently-active (not-terminated) threads have an internal
//! vector clock that happens-after the join operation of the candidate thread. Threads that have not been joined
//! on are not considered. Since the thread's vector clock will only increase and a data-race implies that
//! there is some index x where clock[x] > thread_clock, when this is true clock[candidate-idx] > thread_clock
//! can never hold and hence a data-race can never be reported in that vector index again.
//! This means that the thread-index can be safely re-used, starting on the next timestamp for the newly created
//! thread.
//!
//! The sequentially consistant ordering corresponds to the ordering that the threads
//! are currently scheduled, this means that the data-race detector has no additional
//! logic for sequentially consistent accesses at the moment since they are indistinguishable
//! from acquire/release operations. If weak memory orderings are explored then this
//! may need to change or be updated accordingly.
//!
//! FIXME:
//! currently we have our own local copy of the currently active thread index and names, this is due
//! in part to the inability to access the current location of threads.active_thread inside the AllocExtra
//! read, write and deallocate functions and should be cleaned up in the future.

use std::{
    fmt::Debug, rc::Rc,
    cell::{Cell, RefCell, Ref, RefMut}, mem
};

use rustc_index::vec::{Idx, IndexVec};
use rustc_target::abi::Size;
use rustc_middle::{mir, ty::layout::TyAndLayout};
use rustc_data_structures::fx::{FxHashSet, FxHashMap};

use crate::{
    MiriEvalContext, MiriEvalContextExt,
    ThreadId, Tag, RangeMap,
    InterpResult, Pointer, ScalarMaybeUninit,
    MPlaceTy, OpTy, MemPlaceMeta, ImmTy, Immediate,
    VClock, VSmallClockMap, VectorIdx, VTimestamp
};

pub type AllocExtra = VClockAlloc;
pub type MemoryExtra = Rc<GlobalState>;

/// Valid atomic read-write operations, alias of atomic::Ordering (not non-exhaustive).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AtomicRwOp {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

/// Valid atomic read operations, subset of atomic::Ordering.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AtomicReadOp {
    Relaxed,
    Acquire,
    SeqCst,
}

/// Valid atomic write operations, subset of atomic::Ordering.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AtomicWriteOp {
    Relaxed,
    Release,
    SeqCst,
}


/// Valid atomic fence operations, subset of atomic::Ordering.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AtomicFenceOp {
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}



/// The current set of vector clocks describing the state
/// of a thread, contains the happens-before clock and
/// additional metadata to model atomic fence operations.
#[derive(Clone, Default, Debug)]
struct ThreadClockSet {

    /// The increasing clock representing timestamps
    /// that happen-before this thread.
    clock: VClock,

    /// The set of timestamps that will happen-before this
    /// thread once it performs an acquire fence.
    fence_acquire: VClock,

    /// The last timesamp of happens-before relations that
    /// have been released by this thread by a fence.
    fence_release: VClock,
}


impl ThreadClockSet {

    /// Apply the effects of a release fence to this
    /// set of thread vector clocks.
    #[inline]
    fn apply_release_fence(&mut self) {
        self.fence_release.clone_from(&self.clock);
    }

    /// Apply the effects of a acquire fence to this
    /// set of thread vector clocks.
    #[inline]
    fn apply_acquire_fence(&mut self) {
        self.clock.join(&self.fence_acquire);
    }

    /// Increment the happens-before clock at a
    /// known index.
    #[inline]
    fn increment_clock(&mut self, index: VectorIdx) {
        self.clock.increment_index(index);
    }

    /// Join the happens-before clock with that of
    /// another thread, used to model thread join
    /// operations.
    fn join_with(&mut self, other: &ThreadClockSet) {
        self.clock.join(&other.clock);
    }
}


/// Error returned by finding a data race
/// should be elaborated upon.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DataRace;

/// Externally stored memory cell clocks
/// explicitly to reduce memory usage for the
/// common case where no atomic operations
/// exists on the memory cell.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
struct AtomicMemoryCellClocks {

    /// The clock-vector of the timestamp of the last atomic
    /// read operation performed by each thread.
    /// This detects potential data-races between atomic read
    /// and non-atomic write operations.
    read_vector: VClock,

    /// The clock-vector of the timestamp of the last atomic
    /// write operation performed by each thread.
    /// This detects potential data-races between atomic write
    /// and non-atomic read or write operations.
    write_vector: VClock,

    /// Synchronization vector for acquire-release semantics
    /// contains the vector of timestamps that will
    /// happen-before a thread if an acquire-load is 
    /// performed on the data.
    sync_vector: VClock,

    /// The Hash-Map of all threads for which a release
    /// sequence exists in the memory cell, required
    /// since read-modify-write operations do not
    /// invalidate existing release sequences.
    /// See page 6 of linked paper.
    release_sequences: VSmallClockMap,
}

/// Memory Cell vector clock metadata
/// for data-race detection.
#[derive(Clone, PartialEq, Eq, Debug)]
struct MemoryCellClocks {

    /// The vector-clock timestamp of the last write
    /// corresponding to the writing threads timestamp.
    write: VTimestamp,

    /// The identifier of the vector index, corresponding to a thread
    /// that performed the last write operation.
    write_index: VectorIdx,

    /// The vector-clock of the timestamp of the last read operation
    /// performed by a thread since the last write operation occured.
    read: VClock,

    /// Atomic acquire & release sequence tracking clocks.
    /// For non-atomic memory in the common case this
    /// value is set to None.
    atomic_ops: Option<Box<AtomicMemoryCellClocks>>,
}


/// Create a default memory cell clocks instance
/// for uninitialized memory.
impl Default for MemoryCellClocks {
    fn default() -> Self {
        MemoryCellClocks {
            read: VClock::default(),
            write: 0,
            write_index: VectorIdx::MAX_INDEX,
            atomic_ops: None
        }
    }
}


impl MemoryCellClocks {

    /// Load the internal atomic memory cells if they exist.
    #[inline]
    fn atomic(&self) -> Option<&AtomicMemoryCellClocks> {
        match &self.atomic_ops {
            Some(op) => Some(&*op),
            None => None
        }
    }

    /// Load or create the internal atomic memory metadata
    /// if it does not exist.
    #[inline]
    fn atomic_mut(&mut self) -> &mut AtomicMemoryCellClocks {
        self.atomic_ops.get_or_insert_with(Default::default)
    }

    /// Update memory cell data-race tracking for atomic
    /// load acquire semantics, is a no-op if this memory was
    /// not used previously as atomic memory.
    fn load_acquire(&mut self, clocks: &mut ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        self.atomic_read_detect(clocks, index)?;
        if let Some(atomic) = self.atomic() {
            clocks.clock.join(&atomic.sync_vector);
        }
        Ok(())
    }

    /// Update memory cell data-race tracking for atomic
    /// load relaxed semantics, is a no-op if this memory was
    /// not used previously as atomic memory.
    fn load_relaxed(&mut self, clocks: &mut ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        self.atomic_read_detect(clocks, index)?;
        if let Some(atomic) = self.atomic() {
            clocks.fence_acquire.join(&atomic.sync_vector);
        }
        Ok(())
    }


    /// Update the memory cell data-race tracking for atomic
    /// store release semantics.
    fn store_release(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        self.atomic_write_detect(clocks, index)?;
        let atomic = self.atomic_mut();
        atomic.sync_vector.clone_from(&clocks.clock);
        atomic.release_sequences.clear();
        atomic.release_sequences.insert(index, &clocks.clock);
        Ok(())
    }

    /// Update the memory cell data-race tracking for atomic
    /// store relaxed semantics.
    fn store_relaxed(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        self.atomic_write_detect(clocks, index)?;
        let atomic = self.atomic_mut();
        atomic.sync_vector.clone_from(&clocks.fence_release);
        if let Some(release) = atomic.release_sequences.get(index) {
            atomic.sync_vector.join(release);
        }
        atomic.release_sequences.retain_index(index);
        Ok(())
    }

    /// Update the memory cell data-race tracking for atomic
    /// store release semantics for RMW operations.
    fn rmw_release(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        self.atomic_write_detect(clocks, index)?;
        let atomic = self.atomic_mut();
        atomic.sync_vector.join(&clocks.clock);
        atomic.release_sequences.insert(index, &clocks.clock);
        Ok(())
    }

    /// Update the memory cell data-race tracking for atomic
    /// store relaxed semantics for RMW operations.
    fn rmw_relaxed(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        self.atomic_write_detect(clocks, index)?;
        let atomic = self.atomic_mut();
        atomic.sync_vector.join(&clocks.fence_release);
        Ok(())
    }
    
    /// Detect data-races with an atomic read, caused by a non-atomic write that does
    /// not happen-before the atomic-read.
    fn atomic_read_detect(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        log::trace!("Atomic read with vectors: {:#?} :: {:#?}", self, clocks);
        if self.write <= clocks.clock[self.write_index] {
            let atomic = self.atomic_mut();
            atomic.read_vector.set_at_index(&clocks.clock, index);
            Ok(())
        } else {
            Err(DataRace)
        }
    }

    /// Detect data-races with an atomic write, either with a non-atomic read or with
    /// a non-atomic write.
    fn atomic_write_detect(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        log::trace!("Atomic write with vectors: {:#?} :: {:#?}", self, clocks);
        if self.write <= clocks.clock[self.write_index] && self.read <= clocks.clock {
            let atomic = self.atomic_mut();
            atomic.write_vector.set_at_index(&clocks.clock, index);
            Ok(())
        } else {
            Err(DataRace)
        }
    }

    /// Detect races for non-atomic read operations at the current memory cell
    /// returns true if a data-race is detected.
    fn read_race_detect(&mut self, clocks: &ThreadClockSet, index: VectorIdx) -> Result<(), DataRace> {
        log::trace!("Unsynchronized read with vectors: {:#?} :: {:#?}", self, clocks);
        if self.write <= clocks.clock[self.write_index] {
            let race_free = if let Some(atomic) = self.atomic() {
                atomic.write_vector <= clocks.clock
            } else {
                true
            };
            if race_free {
                self.read.set_at_index(&clocks.clock, index);
                Ok(())
            } else {
                Err(DataRace)
            }
        } else {
            Err(DataRace)
        }
    }

    /// Detect races for non-atomic write operations at the current memory cell
    /// returns true if a data-race is detected.
    fn write_race_detect(&mut self, clocks: &ThreadClockSet, index: VectorIdx)  -> Result<(), DataRace> {
        log::trace!("Unsynchronized write with vectors: {:#?} :: {:#?}", self, clocks);
        if self.write <= clocks.clock[self.write_index] && self.read <= clocks.clock {
            let race_free = if let Some(atomic) = self.atomic() {
                atomic.write_vector <= clocks.clock && atomic.read_vector <= clocks.clock
            } else {
                true
            };
            if race_free {
                self.write = clocks.clock[index];
                self.write_index = index;
                self.read.set_zero_vector();
                Ok(())
            } else {
                Err(DataRace)
            }
        } else {
            Err(DataRace)
        }
    }
}


/// Evaluation context extensions.
impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: MiriEvalContextExt<'mir, 'tcx> {

    /// Atomic variant of read_scalar_at_offset.
    fn read_scalar_at_offset_atomic(
        &self,
        op: OpTy<'tcx, Tag>,
        offset: u64,
        layout: TyAndLayout<'tcx>,
        atomic: AtomicReadOp
    ) -> InterpResult<'tcx, ScalarMaybeUninit<Tag>> {
        let this = self.eval_context_ref();
        let op_place = this.deref_operand(op)?;
        let offset = Size::from_bytes(offset);

        // Ensure that the following read at an offset is within bounds.
        assert!(op_place.layout.size >= offset + layout.size);
        let value_place = op_place.offset(offset, MemPlaceMeta::None, layout, this)?;
        this.read_scalar_atomic(value_place, atomic)
    }

    /// Atomic variant of write_scalar_at_offset.
    fn write_scalar_at_offset_atomic(
        &mut self,
        op: OpTy<'tcx, Tag>,
        offset: u64,
        value: impl Into<ScalarMaybeUninit<Tag>>,
        layout: TyAndLayout<'tcx>,
        atomic: AtomicWriteOp
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let op_place = this.deref_operand(op)?;
        let offset = Size::from_bytes(offset);

        // Ensure that the following read at an offset is within bounds.
        assert!(op_place.layout.size >= offset + layout.size);
        let value_place = op_place.offset(offset, MemPlaceMeta::None, layout, this)?;
        this.write_scalar_atomic(value.into(), value_place, atomic)
    }

    /// Perform an atomic read operation at the memory location.
    fn read_scalar_atomic(
        &self, place: MPlaceTy<'tcx, Tag>, atomic: AtomicReadOp
    ) -> InterpResult<'tcx, ScalarMaybeUninit<Tag>> {
        let this = self.eval_context_ref();
        let scalar = this.allow_data_races_ref(move |this| {
            this.read_scalar(place.into())
        })?;
        self.validate_atomic_load(place, atomic)?;
        Ok(scalar)
    }

    /// Perform an atomic write operation at the memory location.
    fn write_scalar_atomic(
        &mut self, val: ScalarMaybeUninit<Tag>, dest: MPlaceTy<'tcx, Tag>,
        atomic: AtomicWriteOp
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.allow_data_races_mut(move |this| {
            this.write_scalar(val, dest.into())
        })?;
        self.validate_atomic_store(dest, atomic)
    }

    /// Perform a atomic operation on a memory location.
    fn atomic_op_immediate(
        &mut self,
        place: MPlaceTy<'tcx, Tag>, rhs: ImmTy<'tcx, Tag>,
        op: mir::BinOp, neg: bool, atomic: AtomicRwOp
    ) -> InterpResult<'tcx, ImmTy<'tcx, Tag>> {
        let this = self.eval_context_mut();

        let old = this.allow_data_races_mut(|this| {
            this.read_immediate(place. into())
        })?;        

        // Atomics wrap around on overflow.
        let val = this.binary_op(op, old, rhs)?;
        let val = if neg { this.unary_op(mir::UnOp::Not, val)? } else { val };
        this.allow_data_races_mut(|this| {
            this.write_immediate(*val, place.into())
        })?;

        this.validate_atomic_rmw(place, atomic)?;
        Ok(old)
    }

    /// Perform an atomic exchange with a memory place and a new
    /// scalar value, the old value is returned.
    fn atomic_exchange_scalar(
        &mut self,
        place: MPlaceTy<'tcx, Tag>, new: ScalarMaybeUninit<Tag>,
        atomic: AtomicRwOp
    ) -> InterpResult<'tcx, ScalarMaybeUninit<Tag>> {
        let this = self.eval_context_mut();

        let old = this.allow_data_races_mut(|this| {
            this.read_scalar(place.into())
        })?;
        this.allow_data_races_mut(|this| {
            this.write_scalar(new, place.into())
        })?;
        this.validate_atomic_rmw(place, atomic)?;
        Ok(old)
    }

    /// Perform an atomic compare and exchange at a given memory location
    /// on success an atomic RMW operation is performed and on failure
    /// only an atomic read occurs.
    fn atomic_compare_exchange_scalar(
        &mut self, place: MPlaceTy<'tcx, Tag>,
        expect_old: ImmTy<'tcx, Tag>, new: ScalarMaybeUninit<Tag>,
        success: AtomicRwOp, fail: AtomicReadOp
    ) -> InterpResult<'tcx, Immediate<Tag>> {
        let this = self.eval_context_mut();

        // Failure ordering cannot be stronger than success ordering, therefore first attempt
        // to read with the failure ordering and if successfull then try again with the success
        // read ordering and write in the success case.
        // Read as immediate for the sake of `binary_op()`
        let old = this.allow_data_races_mut(|this| {
            this.read_immediate(place.into())
        })?; 

        // `binary_op` will bail if either of them is not a scalar.
        let eq = this.overflowing_binary_op(mir::BinOp::Eq, old, expect_old)?.0;
        let res = Immediate::ScalarPair(old.to_scalar_or_uninit(), eq.into());

        // Update ptr depending on comparison.
        // if successful, perform a full rw-atomic validation
        // otherwise treat this as an atomic load with the fail ordering.
        if eq.to_bool()? {
            this.allow_data_races_mut(|this| {
                this.write_scalar(new, place.into())
            })?;
            this.validate_atomic_rmw(place, success)?;
        } else {
            this.validate_atomic_load(place, fail)?;
        }

        // Return the old value.
        Ok(res)
    }
    
    
    /// Update the data-race detector for an atomic read occuring at the
    /// associated memory-place and on the current thread.
    fn validate_atomic_load(
        &self, place: MPlaceTy<'tcx, Tag>, atomic: AtomicReadOp
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_ref();
        this.validate_atomic_op(
            place, atomic, "Atomic Load",
            move |memory, clocks, index, atomic| {
                if atomic == AtomicReadOp::Relaxed {
                    memory.load_relaxed(&mut *clocks, index)
                } else {
                    memory.load_acquire(&mut *clocks, index)
                }
            }
        )
    }

    /// Update the data-race detector for an atomic write occuring at the
    /// associated memory-place and on the current thread.
    fn validate_atomic_store(
        &mut self, place: MPlaceTy<'tcx, Tag>, atomic: AtomicWriteOp
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_ref();
        this.validate_atomic_op(
            place, atomic, "Atomic Store",
            move |memory, clocks, index, atomic| {
                if atomic == AtomicWriteOp::Relaxed {
                    memory.store_relaxed(clocks, index)
                } else {
                    memory.store_release(clocks, index)
                }
            }
        )
    }

    /// Update the data-race detector for an atomic read-modify-write occuring
    /// at the associated memory place and on the current thread.
    fn validate_atomic_rmw(
        &mut self, place: MPlaceTy<'tcx, Tag>, atomic: AtomicRwOp
    ) -> InterpResult<'tcx> {
        use AtomicRwOp::*;
        let acquire = matches!(atomic, Acquire | AcqRel | SeqCst);
        let release = matches!(atomic, Release | AcqRel | SeqCst);
        let this = self.eval_context_ref();
        this.validate_atomic_op(
            place, atomic, "Atomic RMW",
            move |memory, clocks, index, _| {
                if acquire {
                    memory.load_acquire(clocks, index)?;
                } else {
                    memory.load_relaxed(clocks, index)?;
                }
                if release {
                    memory.rmw_release(clocks, index)
                } else {
                    memory.rmw_relaxed(clocks, index)
                }
            }
        )
    }

    /// Update the data-race detector for an atomic fence on the current thread.
    fn validate_atomic_fence(&mut self, atomic: AtomicFenceOp) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        if let Some(data_race) = &this.memory.extra.data_race {
            data_race.maybe_perform_sync_operation(move |index, mut clocks| {
                log::trace!("Atomic fence on {:?} with ordering {:?}", index, atomic);

                // Apply data-race detection for the current fences
                // this treats AcqRel and SeqCst as the same as a acquire
                // and release fence applied in the same timestamp.
                if atomic != AtomicFenceOp::Release {
                    // Either Acquire | AcqRel | SeqCst
                    clocks.apply_acquire_fence();
                }
                if atomic != AtomicFenceOp::Acquire {
                    // Either Release | AcqRel | SeqCst
                    clocks.apply_release_fence();
                }
                Ok(())
            })
        } else {
            Ok(())
        }
    }
}



/// Vector clock metadata for a logical memory allocation.
#[derive(Debug, Clone)]
pub struct VClockAlloc {

    /// Range of Vector clocks, this gives each byte a potentially
    /// unqiue set of vector clocks, but merges identical information
    /// together for improved efficiency.
    alloc_ranges: RefCell<RangeMap<MemoryCellClocks>>,

    // Pointer to global state.
    global: MemoryExtra,
}


impl VClockAlloc {

    /// Create a new data-race allocation detector.
    pub fn new_allocation(global: &MemoryExtra, len: Size) -> VClockAlloc {
        VClockAlloc {
            global: Rc::clone(global),
            alloc_ranges: RefCell::new(
                RangeMap::new(len, MemoryCellClocks::default())
            )
        }
    }

    // Find an index, if one exists where the value
    // in `l` is greater than the value in `r`.
    fn find_gt_index(l: &VClock, r: &VClock) -> Option<VectorIdx> {
        let l_slice = l.as_slice();
        let r_slice = r.as_slice();
        l_slice.iter().zip(r_slice.iter())
            .enumerate()
            .find_map(|(idx, (&l, &r))| {
                if l > r { Some(idx) } else { None }
            }).or_else(|| {
                if l_slice.len() > r_slice.len() {

                    // By invariant, if l_slice is longer
                    // then one element must be larger.
                    // This just validates that this is true
                    // and reports earlier elements first.
                    let l_remainder_slice = &l_slice[r_slice.len()..];
                    let idx = l_remainder_slice.iter().enumerate()
                        .find_map(|(idx, &r)| {
                            if r == 0 { None } else { Some(idx) }
                        }).expect("Invalid VClock Invariant");
                    Some(idx)
                } else {
                    None
                }
            }).map(|idx| VectorIdx::new(idx))
    }

    /// Report a data-race found in the program.
    /// This finds the two racing threads and the type
    /// of data-race that occured. This will also
    /// return info about the memory location the data-race
    /// occured in.
    #[cold]
    #[inline(never)]
    fn report_data_race<'tcx>(
        global: &MemoryExtra, range: &MemoryCellClocks,
        action: &str, is_atomic: bool,
        pointer: Pointer<Tag>, len: Size
    ) -> InterpResult<'tcx> {
        let (current_index, current_clocks) = global.current_thread_state();
        let write_clock;
        let (
            other_action, other_thread, other_clock
        ) = if range.write > current_clocks.clock[range.write_index] {

            // Convert the write action into the vector clock it
            // represents for diagnostic purposes.
            write_clock = VClock::new_with_index(range.write_index, range.write);
            ("WRITE", range.write_index, &write_clock)
        } else if let Some(idx) = Self::find_gt_index(
            &range.read, &current_clocks.clock
        ){
            ("READ", idx, &range.read)
        } else if !is_atomic {
            if let Some(atomic) = range.atomic() {
                if let Some(idx) = Self::find_gt_index(
                    &atomic.write_vector, &current_clocks.clock
                ) {
                    ("ATOMIC_STORE", idx, &atomic.write_vector)
                } else if let Some(idx) = Self::find_gt_index(
                    &atomic.read_vector, &current_clocks.clock
                ) {
                    ("ATOMIC_LOAD", idx, &atomic.read_vector)
                } else {
                    unreachable!("Failed to report data-race for non-atomic operation: no race found")
                }
            } else {
                unreachable!("Failed to report data-race for non-atomic operation: no atomic component")
            }
        } else {
            unreachable!("Failed to report data-race for atomic operation")
        };

        // Load elaborated thread information about the racing thread actions.
        let current_thread_info = global.print_thread_metadata(current_index);
        let other_thread_info = global.print_thread_metadata(other_thread);
        
        // Throw the data-race detection.
        throw_ub_format!(
            "Data race detected between {} on {} and {} on {}, memory({:?},offset={},size={})\
            \n\t\t -current vector clock = {:?}\
            \n\t\t -conflicting timestamp = {:?}",
            action, current_thread_info, 
            other_action, other_thread_info,
            pointer.alloc_id, pointer.offset.bytes(), len.bytes(),
            current_clocks.clock,
            other_clock
        )
    }

    /// Detect data-races for an unsychronized read operation, will not perform
    /// data-race detection if `multi-threaded` is false, either due to no threads
    /// being created or if it is temporarily disabled during a racy read or write
    /// operation for which data-race detection is handled separately, for example
    /// atomic read operations.
    pub fn read<'tcx>(&self, pointer: Pointer<Tag>, len: Size) -> InterpResult<'tcx> {
        if self.global.multi_threaded.get() {
            let (index, clocks) = self.global.current_thread_state();
            let mut alloc_ranges = self.alloc_ranges.borrow_mut();
            for (_,range) in alloc_ranges.iter_mut(pointer.offset, len) {
                if let Err(DataRace) = range.read_race_detect(&*clocks, index) {

                    // Report data-race.
                    return Self::report_data_race(
                        &self.global,range, "READ", false, pointer, len
                    );
                }
            }
            Ok(())
        } else {
            Ok(())
        }
    }


    // Shared code for detecting data-races on unique access to a section of memory
    fn unique_access<'tcx>(&mut self, pointer: Pointer<Tag>, len: Size, action: &str) -> InterpResult<'tcx> {
        if self.global.multi_threaded.get() {
            let (index, clocks) = self.global.current_thread_state();
            for (_,range) in self.alloc_ranges.get_mut().iter_mut(pointer.offset, len) {
                if let Err(DataRace) = range.write_race_detect(&*clocks, index) {
                    
                    // Report data-race
                    return Self::report_data_race(
                        &self.global, range, action, false, pointer, len
                    );
                }
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Detect data-races for an unsychronized write operation, will not perform
    /// data-race threads if `multi-threaded` is false, either due to no threads
    /// being created or if it is temporarily disabled during a racy read or write
    /// operation
    pub fn write<'tcx>(&mut self, pointer: Pointer<Tag>, len: Size) -> InterpResult<'tcx> {
        self.unique_access(pointer, len, "Write")
    }

    /// Detect data-races for an unsychronized deallocate operation, will not perform
    /// data-race threads if `multi-threaded` is false, either due to no threads
    /// being created or if it is temporarily disabled during a racy read or write
    /// operation
    pub fn deallocate<'tcx>(&mut self, pointer: Pointer<Tag>, len: Size) -> InterpResult<'tcx> {
        self.unique_access(pointer, len, "Deallocate")
    }
}

impl<'mir, 'tcx: 'mir> EvalContextPrivExt<'mir, 'tcx> for MiriEvalContext<'mir, 'tcx> {}
trait EvalContextPrivExt<'mir, 'tcx: 'mir>: MiriEvalContextExt<'mir, 'tcx> {

    // Temporarily allow data-races to occur, this should only be
    // used if either one of the appropiate `validate_atomic` functions
    // will be called to treat a memory access as atomic or if the memory
    // being accessed should be treated as internal state, that cannot be
    // accessed by the interpreted program.
    #[inline]
    fn allow_data_races_ref<R>(&self, op: impl FnOnce(&MiriEvalContext<'mir, 'tcx>) -> R) -> R {
        let this = self.eval_context_ref();
        let old = if let Some(data_race) = &this.memory.extra.data_race {
            data_race.multi_threaded.replace(false)
        } else {
            false
        };
        let result = op(this);
        if let Some(data_race) = &this.memory.extra.data_race {
            data_race.multi_threaded.set(old);
        }
        result
    }

    /// Same as `allow_data_races_ref`, this temporarily disables any data-race detection and
    /// so should only be used for atomic operations or internal state that the program cannot
    /// access.
    #[inline]
    fn allow_data_races_mut<R>(&mut self, op: impl FnOnce(&mut MiriEvalContext<'mir, 'tcx>) -> R) -> R {
        let this = self.eval_context_mut();
        let old = if let Some(data_race) = &this.memory.extra.data_race {
            data_race.multi_threaded.replace(false)
        } else {
            false
        };
        let result = op(this);
        if let Some(data_race) = &this.memory.extra.data_race {
            data_race.multi_threaded.set(old);
        }
        result
    }

    /// Generic atomic operation implementation,
    /// this accesses memory via get_raw instead of
    /// get_raw_mut, due to issues calling get_raw_mut
    /// for atomic loads from read-only memory.
    /// FIXME: is this valid, or should get_raw_mut be used for
    /// atomic-stores/atomic-rmw?
    fn validate_atomic_op<A: Debug + Copy>(
        &self, place: MPlaceTy<'tcx, Tag>,
        atomic: A, description: &str,
        mut op: impl FnMut(
            &mut MemoryCellClocks, &mut ThreadClockSet, VectorIdx, A
        ) -> Result<(), DataRace>
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_ref();
        if let Some(data_race) = &this.memory.extra.data_race {
            if data_race.multi_threaded.get() {

                // Load and log the atomic operation.
                let place_ptr = place.ptr.assert_ptr();
                let size = place.layout.size;
                let alloc_meta =  &this.memory.get_raw(place_ptr.alloc_id)?.extra.data_race.as_ref().unwrap();
                log::trace!(
                    "Atomic op({}) with ordering {:?} on memory({:?}, offset={}, size={})",
                    description, &atomic, place_ptr.alloc_id, place_ptr.offset.bytes(), size.bytes()
                );

                // Perform the atomic operation.
                let data_race = &alloc_meta.global;
                data_race.maybe_perform_sync_operation(|index, mut clocks| {
                    for (_,range) in alloc_meta.alloc_ranges.borrow_mut().iter_mut(place_ptr.offset, size) {
                        if let Err(DataRace) = op(range, &mut *clocks, index, atomic) {
                            mem::drop(clocks);
                            return VClockAlloc::report_data_race(
                                &alloc_meta.global, range, description, true,
                                place_ptr, size
                            );
                        }
                    }
                    Ok(())
                })?;

                // Log changes to atomic memory.
                if log::log_enabled!(log::Level::Trace) {
                    for (_,range) in alloc_meta.alloc_ranges.borrow().iter(place_ptr.offset, size) {
                        log::trace!(
                            "Updated atomic memory({:?}, offset={}, size={}) to {:#?}",
                            place.ptr.assert_ptr().alloc_id, place_ptr.offset.bytes(), size.bytes(),
                            range.atomic_ops
                        );
                    }
                }
            }
        }
        Ok(())
    }

}


/// Extra metadata associated with a thread.
#[derive(Debug, Clone, Default)]
struct ThreadExtraState {

    /// The current vector index in use by the
    /// thread currently, this is set to None
    /// after the vector index has been re-used
    /// and hence the value will never need to be
    /// read during data-race reporting.
    vector_index: Option<VectorIdx>,

    /// The name of the thread, updated for better
    /// diagnostics when reporting detected data
    /// races.
    thread_name: Option<Box<str>>,
    
    /// Thread termination vector clock, this
    /// is set on thread termination and is used
    /// for joining on threads since the vector_index
    /// may be re-used when the join operation occurs.
    termination_vector_clock: Option<VClock>,
}

/// Global data-race detection state, contains the currently
/// executing thread as well as the vector-clocks associated
/// with each of the threads.
#[derive(Debug, Clone)]
pub struct GlobalState {

    /// Set to true once the first additional
    /// thread has launched, due to the dependency
    /// between before and after a thread launch.
    /// Any data-races must be recorded after this
    /// so concurrent execution can ignore recording
    /// any data-races.
    multi_threaded: Cell<bool>,

    /// Mapping of a vector index to a known set of thread
    /// clocks, this is not directly mapping from a thread id
    /// since it may refer to multiple threads.
    vector_clocks: RefCell<IndexVec<VectorIdx, ThreadClockSet>>,

    /// Mapping of a given vector index to the current thread
    /// that the execution is representing, this may change
    /// if a vector index is re-assigned to a new thread.
    vector_info: RefCell<IndexVec<VectorIdx, ThreadId>>,

    /// The mapping of a given thread to assocaited thread metadata.
    thread_info: RefCell<IndexVec<ThreadId, ThreadExtraState>>,

    /// The current vector index being executed.
    current_index: Cell<VectorIdx>,

    /// Potential vector indices that could be re-used on thread creation
    /// values are inserted here on after the thread has terminated and
    /// been joined with, and hence may potentially become free
    /// for use as the index for a new thread.
    /// Elements in this set may still require the vector index to
    /// report data-races, and can only be re-used after all
    /// active vector-clocks catch up with the threads timestamp.
    reuse_candidates: RefCell<FxHashSet<VectorIdx>>,

    /// Counts the number of threads that are currently active
    /// if the number of active threads reduces to 1 and then
    /// a join operation occures with the remaining main thread
    /// then multi-threaded execution may be disabled.
    active_thread_count: Cell<usize>, 

    /// This contains threads that have terminated, but not yet joined
    /// and so cannot become re-use candidates until a join operation
    /// occurs.
    /// The associated vector index will be moved into re-use candidates
    /// after the join operation occurs.
    terminated_threads: RefCell<FxHashMap<ThreadId, VectorIdx>>,
}

impl GlobalState {

    /// Create a new global state, setup with just thread-id=0
    /// advanced to timestamp = 1.
    pub fn new() -> Self {
        let global_state = GlobalState {
            multi_threaded: Cell::new(false),
            vector_clocks: RefCell::new(IndexVec::new()),
            vector_info: RefCell::new(IndexVec::new()),
            thread_info: RefCell::new(IndexVec::new()),
            current_index: Cell::new(VectorIdx::new(0)),
            active_thread_count: Cell::new(1),
            reuse_candidates: RefCell::new(FxHashSet::default()),
            terminated_threads: RefCell::new(FxHashMap::default())
        };

        // Setup the main-thread since it is not explicitly created:
        // uses vector index and thread-id 0, also the rust runtime gives
        // the main-thread a name of "main".
        let index = global_state.vector_clocks.borrow_mut().push(ThreadClockSet::default());
        global_state.vector_info.borrow_mut().push(ThreadId::new(0));
        global_state.thread_info.borrow_mut().push(
            ThreadExtraState {
                vector_index: Some(index),
                thread_name: Some("main".to_string().into_boxed_str()),
                termination_vector_clock: None
            }
        );

        global_state
    }
    
    // Try to find vector index values that can potentially be re-used
    // by a new thread instead of a new vector index being created.
    fn find_vector_index_reuse_candidate(&self) -> Option<VectorIdx> {
        let mut reuse = self.reuse_candidates.borrow_mut();
        let vector_clocks = self.vector_clocks.borrow();
        let vector_info = self.vector_info.borrow();
        let terminated_threads = self.terminated_threads.borrow();
        for  &candidate in reuse.iter() {
            let target_timestamp = vector_clocks[candidate].clock[candidate];
            if vector_clocks.iter_enumerated().all(|(clock_idx, clock)| {

                // The thread happens before the clock, and hence cannot report
                // a data-race with this the candidate index.
                let no_data_race = clock.clock[candidate] >= target_timestamp;

                // The vector represents a thread that has terminated and hence cannot
                // report a data-race with the candidate index.
                let thread_id = vector_info[clock_idx];
                let vector_terminated = reuse.contains(&clock_idx)
                    || terminated_threads.contains_key(&thread_id);

                // The vector index cannot report a race with the candidate index
                // and hence allows the candidate index to be re-used.
                no_data_race || vector_terminated
            }) {

                // All vector clocks for each vector index are equal to
                // the target timestamp, and the thread is known to have
                // terminated, therefore this vector clock index cannot
                // report any more data-races.
                assert!(reuse.remove(&candidate));
                return Some(candidate)
            }
        }
        None
    }

    // Hook for thread creation, enabled multi-threaded execution and marks
    // the current thread timestamp as happening-before the current thread.
    #[inline]
    pub fn thread_created(&self, thread: ThreadId) {
        let current_index = self.current_index();

        // Increment the number of active threads.
        let active_threads = self.active_thread_count.get();
        self.active_thread_count.set(active_threads + 1);

        // Enable multi-threaded execution, there are now two threads
        // so data-races are now possible.
        self.multi_threaded.set(true);

        // Load and setup the associated thread metadata
        let mut thread_info = self.thread_info.borrow_mut();
        thread_info.ensure_contains_elem(thread, Default::default);

        // Assign a vector index for the thread, attempting to re-use an old
        // vector index that can no longer report any data-races if possible.
        let created_index = if let Some(
            reuse_index
        ) = self.find_vector_index_reuse_candidate() {

            // Now re-configure the re-use candidate, increment the clock
            // for the new sync use of the vector.
            let mut vector_clocks = self.vector_clocks.borrow_mut();
            vector_clocks[reuse_index].increment_clock(reuse_index);

            // Locate the old thread the vector was associated with and update
            // it to represent the new thread instead.
            let mut vector_info = self.vector_info.borrow_mut();
            let old_thread = vector_info[reuse_index];
            vector_info[reuse_index] = thread;

            // Mark the thread the vector index was associated with as no longer
            // representing a thread index.
            thread_info[old_thread].vector_index = None;

            reuse_index
        } else {

            // No vector re-use candidates available, instead create
            // a new vector index.
            let mut vector_info = self.vector_info.borrow_mut();
            vector_info.push(thread)
        };

        // Mark the chosen vector index as in use by the thread.
        thread_info[thread].vector_index = Some(created_index);

        // Create a thread clock set if applicable.
        let mut vector_clocks = self.vector_clocks.borrow_mut();
        if created_index == vector_clocks.next_index() {
            vector_clocks.push(ThreadClockSet::default());
        }

        // Now load the two clocks and configure the initial state.
        let (current, created) = vector_clocks.pick2_mut(current_index, created_index);

        // Advance the current thread before the synchronized operation.
        current.increment_clock(current_index);

        // Join the created with current, since the current threads
        // previous actions happen-before the created thread.
        created.join_with(current);

        // Advance both threads after the synchronized operation.
        current.increment_clock(current_index);
        created.increment_clock(created_index);
    }

    /// Hook on a thread join to update the implicit happens-before relation
    /// between the joined thead and the current thread.
    #[inline]
    pub fn thread_joined(&self, current_thread: ThreadId, join_thread: ThreadId) {
        let mut clocks_vec = self.vector_clocks.borrow_mut();
        let thread_info = self.thread_info.borrow();

        // Load the vector clock of the current thread.
        let current_index = thread_info[current_thread].vector_index
            .expect("Performed thread join on thread with no assigned vector");
        let current = &mut clocks_vec[current_index];

        // Load the associated vector clock for the terminated thread.
        let join_clock = thread_info[join_thread].termination_vector_clock
            .as_ref().expect("Joined with thread but thread has not terminated");

        // Pre increment clocks before atomic operation.
        current.increment_clock(current_index);

        // The join thread happens-before the current thread
        // so update the current vector clock.
        current.clock.join(join_clock);

        // Post increment clocks after atomic operation.
        current.increment_clock(current_index);

        // Check the number of active threads, if the value is 1
        // then test for potentially disabling multi-threaded execution.
        let active_threads = self.active_thread_count.get();
        if active_threads == 1 {

            // May potentially be able to disable multi-threaded execution.
            let current_clock = &clocks_vec[current_index];
            if clocks_vec.iter_enumerated().all(|(idx, clocks)| {
                clocks.clock[idx] <= current_clock.clock[idx]
            }) {

                // The all thread termations happen-before the current clock
                // therefore no data-races can be reported until a new thread
                // is created, so disable multi-threaded execution.
                self.multi_threaded.set(false);
            }
        }

        // If the thread is marked as terminated but not joined
        // then move the thread to the re-use set.
        let mut termination = self.terminated_threads.borrow_mut();
        if let Some(index) = termination.remove(&join_thread) {
            let mut reuse = self.reuse_candidates.borrow_mut();
            reuse.insert(index);
        }
    }

    /// On thread termination, the vector-clock may re-used
    /// in the future once all remaining thread-clocks catch
    /// up with the time index of the terminated thread.
    /// This assiges thread termination with a unique index
    /// which will be used to join the thread
    /// This should be called strictly before any calls to
    /// `thread_joined`.
    #[inline]
    pub fn thread_terminated(&self) {
        let current_index = self.current_index();
        
        // Increment the clock to a unique termination timestamp.
        let mut vector_clocks = self.vector_clocks.borrow_mut();
        let current_clocks = &mut vector_clocks[current_index];
        current_clocks.increment_clock(current_index);

        // Load the current thread id for the executing vector.
        let vector_info = self.vector_info.borrow();
        let current_thread = vector_info[current_index];

        // Load the current thread metadata, and move to a terminated
        // vector state. Setting up the vector clock all join operations
        // will use.
        let mut thread_info = self.thread_info.borrow_mut();
        let current = &mut thread_info[current_thread];
        current.termination_vector_clock = Some(current_clocks.clock.clone());

        // Add this thread as a candidate for re-use after a thread join
        // occurs.
        let mut termination = self.terminated_threads.borrow_mut();
        termination.insert(current_thread, current_index);
            
        // Reduce the number of active threads, now that a thread has
        // terminated.
        let mut active_threads = self.active_thread_count.get();
        active_threads -= 1;
        self.active_thread_count.set(active_threads);
    }

    /// Hook for updating the local tracker of the currently
    /// enabled thread, should always be updated whenever
    /// `active_thread` in thread.rs is updated.
    #[inline]
    pub fn thread_set_active(&self, thread: ThreadId) {
        let thread_info = self.thread_info.borrow();
        let vector_idx = thread_info[thread].vector_index
            .expect("Setting thread active with no assigned vector");
        self.current_index.set(vector_idx);
    }

    /// Hook for updating the local tracker of the threads name
    /// this should always mirror the local value in thread.rs
    /// the thread name is used for improved diagnostics
    /// during a data-race.
    #[inline]
    pub fn thread_set_name(&self, thread: ThreadId, name: String) {
        let name = name.into_boxed_str();
        let mut thread_info = self.thread_info.borrow_mut();
        thread_info[thread].thread_name = Some(name);
    }


    /// Attempt to perform a synchronized operation, this
    /// will perform no operation if multi-threading is
    /// not currently enabled.
    /// Otherwise it will increment the clock for the current
    /// vector before and after the operation for data-race
    /// detection between any happens-before edges the
    /// operation may create.
    fn maybe_perform_sync_operation<'tcx>(
        &self, op: impl FnOnce(VectorIdx, RefMut<'_,ThreadClockSet>) -> InterpResult<'tcx>,
    ) -> InterpResult<'tcx> {
        if self.multi_threaded.get() {
            let (index, mut clocks) = self.current_thread_state_mut();
            clocks.increment_clock(index);
            op(index, clocks)?;
            let (_, mut clocks) = self.current_thread_state_mut();
            clocks.increment_clock(index);
        }
        Ok(())
    }
    

    /// Internal utility to identify a thread stored internally
    /// returns the id and the name for better diagnostics.
    fn print_thread_metadata(&self, vector: VectorIdx) -> String {
        let thread = self.vector_info.borrow()[vector];
        let thread_name = &self.thread_info.borrow()[thread].thread_name;
        if let Some(name) = thread_name {
            let name: &str = name;
            format!("Thread(id = {:?}, name = {:?})", thread.to_u32(), &*name)
        } else {
            format!("Thread(id = {:?})", thread.to_u32())
        }
    }


    /// Acquire a lock, express that the previous call of
    /// `validate_lock_release` must happen before this.
    pub fn validate_lock_acquire(&self, lock: &VClock, thread: ThreadId) {
        let (index, mut clocks) = self.load_thread_state_mut(thread);
        clocks.increment_clock(index);
        clocks.clock.join(&lock);
        clocks.increment_clock(index);
    }

    /// Release a lock handle, express that this happens-before
    /// any subsequent calls to `validate_lock_acquire`.
    pub fn validate_lock_release(&self, lock: &mut VClock, thread: ThreadId) {
        let (index, mut clocks) = self.load_thread_state_mut(thread);
        clocks.increment_clock(index);
        lock.clone_from(&clocks.clock);
        clocks.increment_clock(index);
    }

    /// Release a lock handle, express that this happens-before
    /// any subsequent calls to `validate_lock_acquire` as well
    /// as any previous calls to this function after any
    /// `validate_lock_release` calls.
    pub fn validate_lock_release_shared(&self, lock: &mut VClock, thread: ThreadId) {
        let (index, mut clocks) = self.load_thread_state_mut(thread);
        clocks.increment_clock(index);
        lock.join(&clocks.clock);
        clocks.increment_clock(index);
    }

    /// Load the vector index used by the given thread as well as the set of vector clocks
    /// used by the thread.
    #[inline]
    fn load_thread_state_mut(&self, thread: ThreadId) -> (VectorIdx, RefMut<'_, ThreadClockSet>) {
        let index = self.thread_info.borrow()[thread].vector_index
            .expect("Loading thread state for thread with no assigned vector");
        let ref_vector = self.vector_clocks.borrow_mut();
        let clocks = RefMut::map(ref_vector, |vec| &mut vec[index]);
        (index, clocks)
    }

    /// Load the current vector clock in use and the current set of thread clocks
    /// in use for the vector.
    #[inline]
    fn current_thread_state(&self) -> (VectorIdx, Ref<'_, ThreadClockSet>) {
        let index = self.current_index();
        let ref_vector = self.vector_clocks.borrow();
        let clocks = Ref::map(ref_vector, |vec| &vec[index]);
        (index, clocks)
    }

    /// Load the current vector clock in use and the current set of thread clocks
    /// in use for the vector mutably for modification.
    #[inline]
    fn current_thread_state_mut(&self) -> (VectorIdx, RefMut<'_, ThreadClockSet>) {
        let index = self.current_index();
        let ref_vector = self.vector_clocks.borrow_mut();
        let clocks = RefMut::map(ref_vector, |vec| &mut vec[index]);
        (index, clocks)
    }

    /// Return the current thread, should be the same
    /// as the data-race active thread.
    #[inline]
    fn current_index(&self) -> VectorIdx {
        self.current_index.get()
    }
}

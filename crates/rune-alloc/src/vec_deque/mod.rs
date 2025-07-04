//! A double-ended queue (deque) implemented with a growable ring buffer.
//!
//! This queue has *O*(1) amortized inserts and removals from both ends of the
//! container. It also has *O*(1) indexing like a vector. The contained elements
//! are not required to be copyable, and the queue will be sendable if the
//! contained type is sendable.

#![allow(clippy::redundant_closure)]

use core::cmp::{self, Ordering};
use core::fmt;
use core::hash::{Hash, Hasher};
use core::mem::ManuallyDrop;
use core::ops::{Index, IndexMut, Range, RangeBounds};
use core::ptr;
use core::slice;

// This is used in a bunch of intra-doc links.
// FIXME: For some reason, `#[cfg(doc)]` wasn't sufficient, resulting in
// failures in linkchecker even though rustdoc built the docs just fine.
#[allow(unused_imports)]
use core::mem;

use crate::alloc::{Allocator, Global, SizedTypeProperties};
use crate::clone::TryClone;
use crate::error::Error;
use crate::iter::{TryExtend, TryFromIteratorIn};
use crate::raw_vec::RawVec;
use crate::slice::range as slice_range;
use crate::vec::Vec;

#[macro_use]
mod macros;

pub use self::drain::Drain;

mod drain;

pub use self::iter_mut::IterMut;

mod iter_mut;

pub use self::into_iter::IntoIter;

mod into_iter;

pub use self::iter::Iter;

mod iter;

pub use self::raw_iter::RawIter;

mod raw_iter;

/// A double-ended queue implemented with a growable ring buffer.
///
/// The "default" usage of this type as a queue is to use [`try_push_back`] to add to
/// the queue, and [`pop_front`] to remove from the queue. [`try_extend`] and [`try_append`]
/// push onto the back in this manner, and iterating over `VecDeque` goes front
/// to back.
///
/// A `VecDeque` with a known list of items can be initialized from an array:
///
/// ```
/// use rune::alloc::VecDeque;
///
/// let deq = VecDeque::try_from([-1, 0, 1])?;
/// # Ok::<_, rune::alloc::Error>(())
/// ```
///
/// Since `VecDeque` is a ring buffer, its elements are not necessarily contiguous
/// in memory. If you want to access the elements as a single slice, such as for
/// efficient sorting, you can use [`make_contiguous`]. It rotates the `VecDeque`
/// so that its elements do not wrap, and returns a mutable slice to the
/// now-contiguous element sequence.
///
/// [`try_push_back`]: VecDeque::try_push_back
/// [`pop_front`]: VecDeque::pop_front
/// [`try_extend`]: VecDeque::try_extend
/// [`try_append`]: VecDeque::try_append
/// [`make_contiguous`]: VecDeque::make_contiguous
pub struct VecDeque<T, A: Allocator = Global> {
    // `self[0]`, if it exists, is `buf[head]`.
    // `head < buf.capacity()`, unless `buf.capacity() == 0` when `head == 0`.
    head: usize,
    // the number of initialized elements, starting from the one at `head` and potentially wrapping around.
    // if `len == 0`, the exact value of `head` is unimportant.
    // if `T` is zero-Sized, then `self.len <= usize::MAX`, otherwise `self.len <= isize::MAX as usize`.
    len: usize,
    buf: RawVec<T, A>,
}

impl<T: TryClone, A: Allocator + Clone> TryClone for VecDeque<T, A> {
    fn try_clone(&self) -> Result<Self, Error> {
        let mut deq = Self::try_with_capacity_in(self.len(), self.allocator().clone())?;

        for value in self.iter() {
            deq.try_push_back(value.try_clone()?)?;
        }

        Ok(deq)
    }

    fn try_clone_from(&mut self, other: &Self) -> Result<(), Error> {
        self.clear();

        for value in other.iter() {
            self.try_push_back(value.try_clone()?)?;
        }

        Ok(())
    }
}

#[cfg(rune_nightly)]
unsafe impl<#[may_dangle] T, A> Drop for VecDeque<T, A>
where
    A: Allocator,
{
    fn drop(&mut self) {
        /// Runs the destructor for all items in the slice when it gets dropped (normally or
        /// during unwinding).
        struct Dropper<'a, T>(&'a mut [T]);

        impl<'a, T> Drop for Dropper<'a, T> {
            fn drop(&mut self) {
                unsafe {
                    ptr::drop_in_place(self.0);
                }
            }
        }

        let (front, back) = self.as_mut_slices();
        unsafe {
            let _back_dropper = Dropper(back);
            // use drop for [T]
            ptr::drop_in_place(front);
        }
        // RawVec handles deallocation
    }
}

#[cfg(not(rune_nightly))]
impl<T, A> Drop for VecDeque<T, A>
where
    A: Allocator,
{
    fn drop(&mut self) {
        /// Runs the destructor for all items in the slice when it gets dropped (normally or
        /// during unwinding).
        struct Dropper<'a, T>(&'a mut [T]);

        impl<T> Drop for Dropper<'_, T> {
            fn drop(&mut self) {
                unsafe {
                    ptr::drop_in_place(self.0);
                }
            }
        }

        let (front, back) = self.as_mut_slices();
        unsafe {
            let _back_dropper = Dropper(back);
            // use drop for [T]
            ptr::drop_in_place(front);
        }
        // RawVec handles deallocation
    }
}

impl<T> Default for VecDeque<T> {
    /// Creates an empty deque.
    #[inline]
    fn default() -> VecDeque<T> {
        VecDeque::new()
    }
}

impl<T, A> VecDeque<T, A>
where
    A: Allocator,
{
    /// Marginally more convenient
    #[inline]
    fn ptr(&self) -> *mut T {
        self.buf.ptr()
    }

    /// Moves an element out of the buffer
    #[inline]
    unsafe fn buffer_read(&mut self, off: usize) -> T {
        unsafe { ptr::read(self.ptr().add(off)) }
    }

    /// Writes an element into the buffer, moving it.
    #[inline]
    unsafe fn buffer_write(&mut self, off: usize, value: T) {
        unsafe {
            ptr::write(self.ptr().add(off), value);
        }
    }

    /// Returns a slice pointer into the buffer.
    /// `range` must lie inside `0..self.capacity()`.
    #[inline]
    unsafe fn buffer_range(&self, range: Range<usize>) -> *mut [T] {
        unsafe {
            ptr::slice_from_raw_parts_mut(self.ptr().add(range.start), range.end - range.start)
        }
    }

    /// Returns `true` if the buffer is at full capacity.
    #[inline]
    fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    /// Returns the index in the underlying buffer for a given logical element
    /// index + addend.
    #[inline]
    fn wrap_add(&self, idx: usize, addend: usize) -> usize {
        wrap_index(idx.wrapping_add(addend), self.capacity())
    }

    #[inline]
    fn to_physical_idx(&self, idx: usize) -> usize {
        self.wrap_add(self.head, idx)
    }

    /// Returns the index in the underlying buffer for a given logical element
    /// index - subtrahend.
    #[inline]
    fn wrap_sub(&self, idx: usize, subtrahend: usize) -> usize {
        wrap_index(
            idx.wrapping_sub(subtrahend).wrapping_add(self.capacity()),
            self.capacity(),
        )
    }

    /// Copies a contiguous block of memory len long from src to dst
    #[inline]
    unsafe fn copy(&mut self, src: usize, dst: usize, len: usize) {
        debug_assert!(
            dst + len <= self.capacity(),
            "cpy dst={} src={} len={} cap={}",
            dst,
            src,
            len,
            self.capacity()
        );
        debug_assert!(
            src + len <= self.capacity(),
            "cpy dst={} src={} len={} cap={}",
            dst,
            src,
            len,
            self.capacity()
        );
        unsafe {
            ptr::copy(self.ptr().add(src), self.ptr().add(dst), len);
        }
    }

    /// Copies a contiguous block of memory len long from src to dst
    #[inline]
    unsafe fn copy_nonoverlapping(&mut self, src: usize, dst: usize, len: usize) {
        debug_assert!(
            dst + len <= self.capacity(),
            "cno dst={} src={} len={} cap={}",
            dst,
            src,
            len,
            self.capacity()
        );
        debug_assert!(
            src + len <= self.capacity(),
            "cno dst={} src={} len={} cap={}",
            dst,
            src,
            len,
            self.capacity()
        );
        unsafe {
            ptr::copy_nonoverlapping(self.ptr().add(src), self.ptr().add(dst), len);
        }
    }

    /// Copies a potentially wrapping block of memory len long from src to dest.
    /// (abs(dst - src) + len) must be no larger than capacity() (There must be at
    /// most one continuous overlapping region between src and dest).
    unsafe fn wrap_copy(&mut self, src: usize, dst: usize, len: usize) {
        debug_assert!(
            cmp::min(src.abs_diff(dst), self.capacity() - src.abs_diff(dst)) + len
                <= self.capacity(),
            "wrc dst={} src={} len={} cap={}",
            dst,
            src,
            len,
            self.capacity()
        );

        // If T is a ZST, don't do any copying.
        if T::IS_ZST || src == dst || len == 0 {
            return;
        }

        let dst_after_src = self.wrap_sub(dst, src) < len;

        let src_pre_wrap_len = self.capacity() - src;
        let dst_pre_wrap_len = self.capacity() - dst;
        let src_wraps = src_pre_wrap_len < len;
        let dst_wraps = dst_pre_wrap_len < len;

        match (dst_after_src, src_wraps, dst_wraps) {
            (_, false, false) => {
                // src doesn't wrap, dst doesn't wrap
                //
                //        S . . .
                // 1 [_ _ A A B B C C _]
                // 2 [_ _ A A A A B B _]
                //            D . . .
                //
                unsafe {
                    self.copy(src, dst, len);
                }
            }
            (false, false, true) => {
                // dst before src, src doesn't wrap, dst wraps
                //
                //    S . . .
                // 1 [A A B B _ _ _ C C]
                // 2 [A A B B _ _ _ A A]
                // 3 [B B B B _ _ _ A A]
                //    . .           D .
                //
                unsafe {
                    self.copy(src, dst, dst_pre_wrap_len);
                    self.copy(src + dst_pre_wrap_len, 0, len - dst_pre_wrap_len);
                }
            }
            (true, false, true) => {
                // src before dst, src doesn't wrap, dst wraps
                //
                //              S . . .
                // 1 [C C _ _ _ A A B B]
                // 2 [B B _ _ _ A A B B]
                // 3 [B B _ _ _ A A A A]
                //    . .           D .
                //
                unsafe {
                    self.copy(src + dst_pre_wrap_len, 0, len - dst_pre_wrap_len);
                    self.copy(src, dst, dst_pre_wrap_len);
                }
            }
            (false, true, false) => {
                // dst before src, src wraps, dst doesn't wrap
                //
                //    . .           S .
                // 1 [C C _ _ _ A A B B]
                // 2 [C C _ _ _ B B B B]
                // 3 [C C _ _ _ B B C C]
                //              D . . .
                //
                unsafe {
                    self.copy(src, dst, src_pre_wrap_len);
                    self.copy(0, dst + src_pre_wrap_len, len - src_pre_wrap_len);
                }
            }
            (true, true, false) => {
                // src before dst, src wraps, dst doesn't wrap
                //
                //    . .           S .
                // 1 [A A B B _ _ _ C C]
                // 2 [A A A A _ _ _ C C]
                // 3 [C C A A _ _ _ C C]
                //    D . . .
                //
                unsafe {
                    self.copy(0, dst + src_pre_wrap_len, len - src_pre_wrap_len);
                    self.copy(src, dst, src_pre_wrap_len);
                }
            }
            (false, true, true) => {
                // dst before src, src wraps, dst wraps
                //
                //    . . .         S .
                // 1 [A B C D _ E F G H]
                // 2 [A B C D _ E G H H]
                // 3 [A B C D _ E G H A]
                // 4 [B C C D _ E G H A]
                //    . .         D . .
                //
                debug_assert!(dst_pre_wrap_len > src_pre_wrap_len);
                let delta = dst_pre_wrap_len - src_pre_wrap_len;
                unsafe {
                    self.copy(src, dst, src_pre_wrap_len);
                    self.copy(0, dst + src_pre_wrap_len, delta);
                    self.copy(delta, 0, len - dst_pre_wrap_len);
                }
            }
            (true, true, true) => {
                // src before dst, src wraps, dst wraps
                //
                //    . .         S . .
                // 1 [A B C D _ E F G H]
                // 2 [A A B D _ E F G H]
                // 3 [H A B D _ E F G H]
                // 4 [H A B D _ E F F G]
                //    . . .         D .
                //
                debug_assert!(src_pre_wrap_len > dst_pre_wrap_len);
                let delta = src_pre_wrap_len - dst_pre_wrap_len;
                unsafe {
                    self.copy(0, delta, len - src_pre_wrap_len);
                    self.copy(self.capacity() - delta, 0, delta);
                    self.copy(src, dst, dst_pre_wrap_len);
                }
            }
        }
    }

    /// Copies all values from `src` to `dst`, wrapping around if needed.
    /// Assumes capacity is sufficient.
    #[inline]
    unsafe fn copy_slice(&mut self, dst: usize, src: &[T]) {
        debug_assert!(src.len() <= self.capacity());
        let head_room = self.capacity() - dst;
        if src.len() <= head_room {
            unsafe {
                ptr::copy_nonoverlapping(src.as_ptr(), self.ptr().add(dst), src.len());
            }
        } else {
            let (left, right) = src.split_at(head_room);
            unsafe {
                ptr::copy_nonoverlapping(left.as_ptr(), self.ptr().add(dst), left.len());
                ptr::copy_nonoverlapping(right.as_ptr(), self.ptr(), right.len());
            }
        }
    }

    /// Frobs the head and tail sections around to handle the fact that we
    /// just reallocated. Unsafe because it trusts old_capacity.
    #[inline]
    unsafe fn handle_capacity_increase(&mut self, old_capacity: usize) {
        let new_capacity = self.capacity();
        debug_assert!(new_capacity >= old_capacity);

        // Move the shortest contiguous section of the ring buffer
        //
        // H := head
        // L := last element (`self.to_physical_idx(self.len - 1)`)
        //
        //    H           L
        //   [o o o o o o o . ]
        //    H           L
        // A [o o o o o o o . . . . . . . . . ]
        //        L H
        //   [o o o o o o o o ]
        //          H           L
        // B [. . . o o o o o o o . . . . . . ]
        //              L H
        //   [o o o o o o o o ]
        //            L                   H
        // C [o o o o o . . . . . . . . . o o ]

        // can't use is_contiguous() because the capacity is already updated.
        if self.head <= old_capacity - self.len {
            // A
            // Nop
        } else {
            let head_len = old_capacity - self.head;
            let tail_len = self.len - head_len;
            if head_len > tail_len && new_capacity - old_capacity >= tail_len {
                // B
                unsafe {
                    self.copy_nonoverlapping(0, old_capacity, tail_len);
                }
            } else {
                // C
                let new_head = new_capacity - head_len;
                unsafe {
                    // can't use copy_nonoverlapping here, because if e.g. head_len = 2
                    // and new_capacity = old_capacity + 1, then the heads overlap.
                    self.copy(self.head, new_head, head_len);
                }
                self.head = new_head;
            }
        }
        debug_assert!(self.head < self.capacity() || self.capacity() == 0);
    }
}

impl<T> VecDeque<T> {
    /// Creates an empty deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<u32> = VecDeque::new();
    /// ```
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self::new_in(Global)
    }

    /// Creates an empty deque with space for at least `capacity` elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<u32> = VecDeque::try_with_capacity(10)?;
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_with_capacity(capacity: usize) -> Result<Self, Error> {
        Self::try_with_capacity_in(capacity, Global)
    }
}

impl<T, A> VecDeque<T, A>
where
    A: Allocator,
{
    /// Creates an empty deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<u32> = VecDeque::new();
    /// ```
    #[inline]
    pub const fn new_in(alloc: A) -> VecDeque<T, A> {
        VecDeque {
            head: 0,
            len: 0,
            buf: RawVec::new_in(alloc),
        }
    }

    /// Creates an empty deque with space for at least `capacity` elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::alloc::Global;
    ///
    /// let deque: VecDeque<u32> = VecDeque::try_with_capacity_in(10, Global)?;
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<VecDeque<T, A>, Error> {
        Ok(VecDeque {
            head: 0,
            len: 0,
            buf: RawVec::try_with_capacity_in(capacity, alloc)?,
        })
    }

    /// Provides a reference to the element at the given index.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    ///
    /// buf.try_push_back(3);
    /// buf.try_push_back(4);
    /// buf.try_push_back(5);
    /// buf.try_push_back(6);
    ///
    /// assert_eq!(buf.get(1), Some(&4));
    ///
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len {
            let idx = self.to_physical_idx(index);
            unsafe { Some(&*self.ptr().add(idx)) }
        } else {
            None
        }
    }

    /// Provides a mutable reference to the element at the given index.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    ///
    /// buf.try_push_back(3)?;
    /// buf.try_push_back(4)?;
    /// buf.try_push_back(5)?;
    /// buf.try_push_back(6)?;
    ///
    /// assert_eq!(buf[1], 4);
    ///
    /// if let Some(elem) = buf.get_mut(1) {
    ///     *elem = 7;
    /// }
    ///
    /// assert_eq!(buf[1], 7);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index < self.len {
            let idx = self.to_physical_idx(index);
            unsafe { Some(&mut *self.ptr().add(idx)) }
        } else {
            None
        }
    }

    /// Swaps elements at indices `i` and `j`.
    ///
    /// `i` and `j` may be equal.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Panics
    ///
    /// Panics if either index is out of bounds.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    ///
    /// buf.try_push_back(3)?;
    /// buf.try_push_back(4)?;
    /// buf.try_push_back(5)?;
    ///
    /// assert_eq!(buf, [3, 4, 5]);
    ///
    /// buf.swap(0, 2);
    ///
    /// assert_eq!(buf, [5, 4, 3]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn swap(&mut self, i: usize, j: usize) {
        assert!(i < self.len());
        assert!(j < self.len());
        let ri = self.to_physical_idx(i);
        let rj = self.to_physical_idx(j);
        unsafe { ptr::swap(self.ptr().add(ri), self.ptr().add(rj)) }
    }

    /// Returns the number of elements the deque can hold without reallocating.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let buf: VecDeque<i32> = VecDeque::try_with_capacity(10)?;
    /// assert!(buf.capacity() >= 10);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        if T::IS_ZST {
            usize::MAX
        } else {
            self.buf.capacity()
        }
    }

    /// Tries to reserve the minimum capacity for at least `additional` more elements to
    /// be inserted in the given deque. After calling `try_reserve_exact`,
    /// capacity will be greater than or equal to `self.len() + additional` if
    /// it returns `Ok(())`. Does nothing if the capacity is already sufficient.
    ///
    /// Note that the allocator may give the collection more space than it
    /// requests. Therefore, capacity can not be relied upon to be precisely
    /// minimal. Prefer [`try_reserve`] if future insertions are expected.
    ///
    /// [`try_reserve`]: VecDeque::try_reserve
    ///
    /// # Errors
    ///
    /// If the capacity overflows `usize`, or the allocator reports a failure, then an error
    /// is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::{VecDeque, Error};
    /// use rune::alloc::prelude::*;
    ///
    /// fn process_data(data: &[u32]) -> Result<VecDeque<u32>, Error> {
    ///     let mut output = VecDeque::new();
    ///
    ///     // Pre-reserve the memory, exiting if we can't
    ///     output.try_reserve_exact(data.len())?;
    ///
    ///     // Now we know this can't OOM(Out-Of-Memory) in the middle of our complex work
    ///     output.try_extend(data.iter().map(|&val| {
    ///         val * 2 + 5 // very complicated
    ///     }))?;
    ///
    ///     Ok(output)
    /// }
    /// # process_data(&[1, 2, 3]).expect("why is the test harness OOMing on 12 bytes?");
    /// ```
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), Error> {
        let new_cap = self
            .len
            .checked_add(additional)
            .ok_or(Error::CapacityOverflow)?;
        let old_cap = self.capacity();

        if new_cap > old_cap {
            self.buf.try_reserve_exact(self.len, additional)?;
            unsafe {
                self.handle_capacity_increase(old_cap);
            }
        }
        Ok(())
    }

    /// Tries to reserve capacity for at least `additional` more elements to be inserted
    /// in the given deque. The collection may reserve more space to speculatively avoid
    /// frequent reallocations. After calling `try_reserve`, capacity will be
    /// greater than or equal to `self.len() + additional` if it returns
    /// `Ok(())`. Does nothing if capacity is already sufficient. This method
    /// preserves the contents even if an error occurs.
    ///
    /// # Errors
    ///
    /// If the capacity overflows `usize`, or the allocator reports a failure, then an error
    /// is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::{VecDeque, Error};
    /// use rune::alloc::prelude::*;
    ///
    /// fn process_data(data: &[u32]) -> Result<VecDeque<u32>, Error> {
    ///     let mut output = VecDeque::new();
    ///
    ///     // Pre-reserve the memory, exiting if we can't
    ///     output.try_reserve(data.len())?;
    ///
    ///     // Now we know this can't OOM in the middle of our complex work
    ///     output.try_extend(data.iter().map(|&val| {
    ///         val * 2 + 5 // very complicated
    ///     }))?;
    ///
    ///     Ok(output)
    /// }
    /// # process_data(&[1, 2, 3]).expect("why is the test harness OOMing on 12 bytes?");
    /// ```
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), Error> {
        let new_cap = self
            .len
            .checked_add(additional)
            .ok_or(Error::CapacityOverflow)?;
        let old_cap = self.capacity();

        if new_cap > old_cap {
            self.buf.try_reserve(self.len, additional)?;
            unsafe {
                self.handle_capacity_increase(old_cap);
            }
        }

        Ok(())
    }

    /// Shrinks the capacity of the deque as much as possible.
    ///
    /// It will drop down as close as possible to the length but the allocator may still inform the
    /// deque that there is space for a few more elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf = VecDeque::try_with_capacity(15)?;
    /// buf.try_extend(0..4)?;
    /// assert_eq!(buf.capacity(), 15);
    /// buf.try_shrink_to_fit()?;
    /// assert!(buf.capacity() >= 4);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_shrink_to_fit(&mut self) -> Result<(), Error> {
        self.try_shrink_to(0)
    }

    /// Shrinks the capacity of the deque with a lower bound.
    ///
    /// The capacity will remain at least as large as both the length
    /// and the supplied value.
    ///
    /// If the current capacity is less than the lower limit, this is a no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf = VecDeque::try_with_capacity(15)?;
    /// buf.try_extend(0..4)?;
    /// assert_eq!(buf.capacity(), 15);
    /// buf.try_shrink_to(6)?;
    /// assert!(buf.capacity() >= 6);
    /// buf.try_shrink_to(0)?;
    /// assert!(buf.capacity() >= 4);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_shrink_to(&mut self, min_capacity: usize) -> Result<(), Error> {
        let target_cap = min_capacity.max(self.len);

        // never shrink ZSTs
        if T::IS_ZST || self.capacity() <= target_cap {
            return Ok(());
        }

        // There are three cases of interest:
        //   All elements are out of desired bounds
        //   Elements are contiguous, and tail is out of desired bounds
        //   Elements are discontiguous
        //
        // At all other times, element positions are unaffected.

        // `head` and `len` are at most `isize::MAX` and `target_cap < self.capacity()`, so nothing can
        // overflow.
        let tail_outside = (target_cap + 1..=self.capacity()).contains(&(self.head + self.len));

        if self.len == 0 {
            self.head = 0;
        } else if self.head >= target_cap && tail_outside {
            // Head and tail are both out of bounds, so copy all of them to the front.
            //
            //  H := head
            //  L := last element
            //                    H           L
            //   [. . . . . . . . o o o o o o o . ]
            //    H           L
            //   [o o o o o o o . ]
            unsafe {
                // nonoverlapping because `self.head >= target_cap >= self.len`.
                self.copy_nonoverlapping(self.head, 0, self.len);
            }
            self.head = 0;
        } else if self.head < target_cap && tail_outside {
            // Head is in bounds, tail is out of bounds.
            // Copy the overflowing part to the beginning of the
            // buffer. This won't overlap because `target_cap >= self.len`.
            //
            //  H := head
            //  L := last element
            //          H           L
            //   [. . . o o o o o o o . . . . . . ]
            //      L   H
            //   [o o . o o o o o ]
            let len = self.head + self.len - target_cap;
            unsafe {
                self.copy_nonoverlapping(target_cap, 0, len);
            }
        } else if !self.is_contiguous() {
            // The head slice is at least partially out of bounds, tail is in bounds.
            // Copy the head backwards so it lines up with the target capacity.
            // This won't overlap because `target_cap >= self.len`.
            //
            //  H := head
            //  L := last element
            //            L                   H
            //   [o o o o o . . . . . . . . . o o ]
            //            L   H
            //   [o o o o o . o o ]
            let head_len = self.capacity() - self.head;
            let new_head = target_cap - head_len;
            unsafe {
                // can't use `copy_nonoverlapping()` here because the new and old
                // regions for the head might overlap.
                self.copy(self.head, new_head, head_len);
            }
            self.head = new_head;
        }

        self.buf.try_shrink_to_fit(target_cap)?;

        debug_assert!(self.head < self.capacity() || self.capacity() == 0);
        debug_assert!(self.len <= self.capacity());
        Ok(())
    }

    /// Shortens the deque, keeping the first `len` elements and dropping
    /// the rest.
    ///
    /// If `len` is greater than the deque's current length, this has no
    /// effect.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    ///
    /// buf.try_push_back(5)?;
    /// buf.try_push_back(10)?;
    /// buf.try_push_back(15)?;
    ///
    /// assert_eq!(buf, [5, 10, 15]);
    ///
    /// buf.truncate(1);
    ///
    /// assert_eq!(buf, [5]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn truncate(&mut self, len: usize) {
        /// Runs the destructor for all items in the slice when it gets dropped (normally or
        /// during unwinding).
        struct Dropper<'a, T>(&'a mut [T]);

        impl<T> Drop for Dropper<'_, T> {
            fn drop(&mut self) {
                unsafe {
                    ptr::drop_in_place(self.0);
                }
            }
        }

        // Safe because:
        //
        // * Any slice passed to `drop_in_place` is valid; the second case has
        //   `len <= front.len()` and returning on `len > self.len()` ensures
        //   `begin <= back.len()` in the first case
        // * The head of the VecDeque is moved before calling `drop_in_place`,
        //   so no value is dropped twice if `drop_in_place` panics
        unsafe {
            if len >= self.len {
                return;
            }

            let (front, back) = self.as_mut_slices();
            if len > front.len() {
                let begin = len - front.len();
                let drop_back = back.get_unchecked_mut(begin..) as *mut _;
                self.len = len;
                ptr::drop_in_place(drop_back);
            } else {
                let drop_back = back as *mut _;
                let drop_front = front.get_unchecked_mut(len..) as *mut _;
                self.len = len;

                // Make sure the second half is dropped even when a destructor
                // in the first one panics.
                let _back_dropper = Dropper(&mut *drop_back);
                ptr::drop_in_place(drop_front);
            }
        }
    }

    /// Returns a reference to the underlying allocator.
    #[inline]
    pub fn allocator(&self) -> &A {
        self.buf.allocator()
    }

    /// Returns a front-to-back iterator.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::{Vec, VecDeque};
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_push_back(5)?;
    /// buf.try_push_back(3)?;
    /// buf.try_push_back(4)?;
    /// let b: &[_] = &[&5, &3, &4];
    /// let c: Vec<&i32> = buf.iter().try_collect()?;
    /// assert_eq!(&c[..], b);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn iter(&self) -> Iter<'_, T> {
        let (a, b) = self.as_slices();
        Iter::new(a.iter(), b.iter())
    }

    /// Returns a raw front-to-back iterator.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the iterator doesn't outlive `self`.
    pub unsafe fn raw_iter(&self) -> RawIter<T> {
        let (a, b) = self.as_slices();
        RawIter::new(crate::slice::RawIter::new(a), crate::slice::RawIter::new(b))
    }

    /// Returns a front-to-back iterator that returns mutable references.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_push_back(5)?;
    /// buf.try_push_back(3)?;
    /// buf.try_push_back(4)?;
    /// for num in buf.iter_mut() {
    ///     *num = *num - 2;
    /// }
    /// let b: &[_] = &[&mut 3, &mut 1, &mut 2];
    /// assert_eq!(&buf.iter_mut().collect::<Vec<&mut i32>>()[..], b);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        let (a, b) = self.as_mut_slices();
        IterMut::new(a.iter_mut(), b.iter_mut())
    }

    /// Returns a pair of slices which contain, in order, the contents of the
    /// deque.
    ///
    /// If [`make_contiguous`] was previously called, all elements of the
    /// deque will be in the first slice and the second slice will be empty.
    ///
    /// [`make_contiguous`]: VecDeque::make_contiguous
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque = VecDeque::new();
    ///
    /// deque.try_push_back(0)?;
    /// deque.try_push_back(1)?;
    /// deque.try_push_back(2)?;
    ///
    /// assert_eq!(deque.as_slices(), (&[0, 1, 2][..], &[][..]));
    ///
    /// deque.try_push_front(10)?;
    /// deque.try_push_front(9)?;
    ///
    /// assert_eq!(deque.as_slices(), (&[9, 10][..], &[0, 1, 2][..]));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn as_slices(&self) -> (&[T], &[T]) {
        let (a_range, b_range) = self.slice_ranges(.., self.len);
        // SAFETY: `slice_ranges` always returns valid ranges into
        // the physical buffer.
        unsafe { (&*self.buffer_range(a_range), &*self.buffer_range(b_range)) }
    }

    /// Returns a pair of slices which contain, in order, the contents of the
    /// deque.
    ///
    /// If [`make_contiguous`] was previously called, all elements of the
    /// deque will be in the first slice and the second slice will be empty.
    ///
    /// [`make_contiguous`]: VecDeque::make_contiguous
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque = VecDeque::new();
    ///
    /// deque.try_push_back(0)?;
    /// deque.try_push_back(1)?;
    ///
    /// deque.try_push_front(10)?;
    /// deque.try_push_front(9)?;
    ///
    /// deque.as_mut_slices().0[0] = 42;
    /// deque.as_mut_slices().1[0] = 24;
    /// assert_eq!(deque.as_slices(), (&[42, 10][..], &[24, 1][..]));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn as_mut_slices(&mut self) -> (&mut [T], &mut [T]) {
        let (a_range, b_range) = self.slice_ranges(.., self.len);
        // SAFETY: `slice_ranges` always returns valid ranges into
        // the physical buffer.
        unsafe {
            (
                &mut *self.buffer_range(a_range),
                &mut *self.buffer_range(b_range),
            )
        }
    }

    /// Returns the number of elements in the deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque = VecDeque::new();
    /// assert_eq!(deque.len(), 0);
    /// deque.try_push_back(1)?;
    /// assert_eq!(deque.len(), 1);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the deque is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque = VecDeque::new();
    /// assert!(deque.is_empty());
    /// deque.try_push_front(1)?;
    /// assert!(!deque.is_empty());
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Given a range into the logical buffer of the deque, this function
    /// return two ranges into the physical buffer that correspond to
    /// the given range. The `len` parameter should usually just be `self.len`;
    /// the reason it's passed explicitly is that if the deque is wrapped in a
    /// `Drain`, then `self.len` is not actually the length of the deque.
    ///
    /// # Safety
    ///
    /// This function is always safe to call. For the resulting ranges to be
    /// valid ranges into the physical buffer, the caller must ensure that the
    /// result of calling `slice::range(range, ..len)` represents a valid range
    /// into the logical buffer, and that all elements in that range are
    /// initialized.
    fn slice_ranges<R>(&self, range: R, len: usize) -> (Range<usize>, Range<usize>)
    where
        R: RangeBounds<usize>,
    {
        let Range { start, end } = slice_range(range, ..len);
        let len = end - start;

        if len == 0 {
            (0..0, 0..0)
        } else {
            // `slice_range` guarantees that `start <= end <= len`.
            // because `len != 0`, we know that `start < end`, so `start < len`
            // and the indexing is valid.
            let wrapped_start = self.to_physical_idx(start);

            // this subtraction can never overflow because `wrapped_start` is
            // at most `self.capacity()` (and if `self.capacity != 0`, then `wrapped_start` is strictly less
            // than `self.capacity`).
            let head_len = self.capacity() - wrapped_start;

            if head_len >= len {
                // we know that `len + wrapped_start <= self.capacity <= usize::MAX`, so this addition can't overflow
                (wrapped_start..wrapped_start + len, 0..0)
            } else {
                // can't overflow because of the if condition
                let tail_len = len - head_len;
                (wrapped_start..self.capacity(), 0..tail_len)
            }
        }
    }

    /// Creates an iterator that covers the specified range in the deque.
    ///
    /// # Panics
    ///
    /// Panics if the starting point is greater than the end point or if
    /// the end point is greater than the length of the deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let deque: VecDeque<_> = [1, 2, 3].try_into()?;
    /// let range = deque.range(2..).copied().try_collect::<VecDeque<_>>()?;
    /// assert_eq!(range, [3]);
    ///
    /// // A full range covers all contents
    /// let all = deque.range(..);
    /// assert_eq!(all.len(), 3);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn range<R>(&self, range: R) -> Iter<'_, T>
    where
        R: RangeBounds<usize>,
    {
        let (a_range, b_range) = self.slice_ranges(range, self.len);
        // SAFETY: The ranges returned by `slice_ranges`
        // are valid ranges into the physical buffer, so
        // it's ok to pass them to `buffer_range` and
        // dereference the result.
        let a = unsafe { &*self.buffer_range(a_range) };
        let b = unsafe { &*self.buffer_range(b_range) };
        Iter::new(a.iter(), b.iter())
    }

    /// Creates an iterator that covers the specified mutable range in the deque.
    ///
    /// # Panics
    ///
    /// Panics if the starting point is greater than the end point or if
    /// the end point is greater than the length of the deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque: VecDeque<_> = [1, 2, 3].try_into()?;
    /// for v in deque.range_mut(2..) {
    ///   *v *= 2;
    /// }
    /// assert_eq!(deque, [1, 2, 6]);
    ///
    /// // A full range covers all contents
    /// for v in deque.range_mut(..) {
    ///   *v *= 2;
    /// }
    /// assert_eq!(deque, [2, 4, 12]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn range_mut<R>(&mut self, range: R) -> IterMut<'_, T>
    where
        R: RangeBounds<usize>,
    {
        let (a_range, b_range) = self.slice_ranges(range, self.len);
        // SAFETY: The ranges returned by `slice_ranges`
        // are valid ranges into the physical buffer, so
        // it's ok to pass them to `buffer_range` and
        // dereference the result.
        let a = unsafe { &mut *self.buffer_range(a_range) };
        let b = unsafe { &mut *self.buffer_range(b_range) };
        IterMut::new(a.iter_mut(), b.iter_mut())
    }

    /// Removes the specified range from the deque in bulk, returning all
    /// removed elements as an iterator. If the iterator is dropped before
    /// being fully consumed, it drops the remaining removed elements.
    ///
    /// The returned iterator keeps a mutable borrow on the queue to optimize
    /// its implementation.
    ///
    ///
    /// # Panics
    ///
    /// Panics if the starting point is greater than the end point or if
    /// the end point is greater than the length of the deque.
    ///
    /// # Leaking
    ///
    /// If the returned iterator goes out of scope without being dropped (due to
    /// [`mem::forget`], for example), the deque may have lost and leaked
    /// elements arbitrarily, including elements outside the range.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut deque: VecDeque<_> = [1, 2, 3].try_into()?;
    /// let drained = deque.drain(2..).try_collect::<VecDeque<_>>()?;
    /// assert_eq!(drained, [3]);
    /// assert_eq!(deque, [1, 2]);
    ///
    /// // A full range clears all contents, like `clear()` does
    /// deque.drain(..);
    /// assert!(deque.is_empty());
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn drain<R>(&mut self, range: R) -> Drain<'_, T, A>
    where
        R: RangeBounds<usize>,
    {
        // Memory safety
        //
        // When the Drain is first created, the source deque is shortened to
        // make sure no uninitialized or moved-from elements are accessible at
        // all if the Drain's destructor never gets to run.
        //
        // Drain will ptr::read out the values to remove.
        // When finished, the remaining data will be copied back to cover the hole,
        // and the head/tail values will be restored correctly.
        //
        let Range { start, end } = slice_range(range, ..self.len);
        let drain_start = start;
        let drain_len = end - start;

        // The deque's elements are parted into three segments:
        // * 0  -> drain_start
        // * drain_start -> drain_start+drain_len
        // * drain_start+drain_len -> self.len
        //
        // H = self.head; T = self.head+self.len; t = drain_start+drain_len; h = drain_head
        //
        // We store drain_start as self.len, and drain_len and self.len as
        // drain_len and orig_len respectively on the Drain. This also
        // truncates the effective array such that if the Drain is leaked, we
        // have forgotten about the potentially moved values after the start of
        // the drain.
        //
        //        H   h   t   T
        // [. . . o o x x o o . . .]
        //
        // "forget" about the values after the start of the drain until after
        // the drain is complete and the Drain destructor is run.

        unsafe { Drain::new(self, drain_start, drain_len) }
    }

    /// Clears the deque, removing all values.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque = VecDeque::new();
    /// deque.try_push_back(1)?;
    /// deque.clear();
    /// assert!(deque.is_empty());
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn clear(&mut self) {
        self.truncate(0);
        // Not strictly necessary, but leaves things in a more consistent/predictable state.
        self.head = 0;
    }

    /// Returns `true` if the deque contains an element equal to the
    /// given value.
    ///
    /// This operation is *O*(*n*).
    ///
    /// Note that if you have a sorted `VecDeque`, [`binary_search`] may be faster.
    ///
    /// [`binary_search`]: VecDeque::binary_search
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque: VecDeque<u32> = VecDeque::new();
    ///
    /// deque.try_push_back(0)?;
    /// deque.try_push_back(1)?;
    ///
    /// assert_eq!(deque.contains(&1), true);
    /// assert_eq!(deque.contains(&10), false);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn contains(&self, x: &T) -> bool
    where
        T: PartialEq<T>,
    {
        let (a, b) = self.as_slices();
        a.contains(x) || b.contains(x)
    }

    /// Provides a reference to the front element, or `None` if the deque is
    /// empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut d = VecDeque::new();
    /// assert_eq!(d.front(), None);
    ///
    /// d.try_push_back(1)?;
    /// d.try_push_back(2)?;
    /// assert_eq!(d.front(), Some(&1));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn front(&self) -> Option<&T> {
        self.get(0)
    }

    /// Provides a mutable reference to the front element, or `None` if the
    /// deque is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut d = VecDeque::new();
    /// assert_eq!(d.front_mut(), None);
    ///
    /// d.try_push_back(1)?;
    /// d.try_push_back(2)?;
    /// match d.front_mut() {
    ///     Some(x) => *x = 9,
    ///     None => (),
    /// }
    /// assert_eq!(d.front(), Some(&9));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn front_mut(&mut self) -> Option<&mut T> {
        self.get_mut(0)
    }

    /// Provides a reference to the back element, or `None` if the deque is
    /// empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut d = VecDeque::new();
    /// assert_eq!(d.back(), None);
    ///
    /// d.try_push_back(1)?;
    /// d.try_push_back(2)?;
    /// assert_eq!(d.back(), Some(&2));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn back(&self) -> Option<&T> {
        self.get(self.len.wrapping_sub(1))
    }

    /// Provides a mutable reference to the back element, or `None` if the
    /// deque is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut d = VecDeque::new();
    /// assert_eq!(d.back(), None);
    ///
    /// d.try_push_back(1)?;
    /// d.try_push_back(2)?;
    /// match d.back_mut() {
    ///     Some(x) => *x = 9,
    ///     None => (),
    /// }
    /// assert_eq!(d.back(), Some(&9));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn back_mut(&mut self) -> Option<&mut T> {
        self.get_mut(self.len.wrapping_sub(1))
    }

    /// Removes the first element and returns it, or `None` if the deque is
    /// empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut d = VecDeque::new();
    /// d.try_push_back(1)?;
    /// d.try_push_back(2)?;
    ///
    /// assert_eq!(d.pop_front(), Some(1));
    /// assert_eq!(d.pop_front(), Some(2));
    /// assert_eq!(d.pop_front(), None);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn pop_front(&mut self) -> Option<T> {
        if self.is_empty() {
            None
        } else {
            let old_head = self.head;
            self.head = self.to_physical_idx(1);
            self.len -= 1;
            Some(unsafe { self.buffer_read(old_head) })
        }
    }

    /// Removes the last element from the deque and returns it, or `None` if
    /// it is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// assert_eq!(buf.pop_back(), None);
    /// buf.try_push_back(1)?;
    /// buf.try_push_back(3)?;
    /// assert_eq!(buf.pop_back(), Some(3));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn pop_back(&mut self) -> Option<T> {
        if self.is_empty() {
            None
        } else {
            self.len -= 1;
            Some(unsafe { self.buffer_read(self.to_physical_idx(self.len)) })
        }
    }

    /// Prepends an element to the deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut d = VecDeque::new();
    /// d.try_push_front(1)?;
    /// d.try_push_front(2)?;
    /// assert_eq!(d.front(), Some(&2));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_push_front(&mut self, value: T) -> Result<(), Error> {
        if self.is_full() {
            self.try_grow()?;
        }

        self.head = self.wrap_sub(self.head, 1);
        self.len += 1;

        unsafe {
            self.buffer_write(self.head, value);
        }

        Ok(())
    }

    /// Appends an element to the back of the deque.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_push_back(1)?;
    /// buf.try_push_back(3)?;
    /// assert_eq!(3, *buf.back().unwrap());
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_push_back(&mut self, value: T) -> Result<(), Error> {
        if self.is_full() {
            self.try_grow()?;
        }

        unsafe { self.buffer_write(self.to_physical_idx(self.len), value) }
        self.len += 1;
        Ok(())
    }

    #[inline]
    fn is_contiguous(&self) -> bool {
        // Do the calculation like this to avoid overflowing if len + head > usize::MAX
        self.head <= self.capacity() - self.len
    }

    /// Removes an element from anywhere in the deque and returns it,
    /// replacing it with the first element.
    ///
    /// This does not preserve ordering, but is *O*(1).
    ///
    /// Returns `None` if `index` is out of bounds.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// assert_eq!(buf.swap_remove_front(0), None);
    /// buf.try_push_back(1)?;
    /// buf.try_push_back(2)?;
    /// buf.try_push_back(3)?;
    /// assert_eq!(buf, [1, 2, 3]);
    ///
    /// assert_eq!(buf.swap_remove_front(2), Some(3));
    /// assert_eq!(buf, [2, 1]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn swap_remove_front(&mut self, index: usize) -> Option<T> {
        let length = self.len;
        if index < length && index != 0 {
            self.swap(index, 0);
        } else if index >= length {
            return None;
        }
        self.pop_front()
    }

    /// Removes an element from anywhere in the deque and returns it,
    /// replacing it with the last element.
    ///
    /// This does not preserve ordering, but is *O*(1).
    ///
    /// Returns `None` if `index` is out of bounds.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// assert_eq!(buf.swap_remove_back(0), None);
    /// buf.try_push_back(1)?;
    /// buf.try_push_back(2)?;
    /// buf.try_push_back(3)?;
    /// assert_eq!(buf, [1, 2, 3]);
    ///
    /// assert_eq!(buf.swap_remove_back(0), Some(1));
    /// assert_eq!(buf, [3, 2]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn swap_remove_back(&mut self, index: usize) -> Option<T> {
        let length = self.len;
        if length > 0 && index < length - 1 {
            self.swap(index, length - 1);
        } else if index >= length {
            return None;
        }
        self.pop_back()
    }

    /// Inserts an element at `index` within the deque, shifting all elements
    /// with indices greater than or equal to `index` towards the back.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Panics
    ///
    /// Panics if `index` is greater than deque's length
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut vec_deque = VecDeque::new();
    /// vec_deque.try_push_back('a')?;
    /// vec_deque.try_push_back('b')?;
    /// vec_deque.try_push_back('c')?;
    /// assert_eq!(vec_deque, &['a', 'b', 'c']);
    ///
    /// vec_deque.try_insert(1, 'd')?;
    /// assert_eq!(vec_deque, &['a', 'd', 'b', 'c']);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_insert(&mut self, index: usize, value: T) -> Result<(), Error> {
        assert!(index <= self.len(), "index out of bounds");

        if self.is_full() {
            self.try_grow()?;
        }

        let k = self.len - index;

        if k < index {
            // `index + 1` can't overflow, because if index was usize::MAX, then either the
            // assert would've failed, or the deque would've tried to grow past usize::MAX
            // and panicked.
            unsafe {
                // see `remove()` for explanation why this wrap_copy() call is safe.
                self.wrap_copy(
                    self.to_physical_idx(index),
                    self.to_physical_idx(index + 1),
                    k,
                );
                self.buffer_write(self.to_physical_idx(index), value);
                self.len += 1;
            }
        } else {
            let old_head = self.head;
            self.head = self.wrap_sub(self.head, 1);
            unsafe {
                self.wrap_copy(old_head, self.head, index);
                self.buffer_write(self.to_physical_idx(index), value);
                self.len += 1;
            }
        }

        Ok(())
    }

    /// Removes and returns the element at `index` from the deque.
    /// Whichever end is closer to the removal point will be moved to make
    /// room, and all the affected elements will be moved to new positions.
    /// Returns `None` if `index` is out of bounds.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_push_back(1)?;
    /// buf.try_push_back(2)?;
    /// buf.try_push_back(3)?;
    /// assert_eq!(buf, [1, 2, 3]);
    ///
    /// assert_eq!(buf.remove(1), Some(2));
    /// assert_eq!(buf, [1, 3]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if self.len <= index {
            return None;
        }

        let wrapped_idx = self.to_physical_idx(index);

        let elem = unsafe { Some(self.buffer_read(wrapped_idx)) };

        let k = self.len - index - 1;
        // safety: due to the nature of the if-condition, whichever wrap_copy gets called,
        // its length argument will be at most `self.len / 2`, so there can't be more than
        // one overlapping area.
        if k < index {
            unsafe { self.wrap_copy(self.wrap_add(wrapped_idx, 1), wrapped_idx, k) };
            self.len -= 1;
        } else {
            let old_head = self.head;
            self.head = self.to_physical_idx(1);
            unsafe { self.wrap_copy(old_head, self.head, index) };
            self.len -= 1;
        }

        elem
    }

    /// Splits the deque into two at the given index.
    ///
    /// Returns a newly allocated `VecDeque`. `self` contains elements `[0, at)`,
    /// and the returned deque contains elements `[at, len)`.
    ///
    /// Note that the capacity of `self` does not change.
    ///
    /// Element at index 0 is the front of the queue.
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf: VecDeque<_> = [1, 2, 3].try_into()?;
    /// let buf2 = buf.try_split_off(1)?;
    /// assert_eq!(buf, [1]);
    /// assert_eq!(buf2, [2, 3]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    #[must_use = "use `.truncate()` if you don't need the other half"]
    pub fn try_split_off(&mut self, at: usize) -> Result<Self, Error>
    where
        A: Clone,
    {
        let len = self.len;
        assert!(at <= len, "`at` out of bounds");

        let other_len = len - at;
        let mut other = VecDeque::try_with_capacity_in(other_len, self.allocator().clone())?;

        unsafe {
            let (first_half, second_half) = self.as_slices();

            let first_len = first_half.len();
            let second_len = second_half.len();
            if at < first_len {
                // `at` lies in the first half.
                let amount_in_first = first_len - at;

                ptr::copy_nonoverlapping(first_half.as_ptr().add(at), other.ptr(), amount_in_first);

                // just take all of the second half.
                ptr::copy_nonoverlapping(
                    second_half.as_ptr(),
                    other.ptr().add(amount_in_first),
                    second_len,
                );
            } else {
                // `at` lies in the second half, need to factor in the elements we skipped
                // in the first half.
                let offset = at - first_len;
                let amount_in_second = second_len - offset;
                ptr::copy_nonoverlapping(
                    second_half.as_ptr().add(offset),
                    other.ptr(),
                    amount_in_second,
                );
            }
        }

        // Cleanup where the ends of the buffers are
        self.len = at;
        other.len = other_len;

        Ok(other)
    }

    /// Moves all the elements of `other` into `self`, leaving `other` empty.
    ///
    /// # Panics
    ///
    /// Panics if the new number of elements in self overflows a `usize`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf: VecDeque<_> = [1, 2].try_into()?;
    /// let mut buf2: VecDeque<_> = [3, 4].try_into()?;
    /// buf.try_append(&mut buf2)?;
    /// assert_eq!(buf, [1, 2, 3, 4]);
    /// assert!(buf2.is_empty());
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn try_append(&mut self, other: &mut Self) -> Result<(), Error> {
        if T::IS_ZST {
            self.len = self
                .len
                .checked_add(other.len)
                .ok_or(Error::CapacityOverflow)?;
            other.len = 0;
            other.head = 0;
            return Ok(());
        }

        self.try_reserve(other.len)?;

        unsafe {
            let (left, right) = other.as_slices();
            self.copy_slice(self.to_physical_idx(self.len), left);
            // no overflow, because self.capacity() >= old_cap + left.len() >= self.len + left.len()
            self.copy_slice(self.to_physical_idx(self.len + left.len()), right);
        }

        // SAFETY: Update pointers after copying to avoid leaving doppelganger
        // in case of panics.
        self.len += other.len;
        // Now that we own its values, forget everything in `other`.
        other.len = 0;
        other.head = 0;
        Ok(())
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// In other words, remove all elements `e` for which `f(&e)` returns false.
    /// This method operates in place, visiting each element exactly once in the
    /// original order, and preserves the order of the retained elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_extend(1..5)?;
    /// buf.retain(|&x| x % 2 == 0);
    /// assert_eq!(buf, [2, 4]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    ///
    /// Because the elements are visited exactly once in the original order,
    /// external state may be used to decide which elements to keep.
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_extend(1..6)?;
    ///
    /// let keep = [false, true, true, false, true];
    /// let mut iter = keep.iter();
    /// buf.retain(|_| *iter.next().unwrap());
    /// assert_eq!(buf, [2, 3, 5]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.retain_mut(|elem| f(elem));
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// In other words, remove all elements `e` for which `f(&e)` returns false.
    /// This method operates in place, visiting each element exactly once in the
    /// original order, and preserves the order of the retained elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_extend(1..5)?;
    /// buf.retain_mut(|x| if *x % 2 == 0 {
    ///     *x += 1;
    ///     true
    /// } else {
    ///     false
    /// });
    /// assert_eq!(buf, [3, 5]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn retain_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut T) -> bool,
    {
        let len = self.len;
        let mut idx = 0;
        let mut cur = 0;

        // Stage 1: All values are retained.
        while cur < len {
            if !f(&mut self[cur]) {
                cur += 1;
                break;
            }
            cur += 1;
            idx += 1;
        }
        // Stage 2: Swap retained value into current idx.
        while cur < len {
            if !f(&mut self[cur]) {
                cur += 1;
                continue;
            }

            self.swap(idx, cur);
            cur += 1;
            idx += 1;
        }
        // Stage 3: Truncate all values after idx.
        if cur != idx {
            self.truncate(idx);
        }
    }

    // Double the buffer size. This method is inline(never), so we expect it to only
    // be called in cold paths.
    // This may panic or abort
    #[inline(never)]
    fn try_grow(&mut self) -> Result<(), Error> {
        // Extend or possibly remove this assertion when valid use-cases for growing the
        // buffer without it being full emerge
        debug_assert!(self.is_full());
        let old_cap = self.capacity();
        self.buf.try_reserve_for_push(old_cap)?;
        unsafe {
            self.handle_capacity_increase(old_cap);
        }
        debug_assert!(!self.is_full());
        Ok(())
    }

    /// Modifies the deque in-place so that `len()` is equal to `new_len`,
    /// either by removing excess elements from the back or by appending
    /// elements generated by calling `generator` to the back.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_push_back(5)?;
    /// buf.try_push_back(10)?;
    /// buf.try_push_back(15)?;
    /// assert_eq!(buf, [5, 10, 15]);
    ///
    /// buf.try_resize_with(5, Default::default)?;
    /// assert_eq!(buf, [5, 10, 15, 0, 0]);
    ///
    /// buf.try_resize_with(2, || unreachable!())?;
    /// assert_eq!(buf, [5, 10]);
    ///
    /// let mut state = 100;
    /// buf.try_resize_with(5, || { state += 1; state })?;
    /// assert_eq!(buf, [5, 10, 101, 102, 103]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_resize_with(
        &mut self,
        new_len: usize,
        mut generator: impl FnMut() -> T,
    ) -> Result<(), Error> {
        let len = self.len;

        if new_len > len {
            for _ in 0..new_len - len {
                self.try_push_back(generator())?;
            }
        } else {
            self.truncate(new_len);
        }

        Ok(())
    }

    /// Rearranges the internal storage of this deque so it is one contiguous
    /// slice, which is then returned.
    ///
    /// This method does not allocate and does not change the order of the
    /// inserted elements. As it returns a mutable slice, this can be used to
    /// sort a deque.
    ///
    /// Once the internal storage is contiguous, the [`as_slices`] and
    /// [`as_mut_slices`] methods will return the entire contents of the
    /// deque in a single slice.
    ///
    /// [`as_slices`]: VecDeque::as_slices
    /// [`as_mut_slices`]: VecDeque::as_mut_slices
    ///
    /// # Examples
    ///
    /// Sorting the content of a deque.
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::try_with_capacity(15)?;
    ///
    /// buf.try_push_back(2)?;
    /// buf.try_push_back(1)?;
    /// buf.try_push_front(3)?;
    ///
    /// // sorting the deque
    /// buf.make_contiguous().sort();
    /// assert_eq!(buf.as_slices(), (&[1, 2, 3] as &[_], &[] as &[_]));
    ///
    /// // sorting it in reverse order
    /// buf.make_contiguous().sort_by(|a, b| b.cmp(a));
    /// assert_eq!(buf.as_slices(), (&[3, 2, 1] as &[_], &[] as &[_]));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    ///
    /// Getting immutable access to the contiguous slice.
    ///
    /// ```rust
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    ///
    /// buf.try_push_back(2)?;
    /// buf.try_push_back(1)?;
    /// buf.try_push_front(3)?;
    ///
    /// buf.make_contiguous();
    /// if let (slice, &[]) = buf.as_slices() {
    ///     // we can now be sure that `slice` contains all elements of the deque,
    ///     // while still having immutable access to `buf`.
    ///     assert_eq!(buf.len(), slice.len());
    ///     assert_eq!(slice, &[3, 2, 1] as &[_]);
    /// }
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn make_contiguous(&mut self) -> &mut [T] {
        if T::IS_ZST {
            self.head = 0;
        }

        if self.is_contiguous() {
            unsafe { return slice::from_raw_parts_mut(self.ptr().add(self.head), self.len) }
        }

        let &mut Self { head, len, .. } = self;
        let ptr = self.ptr();
        let cap = self.capacity();

        let free = cap - len;
        let head_len = cap - head;
        let tail = len - head_len;
        let tail_len = tail;

        if free >= head_len {
            // there is enough free space to copy the head in one go,
            // this means that we first shift the tail backwards, and then
            // copy the head to the correct position.
            //
            // from: DEFGH....ABC
            // to:   ABCDEFGH....
            unsafe {
                self.copy(0, head_len, tail_len);
                // ...DEFGH.ABC
                self.copy_nonoverlapping(head, 0, head_len);
                // ABCDEFGH....
            }

            self.head = 0;
        } else if free >= tail_len {
            // there is enough free space to copy the tail in one go,
            // this means that we first shift the head forwards, and then
            // copy the tail to the correct position.
            //
            // from: FGH....ABCDE
            // to:   ...ABCDEFGH.
            unsafe {
                self.copy(head, tail, head_len);
                // FGHABCDE....
                self.copy_nonoverlapping(0, tail + head_len, tail_len);
                // ...ABCDEFGH.
            }

            self.head = tail;
        } else {
            // `free` is smaller than both `head_len` and `tail_len`.
            // the general algorithm for this first moves the slices
            // right next to each other and then uses `slice::rotate`
            // to rotate them into place:
            //
            // initially:   HIJK..ABCDEFG
            // step 1:      ..HIJKABCDEFG
            // step 2:      ..ABCDEFGHIJK
            //
            // or:
            //
            // initially:   FGHIJK..ABCDE
            // step 1:      FGHIJKABCDE..
            // step 2:      ABCDEFGHIJK..

            // pick the shorter of the 2 slices to reduce the amount
            // of memory that needs to be moved around.
            if head_len > tail_len {
                // tail is shorter, so:
                //  1. copy tail forwards
                //  2. rotate used part of the buffer
                //  3. update head to point to the new beginning (which is just `free`)

                unsafe {
                    // if there is no free space in the buffer, then the slices are already
                    // right next to each other and we don't need to move any memory.
                    if free != 0 {
                        // because we only move the tail forward as much as there's free space
                        // behind it, we don't overwrite any elements of the head slice, and
                        // the slices end up right next to each other.
                        self.copy(0, free, tail_len);
                    }

                    // We just copied the tail right next to the head slice,
                    // so all of the elements in the range are initialized
                    let slice = &mut *self.buffer_range(free..self.capacity());

                    // because the deque wasn't contiguous, we know that `tail_len < self.len == slice.len()`,
                    // so this will never panic.
                    slice.rotate_left(tail_len);

                    // the used part of the buffer now is `free..self.capacity()`, so set
                    // `head` to the beginning of that range.
                    self.head = free;
                }
            } else {
                // head is shorter so:
                //  1. copy head backwards
                //  2. rotate used part of the buffer
                //  3. update head to point to the new beginning (which is the beginning of the buffer)

                unsafe {
                    // if there is no free space in the buffer, then the slices are already
                    // right next to each other and we don't need to move any memory.
                    if free != 0 {
                        // copy the head slice to lie right behind the tail slice.
                        self.copy(self.head, tail_len, head_len);
                    }

                    // because we copied the head slice so that both slices lie right
                    // next to each other, all the elements in the range are initialized.
                    let slice = &mut *self.buffer_range(0..self.len);

                    // because the deque wasn't contiguous, we know that `head_len < self.len == slice.len()`
                    // so this will never panic.
                    slice.rotate_right(head_len);

                    // the used part of the buffer now is `0..self.len`, so set
                    // `head` to the beginning of that range.
                    self.head = 0;
                }
            }
        }

        unsafe { slice::from_raw_parts_mut(ptr.add(self.head), self.len) }
    }

    /// Rotates the double-ended queue `mid` places to the left.
    ///
    /// Equivalently,
    /// - Rotates item `mid` into the first position.
    /// - Pops the first `mid` items and pushes them to the end.
    /// - Rotates `len() - mid` places to the right.
    ///
    /// # Panics
    ///
    /// If `mid` is greater than `len()`. Note that `mid == len()`
    /// does _not_ panic and is a no-op rotation.
    ///
    /// # Complexity
    ///
    /// Takes `*O*(min(mid, len() - mid))` time and no extra space.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf: VecDeque<_> = (0..10).try_collect()?;
    ///
    /// buf.rotate_left(3);
    /// assert_eq!(buf, [3, 4, 5, 6, 7, 8, 9, 0, 1, 2]);
    ///
    /// for i in 1..10 {
    ///     assert_eq!(i * 3 % 10, buf[0]);
    ///     buf.rotate_left(3);
    /// }
    /// assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn rotate_left(&mut self, mid: usize) {
        assert!(mid <= self.len());
        let k = self.len - mid;
        if mid <= k {
            unsafe { self.rotate_left_inner(mid) }
        } else {
            unsafe { self.rotate_right_inner(k) }
        }
    }

    /// Rotates the double-ended queue `k` places to the right.
    ///
    /// Equivalently,
    /// - Rotates the first item into position `k`.
    /// - Pops the last `k` items and pushes them to the front.
    /// - Rotates `len() - k` places to the left.
    ///
    /// # Panics
    ///
    /// If `k` is greater than `len()`. Note that `k == len()`
    /// does _not_ panic and is a no-op rotation.
    ///
    /// # Complexity
    ///
    /// Takes `*O*(min(k, len() - k))` time and no extra space.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    /// use rune::alloc::prelude::*;
    ///
    /// let mut buf: VecDeque<_> = (0..10).try_collect()?;
    ///
    /// buf.rotate_right(3);
    /// assert_eq!(buf, [7, 8, 9, 0, 1, 2, 3, 4, 5, 6]);
    ///
    /// for i in 1..10 {
    ///     assert_eq!(0, buf[i * 3 % 10]);
    ///     buf.rotate_right(3);
    /// }
    /// assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn rotate_right(&mut self, k: usize) {
        assert!(k <= self.len());
        let mid = self.len - k;
        if k <= mid {
            unsafe { self.rotate_right_inner(k) }
        } else {
            unsafe { self.rotate_left_inner(mid) }
        }
    }

    // SAFETY: the following two methods require that the rotation amount
    // be less than half the length of the deque.
    //
    // `wrap_copy` requires that `min(x, capacity() - x) + copy_len <= capacity()`,
    // but then `min` is never more than half the capacity, regardless of x,
    // so it's sound to call here because we're calling with something
    // less than half the length, which is never above half the capacity.

    unsafe fn rotate_left_inner(&mut self, mid: usize) {
        debug_assert!(mid * 2 <= self.len());
        unsafe {
            self.wrap_copy(self.head, self.to_physical_idx(self.len), mid);
        }
        self.head = self.to_physical_idx(mid);
    }

    unsafe fn rotate_right_inner(&mut self, k: usize) {
        debug_assert!(k * 2 <= self.len());
        self.head = self.wrap_sub(self.head, k);
        unsafe {
            self.wrap_copy(self.to_physical_idx(self.len), self.head, k);
        }
    }

    /// Binary searches this `VecDeque` for a given element.
    /// If the `VecDeque` is not sorted, the returned result is unspecified and
    /// meaningless.
    ///
    /// If the value is found then [`Result::Ok`] is returned, containing the
    /// index of the matching element. If there are multiple matches, then any
    /// one of the matches could be returned. If the value is not found then
    /// [`Result::Err`] is returned, containing the index where a matching
    /// element could be inserted while maintaining sorted order.
    ///
    /// See also [`binary_search_by`], [`binary_search_by_key`], and [`partition_point`].
    ///
    /// [`binary_search_by`]: VecDeque::binary_search_by
    /// [`binary_search_by_key`]: VecDeque::binary_search_by_key
    /// [`partition_point`]: VecDeque::partition_point
    ///
    /// # Examples
    ///
    /// Looks up a series of four elements. The first is found, with a
    /// uniquely determined position; the second and third are not
    /// found; the fourth could match any position in `[1, 4]`.
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<_> = [0, 1, 1, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55].try_into()?;
    ///
    /// assert_eq!(deque.binary_search(&13),  Ok(9));
    /// assert_eq!(deque.binary_search(&4),   Err(7));
    /// assert_eq!(deque.binary_search(&100), Err(13));
    /// let r = deque.binary_search(&1);
    /// assert!(matches!(r, Ok(1..=4)));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    ///
    /// If you want to insert an item to a sorted deque, while maintaining
    /// sort order, consider using [`partition_point`]:
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque: VecDeque<_> = [0, 1, 1, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55].try_into()?;
    /// let num = 42;
    /// let idx = deque.partition_point(|&x| x < num);
    /// // The above is equivalent to `let idx = deque.binary_search(&num).unwrap_or_else(|x| x);`
    /// deque.try_insert(idx, num)?;
    /// assert_eq!(deque, &[0, 1, 1, 1, 1, 2, 3, 5, 8, 13, 21, 34, 42, 55]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn binary_search(&self, x: &T) -> Result<usize, usize>
    where
        T: Ord,
    {
        self.binary_search_by(|e| e.cmp(x))
    }

    /// Binary searches this `VecDeque` with a comparator function.
    ///
    /// The comparator function should return an order code that indicates
    /// whether its argument is `Less`, `Equal` or `Greater` the desired
    /// target.
    /// If the `VecDeque` is not sorted or if the comparator function does not
    /// implement an order consistent with the sort order of the underlying
    /// `VecDeque`, the returned result is unspecified and meaningless.
    ///
    /// If the value is found then [`Result::Ok`] is returned, containing the
    /// index of the matching element. If there are multiple matches, then any
    /// one of the matches could be returned. If the value is not found then
    /// [`Result::Err`] is returned, containing the index where a matching
    /// element could be inserted while maintaining sorted order.
    ///
    /// See also [`binary_search`], [`binary_search_by_key`], and [`partition_point`].
    ///
    /// [`binary_search`]: VecDeque::binary_search
    /// [`binary_search_by_key`]: VecDeque::binary_search_by_key
    /// [`partition_point`]: VecDeque::partition_point
    ///
    /// # Examples
    ///
    /// Looks up a series of four elements. The first is found, with a
    /// uniquely determined position; the second and third are not
    /// found; the fourth could match any position in `[1, 4]`.
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<_> = [0, 1, 1, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55].try_into()?;
    ///
    /// assert_eq!(deque.binary_search_by(|x| x.cmp(&13)),  Ok(9));
    /// assert_eq!(deque.binary_search_by(|x| x.cmp(&4)),   Err(7));
    /// assert_eq!(deque.binary_search_by(|x| x.cmp(&100)), Err(13));
    /// let r = deque.binary_search_by(|x| x.cmp(&1));
    /// assert!(matches!(r, Ok(1..=4)));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn binary_search_by<'a, F>(&'a self, mut f: F) -> Result<usize, usize>
    where
        F: FnMut(&'a T) -> Ordering,
    {
        let (front, back) = self.as_slices();
        let cmp_back = back.first().map(|elem| f(elem));

        if let Some(Ordering::Equal) = cmp_back {
            Ok(front.len())
        } else if let Some(Ordering::Less) = cmp_back {
            back.binary_search_by(f)
                .map(|idx| idx + front.len())
                .map_err(|idx| idx + front.len())
        } else {
            front.binary_search_by(f)
        }
    }

    /// Binary searches this `VecDeque` with a key extraction function.
    ///
    /// Assumes that the deque is sorted by the key, for instance with
    /// [`make_contiguous().sort_by_key()`] using the same key extraction function.
    /// If the deque is not sorted by the key, the returned result is
    /// unspecified and meaningless.
    ///
    /// If the value is found then [`Result::Ok`] is returned, containing the
    /// index of the matching element. If there are multiple matches, then any
    /// one of the matches could be returned. If the value is not found then
    /// [`Result::Err`] is returned, containing the index where a matching
    /// element could be inserted while maintaining sorted order.
    ///
    /// See also [`binary_search`], [`binary_search_by`], and [`partition_point`].
    ///
    /// [`make_contiguous().sort_by_key()`]: VecDeque::make_contiguous
    /// [`binary_search`]: VecDeque::binary_search
    /// [`binary_search_by`]: VecDeque::binary_search_by
    /// [`partition_point`]: VecDeque::partition_point
    ///
    /// # Examples
    ///
    /// Looks up a series of four elements in a slice of pairs sorted by
    /// their second elements. The first is found, with a uniquely
    /// determined position; the second and third are not found; the
    /// fourth could match any position in `[1, 4]`.
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<_> = [(0, 0), (2, 1), (4, 1), (5, 1),
    ///          (3, 1), (1, 2), (2, 3), (4, 5), (5, 8), (3, 13),
    ///          (1, 21), (2, 34), (4, 55)].try_into()?;
    ///
    /// assert_eq!(deque.binary_search_by_key(&13, |&(a, b)| b),  Ok(9));
    /// assert_eq!(deque.binary_search_by_key(&4, |&(a, b)| b),   Err(7));
    /// assert_eq!(deque.binary_search_by_key(&100, |&(a, b)| b), Err(13));
    /// let r = deque.binary_search_by_key(&1, |&(a, b)| b);
    /// assert!(matches!(r, Ok(1..=4)));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    #[inline]
    pub fn binary_search_by_key<'a, B, F>(&'a self, b: &B, mut f: F) -> Result<usize, usize>
    where
        F: FnMut(&'a T) -> B,
        B: Ord,
    {
        self.binary_search_by(|k| f(k).cmp(b))
    }

    /// Returns the index of the partition point according to the given predicate
    /// (the index of the first element of the second partition).
    ///
    /// The deque is assumed to be partitioned according to the given predicate.
    /// This means that all elements for which the predicate returns true are at the start of the deque
    /// and all elements for which the predicate returns false are at the end.
    /// For example, `[7, 15, 3, 5, 4, 12, 6]` is partitioned under the predicate `x % 2 != 0`
    /// (all odd numbers are at the start, all even at the end).
    ///
    /// If the deque is not partitioned, the returned result is unspecified and meaningless,
    /// as this method performs a kind of binary search.
    ///
    /// See also [`binary_search`], [`binary_search_by`], and [`binary_search_by_key`].
    ///
    /// [`binary_search`]: VecDeque::binary_search
    /// [`binary_search_by`]: VecDeque::binary_search_by
    /// [`binary_search_by_key`]: VecDeque::binary_search_by_key
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deque: VecDeque<_> = [1, 2, 3, 3, 5, 6, 7].try_into()?;
    /// let i = deque.partition_point(|&x| x < 5);
    ///
    /// assert_eq!(i, 4);
    /// assert!(deque.iter().take(i).all(|&x| x < 5));
    /// assert!(deque.iter().skip(i).all(|&x| !(x < 5)));
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    ///
    /// If you want to insert an item to a sorted deque, while maintaining
    /// sort order:
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut deque: VecDeque<_> = [0, 1, 1, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55].try_into()?;
    /// let num = 42;
    /// let idx = deque.partition_point(|&x| x < num);
    /// deque.try_insert(idx, num)?;
    /// assert_eq!(deque, &[0, 1, 1, 1, 1, 2, 3, 5, 8, 13, 21, 34, 42, 55]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn partition_point<P>(&self, mut pred: P) -> usize
    where
        P: FnMut(&T) -> bool,
    {
        let (front, back) = self.as_slices();

        if let Some(true) = back.first().map(|v| pred(v)) {
            back.partition_point(pred) + front.len()
        } else {
            front.partition_point(pred)
        }
    }
}

impl<T, A> VecDeque<T, A>
where
    T: TryClone,
    A: Allocator,
{
    /// Modifies the deque in-place so that `len()` is equal to new_len,
    /// either by removing excess elements from the back or by appending clones of `value`
    /// to the back.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let mut buf = VecDeque::new();
    /// buf.try_push_back(5)?;
    /// buf.try_push_back(10)?;
    /// buf.try_push_back(15)?;
    /// assert_eq!(buf, [5, 10, 15]);
    ///
    /// buf.try_resize(2, 0)?;
    /// assert_eq!(buf, [5, 10]);
    ///
    /// buf.try_resize(5, 20)?;
    /// assert_eq!(buf, [5, 10, 20, 20, 20]);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    pub fn try_resize(&mut self, new_len: usize, value: T) -> Result<(), Error> {
        if new_len > self.len() {
            let extra = new_len - self.len();

            for _ in 0..extra {
                self.try_push_back(value.try_clone()?)?;
            }
        } else {
            self.truncate(new_len);
        }

        Ok(())
    }
}

/// Returns the index in the underlying buffer for a given logical element index.
#[inline]
fn wrap_index(logical_index: usize, capacity: usize) -> usize {
    debug_assert!(
        (logical_index == 0 && capacity == 0)
            || logical_index < capacity
            || (logical_index - capacity) < capacity
    );
    if logical_index >= capacity {
        logical_index - capacity
    } else {
        logical_index
    }
}

impl<T, A> PartialEq for VecDeque<T, A>
where
    T: PartialEq,
    A: Allocator,
{
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len() {
            return false;
        }
        let (sa, sb) = self.as_slices();
        let (oa, ob) = other.as_slices();
        if sa.len() == oa.len() {
            sa == oa && sb == ob
        } else if sa.len() < oa.len() {
            // Always divisible in three sections, for example:
            // self:  [a b c|d e f]
            // other: [0 1 2 3|4 5]
            // front = 3, mid = 1,
            // [a b c] == [0 1 2] && [d] == [3] && [e f] == [4 5]
            let front = sa.len();
            let mid = oa.len() - front;

            let (oa_front, oa_mid) = oa.split_at(front);
            let (sb_mid, sb_back) = sb.split_at(mid);
            debug_assert_eq!(sa.len(), oa_front.len());
            debug_assert_eq!(sb_mid.len(), oa_mid.len());
            debug_assert_eq!(sb_back.len(), ob.len());
            sa == oa_front && sb_mid == oa_mid && sb_back == ob
        } else {
            let front = oa.len();
            let mid = sa.len() - front;

            let (sa_front, sa_mid) = sa.split_at(front);
            let (ob_mid, ob_back) = ob.split_at(mid);
            debug_assert_eq!(sa_front.len(), oa.len());
            debug_assert_eq!(sa_mid.len(), ob_mid.len());
            debug_assert_eq!(sb.len(), ob_back.len());
            sa_front == oa && sa_mid == ob_mid && sb == ob_back
        }
    }
}

impl<T, A> Eq for VecDeque<T, A>
where
    T: Eq,
    A: Allocator,
{
}

__impl_slice_eq1! { [] VecDeque<T, A>, Vec<U, A>, }
__impl_slice_eq1! { [] VecDeque<T, A>, &[U], }
__impl_slice_eq1! { [] VecDeque<T, A>, &mut [U], }
__impl_slice_eq1! { [const N: usize] VecDeque<T, A>, [U; N], }
__impl_slice_eq1! { [const N: usize] VecDeque<T, A>, &[U; N], }
__impl_slice_eq1! { [const N: usize] VecDeque<T, A>, &mut [U; N], }

impl<T, A> PartialOrd for VecDeque<T, A>
where
    T: PartialOrd,
    A: Allocator,
{
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

impl<T, A> Ord for VecDeque<T, A>
where
    T: Ord,
    A: Allocator,
{
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

impl<T, A> Hash for VecDeque<T, A>
where
    T: Hash,
    A: Allocator,
{
    #[inline]
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        state.write_usize(self.len);
        // It's not possible to use Hash::hash_slice on slices
        // returned by as_slices method as their length can vary
        // in otherwise identical deques.
        //
        // Hasher only guarantees equivalence for the exact same
        // set of calls to its methods.
        self.iter().for_each(|elem| elem.hash(state));
    }
}

impl<T, A> Index<usize> for VecDeque<T, A>
where
    A: Allocator,
{
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &T {
        self.get(index).expect("Out of bounds access")
    }
}

impl<T, A> IndexMut<usize> for VecDeque<T, A>
where
    A: Allocator,
{
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut T {
        self.get_mut(index).expect("Out of bounds access")
    }
}

impl<T, A> IntoIterator for VecDeque<T, A>
where
    A: Allocator,
{
    type Item = T;
    type IntoIter = IntoIter<T, A>;

    /// Consumes the deque into a front-to-back iterator yielding elements by
    /// value.
    fn into_iter(self) -> IntoIter<T, A> {
        IntoIter::new(self)
    }
}

impl<'a, T, A> IntoIterator for &'a VecDeque<T, A>
where
    A: Allocator,
{
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T, A> IntoIterator for &'a mut VecDeque<T, A>
where
    A: Allocator,
{
    type Item = &'a mut T;
    type IntoIter = IterMut<'a, T>;

    fn into_iter(self) -> IterMut<'a, T> {
        self.iter_mut()
    }
}

impl<T, A> fmt::Debug for VecDeque<T, A>
where
    T: fmt::Debug,
    A: Allocator,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T, A> From<Vec<T, A>> for VecDeque<T, A>
where
    A: Allocator,
{
    /// Turn a [`Vec<T>`] into a [`VecDeque<T>`].
    ///
    /// [`Vec<T>`]: crate::Vec
    /// [`VecDeque<T>`]: crate::VecDeque
    ///
    /// This conversion is guaranteed to run in *O*(1) time
    /// and to not re-allocate the `Vec`'s buffer or allocate
    /// any additional memory.
    #[inline]
    fn from(other: Vec<T, A>) -> Self {
        let (buf, len) = other.into_raw_vec();
        Self { head: 0, len, buf }
    }
}

impl<T, A> From<VecDeque<T, A>> for Vec<T, A>
where
    A: Allocator,
{
    /// Turn a [`VecDeque<T>`] into a [`Vec<T>`].
    ///
    /// [`Vec<T>`]: crate::Vec
    /// [`VecDeque<T>`]: crate::VecDeque
    ///
    /// This never needs to re-allocate, but does need to do *O*(*n*) data movement if
    /// the circular buffer doesn't happen to be at the beginning of the allocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use rune::alloc::{VecDeque, Vec};
    /// use rune::alloc::prelude::*;
    ///
    /// // This one is *O*(1).
    /// let deque: VecDeque<_> = (1..5).try_collect()?;
    /// let ptr = deque.as_slices().0.as_ptr();
    /// let vec = Vec::from(deque);
    /// assert_eq!(vec, [1, 2, 3, 4]);
    /// assert_eq!(vec.as_ptr(), ptr);
    ///
    /// // This one needs data rearranging.
    /// let mut deque: VecDeque<_> = (1..5).try_collect()?;
    /// deque.try_push_front(9)?;
    /// deque.try_push_front(8)?;
    /// let ptr = deque.as_slices().1.as_ptr();
    /// let vec = Vec::from(deque);
    /// assert_eq!(vec, [8, 9, 1, 2, 3, 4]);
    /// assert_eq!(vec.as_ptr(), ptr);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    fn from(mut other: VecDeque<T, A>) -> Self {
        other.make_contiguous();

        unsafe {
            let other = ManuallyDrop::new(other);
            let buf = other.buf.ptr();
            let len = other.len();
            let cap = other.capacity();
            let alloc = ptr::read(other.allocator());

            if other.head != 0 {
                ptr::copy(buf.add(other.head), buf, len);
            }
            Vec::from_raw_parts_in(buf, len, cap, alloc)
        }
    }
}

impl<T, const N: usize> TryFrom<[T; N]> for VecDeque<T> {
    type Error = Error;

    /// Converts a `[T; N]` into a `VecDeque<T>`.
    ///
    /// ```
    /// use rune::alloc::VecDeque;
    ///
    /// let deq1 = VecDeque::try_from([1, 2, 3, 4])?;
    /// let deq2: VecDeque<_> = [1, 2, 3, 4].try_into()?;
    /// assert_eq!(deq1, deq2);
    /// # Ok::<_, rune::alloc::Error>(())
    /// ```
    fn try_from(arr: [T; N]) -> Result<Self, Self::Error> {
        Ok(VecDeque::from(Vec::try_from(arr)?))
    }
}

impl<T, A> TryFromIteratorIn<T, A> for VecDeque<T, A>
where
    A: Allocator,
{
    fn try_from_iter_in<I>(iter: I, alloc: A) -> Result<Self, Error>
    where
        I: IntoIterator<Item = T>,
    {
        let mut this = VecDeque::new_in(alloc);
        this.try_extend(iter)?;
        Ok(this)
    }
}

impl<T, A> TryExtend<T> for VecDeque<T, A>
where
    A: Allocator,
{
    #[inline]
    fn try_extend<I: IntoIterator<Item = T>>(&mut self, iter: I) -> Result<(), Error> {
        for value in iter {
            self.try_push_back(value)?;
        }

        Ok(())
    }
}

use core::cmp::Ordering;
use core::iter;

use crate as rune;
use crate::alloc;
use crate::alloc::fmt::TryWrite;
use crate::alloc::prelude::*;
use crate::runtime::{
    EnvProtocolCaller, Formatter, Iterator, Protocol, ProtocolCaller, RawAnyGuard, Ref, Value, Vec,
    VmError, VmErrorKind,
};
use crate::{Any, ContextError, Module};

/// A dynamic vec deque.
#[rune::module(::std::collections::vec_deque)]
pub fn module() -> Result<Module, ContextError> {
    let mut m = Module::from_meta(self::module__meta)?;

    m.ty::<VecDeque>()?;

    m.function_meta(VecDeque::new)?;
    m.function_meta(VecDeque::with_capacity__meta)?;
    m.function_meta(from)?;

    m.function_meta(VecDeque::extend)?;
    m.function_meta(VecDeque::insert)?;
    m.function_meta(VecDeque::iter__meta)?;
    m.function_meta(VecDeque::into_iter__meta)?;
    m.function_meta(VecDeque::from_iter__meta)?;
    m.function_meta(VecDeque::reserve)?;
    m.function_meta(VecDeque::len)?;
    m.function_meta(VecDeque::capacity)?;
    m.function_meta(VecDeque::front)?;
    m.function_meta(VecDeque::back)?;
    m.function_meta(VecDeque::push_back__meta)?;
    m.function_meta(VecDeque::push_front)?;
    m.function_meta(VecDeque::pop_front)?;
    m.function_meta(VecDeque::pop_back)?;
    m.function_meta(VecDeque::remove)?;
    m.function_meta(VecDeque::rotate_left)?;
    m.function_meta(VecDeque::rotate_right)?;

    m.associated_function(&Protocol::INDEX_GET, VecDeque::get)?;
    m.associated_function(&Protocol::INDEX_SET, VecDeque::set)?;

    m.function_meta(VecDeque::debug_fmt)?;

    m.function_meta(VecDeque::partial_eq__meta)?;
    m.implement_trait::<VecDeque>(rune::item!(::std::cmp::PartialEq))?;

    m.function_meta(VecDeque::eq__meta)?;
    m.implement_trait::<VecDeque>(rune::item!(::std::cmp::Eq))?;

    m.function_meta(VecDeque::partial_cmp__meta)?;
    m.implement_trait::<VecDeque>(rune::item!(::std::cmp::PartialOrd))?;

    m.function_meta(VecDeque::cmp__meta)?;
    m.implement_trait::<VecDeque>(rune::item!(::std::cmp::Ord))?;

    m.ty::<Iter>()?;
    m.function_meta(Iter::next__meta)?;
    m.function_meta(Iter::size_hint__meta)?;
    m.implement_trait::<Iter>(rune::item!(::std::iter::Iterator))?;

    m.function_meta(Iter::next_back__meta)?;
    m.implement_trait::<Iter>(rune::item!(::std::iter::DoubleEndedIterator))?;

    m.function_meta(Iter::len__meta)?;
    m.implement_trait::<Iter>(rune::item!(::std::iter::ExactSizeIterator))?;

    Ok(m)
}

/// A double-ended queue implemented with a growable ring buffer.
///
/// The "default" usage of this type as a queue is to use [`push_back`] to add
/// to the queue, and [`pop_front`] to remove from the queue. [`extend`] and
/// [`append`] push onto the back in this manner, and iterating over `VecDeque`
/// goes front to back.
///
/// A `VecDeque` with a known list of items can be initialized from an array:
///
/// ```rune
/// use std::collections::VecDeque;
///
/// let deq = VecDeque::from::<Vec>([-1, 0, 1]);
/// ```
///
/// [`push_back`]: VecDeque::push_back
/// [`pop_front`]: VecDeque::pop_front
/// [`extend`]: VecDeque::extend
/// [`append`]: VecDeque::append
#[derive(Any, Default)]
#[rune(module = crate, item = ::std::collections::vec_deque)]
pub(crate) struct VecDeque {
    inner: alloc::VecDeque<Value>,
}

impl VecDeque {
    /// Creates an empty deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::new();
    /// ```
    #[rune::function(path = Self::new)]
    fn new() -> VecDeque {
        Self {
            inner: alloc::VecDeque::new(),
        }
    }

    /// Creates an empty deque with space for at least `capacity` elements.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::with_capacity(10);
    /// assert!(deque.capacity() >= 10);
    /// ```
    #[rune::function(keep, path = Self::with_capacity)]
    pub(crate) fn with_capacity(count: usize) -> alloc::Result<VecDeque> {
        Ok(Self {
            inner: alloc::VecDeque::try_with_capacity(count)?,
        })
    }

    /// Extend this VecDeque with something that implements the [`INTO_ITER`]
    /// protocol.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::new();
    /// deque.extend([1, 2, 3]);
    ///
    /// assert_eq!(Some(1), deque.pop_front());
    /// assert_eq!(Some(3), deque.pop_back());
    /// ```
    #[rune::function]
    pub fn extend(&mut self, value: Value) -> Result<(), VmError> {
        let mut it = value.into_iter()?;

        while let Some(value) = it.next()? {
            self.inner.try_push_back(value)?;
        }

        Ok(())
    }

    /// Provides a reference to the front element, or `None` if the deque is
    /// empty.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let d = VecDeque::new();
    /// assert_eq!(d.front(), None);
    ///
    /// d.push_back(1);
    /// d.push_back(2);
    /// assert_eq!(d.front(), Some(1));
    /// ```
    #[rune::function]
    fn front(&mut self) -> Option<Value> {
        self.inner.front().cloned()
    }

    /// Provides a reference to the back element, or `None` if the deque is
    /// empty.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let d = VecDeque::new();
    /// assert_eq!(d.back(), None);
    ///
    /// d.push_back(1);
    /// d.push_back(2);
    /// assert_eq!(d.back(), Some(2));
    /// ```
    #[rune::function]
    pub fn back(&self) -> Option<Value> {
        self.inner.back().cloned()
    }

    /// Appends an element to the back of the deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::new();
    /// buf.push_back(1);
    /// buf.push_back(3);
    /// assert_eq!(Some(3), buf.back());
    /// ```
    #[rune::function(keep)]
    pub(crate) fn push_back(&mut self, value: Value) -> alloc::Result<()> {
        self.inner.try_push_back(value)
    }

    /// Prepends an element to the deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let d = VecDeque::new();
    /// d.push_front(1);
    /// d.push_front(2);
    /// assert_eq!(d.front(), Some(2));
    /// ```
    #[rune::function]
    fn push_front(&mut self, value: Value) -> alloc::Result<()> {
        self.inner.try_push_front(value)
    }

    /// Removes the first element and returns it, or `None` if the deque is
    /// empty.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let d = VecDeque::new();
    /// d.push_back(1);
    /// d.push_back(2);
    ///
    /// assert_eq!(d.pop_front(), Some(1));
    /// assert_eq!(d.pop_front(), Some(2));
    /// assert_eq!(d.pop_front(), None);
    /// ```
    #[rune::function]
    fn pop_front(&mut self) -> Option<Value> {
        self.inner.pop_front()
    }

    /// Removes the last element from the deque and returns it, or `None` if it
    /// is empty.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::new();
    /// assert_eq!(buf.pop_back(), None);
    /// buf.push_back(1);
    /// buf.push_back(3);
    /// assert_eq!(buf.pop_back(), Some(3));
    /// ```
    #[rune::function]
    fn pop_back(&mut self) -> Option<Value> {
        self.inner.pop_back()
    }

    /// Reserves capacity for at least `additional` more elements to be inserted
    /// in the given deque. The collection may reserve more space to
    /// speculatively avoid frequent reallocations.
    ///
    /// # Panics
    ///
    /// Panics if the new capacity overflows `usize`.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::from::<Vec>([1]);
    /// buf.reserve(10);
    /// assert!(buf.capacity() >= 11);
    /// ```
    #[rune::function]
    fn reserve(&mut self, index: usize) -> alloc::Result<()> {
        self.inner.try_reserve(index)
    }

    /// Returns the number of elements in the deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::new();
    /// assert_eq!(deque.len(), 0);
    /// deque.push_back(1);
    /// assert_eq!(deque.len(), 1);
    /// ```
    #[rune::function]
    fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns the number of elements the deque can hold without reallocating.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::with_capacity(10);
    /// assert!(buf.capacity() >= 10);
    /// ```
    #[rune::function]
    fn capacity(&mut self) -> usize {
        self.inner.capacity()
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
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::new();
    /// buf.push_back('a');
    /// buf.push_back('b');
    /// buf.push_back('c');
    /// assert_eq!(buf, ['a', 'b', 'c']);
    ///
    /// buf.insert(1, 'd');
    /// assert_eq!(buf, ['a', 'd', 'b', 'c']);
    /// ```
    #[rune::function]
    fn insert(&mut self, index: usize, value: Value) -> Result<(), VmError> {
        if index > self.inner.len() {
            return Err(VmError::new(VmErrorKind::OutOfRange {
                index: index.into(),
                length: self.inner.len().into(),
            }));
        }

        self.inner.try_insert(index, value)?;
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
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::new();
    /// buf.push_back(1);
    /// buf.push_back(2);
    /// buf.push_back(3);
    /// assert_eq!(buf, [1, 2, 3]);
    ///
    /// assert_eq!(buf.remove(1), Some(2));
    /// assert_eq!(buf, [1, 3]);
    /// ```
    #[rune::function]
    fn remove(&mut self, index: usize) -> Option<Value> {
        self.inner.remove(index)
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
    /// If `mid` is greater than `len()`. Note that `mid == len()` does _not_
    /// panic and is a no-op rotation.
    ///
    /// # Complexity
    ///
    /// Takes `*O*(min(mid, len() - mid))` time and no extra space.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = (0..10).iter().collect::<VecDeque>();
    ///
    /// buf.rotate_left(3);
    /// assert_eq!(buf, [3, 4, 5, 6, 7, 8, 9, 0, 1, 2]);
    ///
    /// for i in 1..10 {
    ///     assert_eq!(i * 3 % 10, buf[0]);
    ///     buf.rotate_left(3);
    /// }
    ///
    /// assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    /// ```
    #[rune::function]
    fn rotate_left(&mut self, mid: usize) -> Result<(), VmError> {
        if mid > self.inner.len() {
            return Err(VmError::new(VmErrorKind::OutOfRange {
                index: mid.into(),
                length: self.inner.len().into(),
            }));
        }

        self.inner.rotate_left(mid);
        Ok(())
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
    /// If `k` is greater than `len()`. Note that `k == len()` does _not_ panic
    /// and is a no-op rotation.
    ///
    /// # Complexity
    ///
    /// Takes `*O*(min(k, len() - k))` time and no extra space.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = (0..10).iter().collect::<VecDeque>();
    ///
    /// buf.rotate_right(3);
    /// assert_eq!(buf, [7, 8, 9, 0, 1, 2, 3, 4, 5, 6]);
    ///
    /// for i in 1..10 {
    ///     assert_eq!(0, buf[i * 3 % 10]);
    ///     buf.rotate_right(3);
    /// }
    ///
    /// assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    /// ```
    #[rune::function]
    fn rotate_right(&mut self, mid: usize) -> Result<(), VmError> {
        if mid > self.inner.len() {
            return Err(VmError::new(VmErrorKind::OutOfRange {
                index: mid.into(),
                length: self.inner.len().into(),
            }));
        }

        self.inner.rotate_right(mid);
        Ok(())
    }

    /// Returns a front-to-back iterator.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let buf = VecDeque::new();
    /// buf.push_back(5);
    /// buf.push_back(3);
    /// buf.push_back(4);
    ///
    /// assert_eq!([5, 3, 4], buf.iter());
    /// assert_eq!([4, 3, 5], buf.iter().rev());
    /// ```
    #[inline]
    #[rune::function(keep, instance, path = Self::iter)]
    fn iter(this: Ref<Self>) -> Iter {
        // SAFETY: We're holding onto the reference guard.
        let iter = unsafe { this.inner.raw_iter() };
        let (_, guard) = Ref::into_raw(this);
        Iter { iter, guard }
    }

    #[rune::function(keep, instance, protocol = INTO_ITER, path = Self)]
    fn into_iter(this: Ref<Self>) -> Iter {
        Self::iter(this)
    }

    /// Build a [`VecDeque`] from an iterator.
    ///
    /// The vecdeque can be converted from anything that implements the
    /// [`INTO_ITER`] protocol.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::from_iter(["a", "b", "c"]);
    /// assert_eq!(deque.len(), 3);
    /// assert_eq!(deque.pop_front(), Some("a"));
    /// assert_eq!(deque.pop_back(), Some("c"));
    /// assert_eq!(deque.len(), 1);
    /// ```
    #[rune::function(keep, path = Self::from_iter)]
    fn from_iter(mut it: Iterator) -> Result<Self, VmError> {
        let (lo, _) = it.size_hint()?;
        let mut inner = alloc::VecDeque::try_with_capacity(lo)?;

        while let Some(value) = it.next()? {
            inner.try_push_back(value)?;
        }

        Ok(Self { inner })
    }

    fn get(&self, index: usize) -> Result<Value, VmError> {
        let Some(v) = self.inner.get(index) else {
            return Err(VmError::new(VmErrorKind::OutOfRange {
                index: index.into(),
                length: self.inner.len().into(),
            }));
        };

        Ok(v.clone())
    }

    fn set(&mut self, index: usize, value: Value) -> Result<(), VmError> {
        let Some(v) = self.inner.get_mut(index) else {
            return Err(VmError::new(VmErrorKind::OutOfRange {
                index: index.into(),
                length: self.inner.len().into(),
            }));
        };

        *v = value;
        Ok(())
    }

    /// Write a debug representation to a string.
    ///
    /// This calls the [`DEBUG_FMT`] protocol over all elements of the
    /// collection.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::from::<Vec>([1, 2, 3]);
    /// assert_eq!(format!("{:?}", deque), "[1, 2, 3]");
    /// ```
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug_fmt(&self, f: &mut Formatter) -> Result<(), VmError> {
        self.debug_fmt_with(f, &mut EnvProtocolCaller)
    }

    #[inline]
    fn debug_fmt_with(
        &self,
        f: &mut Formatter,
        caller: &mut dyn ProtocolCaller,
    ) -> Result<(), VmError> {
        let mut it = self.inner.iter().peekable();

        write!(f, "[")?;

        while let Some(value) = it.next() {
            value.debug_fmt_with(f, caller)?;

            if it.peek().is_some() {
                write!(f, ", ")?;
            }
        }

        write!(f, "]")?;
        Ok(())
    }

    /// Perform a partial equality check with this deque.
    ///
    /// This can take any argument which can be converted into an iterator using
    /// [`INTO_ITER`].
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::from::<Vec>([1, 2, 3]);
    ///
    /// assert!(deque == [1, 2, 3]);
    /// assert!(deque == (1..=3));
    /// assert!(deque != [2, 3, 4]);
    /// ```
    #[rune::function(keep, protocol = PARTIAL_EQ)]
    fn partial_eq(&self, b: Value) -> Result<bool, VmError> {
        self.partial_eq_with(b, &mut EnvProtocolCaller)
    }

    fn partial_eq_with(&self, b: Value, caller: &mut dyn ProtocolCaller) -> Result<bool, VmError> {
        let mut b = b.into_iter_with(caller)?;

        for a in &self.inner {
            let Some(b) = b.next()? else {
                return Ok(false);
            };

            if !Value::partial_eq_with(a, &b, caller)? {
                return Ok(false);
            }
        }

        if b.next()?.is_some() {
            return Ok(false);
        }

        Ok(true)
    }

    /// Perform a total equality check with this deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    /// use std::ops::eq;
    ///
    /// let deque = VecDeque::from::<Vec>([1, 2, 3]);
    ///
    /// assert!(eq(deque, VecDeque::from::<Vec>([1, 2, 3])));
    /// assert!(!eq(deque, VecDeque::from::<Vec>([2, 3, 4])));
    /// ```
    #[rune::function(keep, protocol = EQ)]
    fn eq(&self, b: &VecDeque) -> Result<bool, VmError> {
        self.eq_with(b, &mut EnvProtocolCaller)
    }

    fn eq_with(&self, b: &VecDeque, caller: &mut dyn ProtocolCaller) -> Result<bool, VmError> {
        let mut b = b.inner.iter();

        for a in &self.inner {
            let Some(b) = b.next() else {
                return Ok(false);
            };

            if !Value::eq_with(a, b, caller)? {
                return Ok(false);
            }
        }

        if b.next().is_some() {
            return Ok(false);
        }

        Ok(true)
    }

    /// Perform a partial comparison check with this deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    ///
    /// let deque = VecDeque::from::<Vec>([1, 2, 3]);
    ///
    /// assert!(deque > VecDeque::from::<Vec>([0, 2, 3]));
    /// assert!(deque < VecDeque::from::<Vec>([2, 2, 3]));
    /// ```
    #[rune::function(keep, protocol = PARTIAL_CMP)]
    fn partial_cmp(&self, b: &VecDeque) -> Result<Option<Ordering>, VmError> {
        self.partial_cmp_with(b, &mut EnvProtocolCaller)
    }

    fn partial_cmp_with(
        &self,
        b: &VecDeque,
        caller: &mut dyn ProtocolCaller,
    ) -> Result<Option<Ordering>, VmError> {
        let mut b = b.inner.iter();

        for a in self.inner.iter() {
            let Some(b) = b.next() else {
                return Ok(Some(Ordering::Greater));
            };

            match Value::partial_cmp_with(a, b, caller)? {
                Some(Ordering::Equal) => (),
                other => return Ok(other),
            }
        }

        if b.next().is_some() {
            return Ok(Some(Ordering::Less));
        };

        Ok(Some(Ordering::Equal))
    }

    /// Perform a total comparison check with this deque.
    ///
    /// # Examples
    ///
    /// ```rune
    /// use std::collections::VecDeque;
    /// use std::cmp::Ordering;
    /// use std::ops::cmp;
    ///
    /// let deque = VecDeque::from::<Vec>([1, 2, 3]);
    ///
    /// assert_eq!(cmp(deque, VecDeque::from::<Vec>([0, 2, 3])), Ordering::Greater);
    /// assert_eq!(cmp(deque, VecDeque::from::<Vec>([2, 2, 3])), Ordering::Less);
    /// ```
    #[rune::function(keep, protocol = CMP)]
    fn cmp(&self, b: &VecDeque) -> Result<Ordering, VmError> {
        self.cmp_with(b, &mut EnvProtocolCaller)
    }

    fn cmp_with(
        &self,
        other: &VecDeque,
        caller: &mut dyn ProtocolCaller,
    ) -> Result<Ordering, VmError> {
        let mut b = other.inner.iter();

        for a in self.inner.iter() {
            let Some(b) = b.next() else {
                return Ok(Ordering::Greater);
            };

            match Value::cmp_with(a, b, caller)? {
                Ordering::Equal => (),
                other => return Ok(other),
            }
        }

        if b.next().is_some() {
            return Ok(Ordering::Less);
        };

        Ok(Ordering::Equal)
    }
}

impl From<Vec> for VecDeque {
    fn from(value: Vec) -> Self {
        Self {
            inner: alloc::VecDeque::from(value.into_inner()),
        }
    }
}

impl TryClone for VecDeque {
    #[inline]
    fn try_clone(&self) -> alloc::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
        })
    }

    #[inline]
    fn try_clone_from(&mut self, source: &Self) -> alloc::Result<()> {
        self.inner.try_clone_from(&source.inner)
    }
}

/// Construct a [`VecDeque`] from a [`Vec`].
///
/// This is a cheap conversion.
///
/// # Examples
///
/// ```rune
/// use std::collections::VecDeque;
///
/// let buf = VecDeque::from::<Vec>([1, 2, 3]);
/// ```
#[rune::function(free, path = VecDeque::from::<Vec>)]
fn from(vec: Vec) -> VecDeque {
    VecDeque::from(vec)
}

#[derive(Any)]
#[rune(item = ::std::collections::vec_deque)]
pub(crate) struct Iter {
    iter: alloc::vec_deque::RawIter<Value>,
    // Drop must happen after the raw iterator.
    #[allow(unused)]
    guard: RawAnyGuard,
}

impl Iter {
    #[rune::function(keep, protocol = NEXT)]
    fn next(&mut self) -> Option<Value> {
        // SAFETY: We're holding onto the reference guard.
        unsafe { Some((*self.iter.next()?).clone()) }
    }

    #[rune::function(keep, protocol = SIZE_HINT)]
    fn size_hint(self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }

    #[rune::function(keep, protocol = LEN)]
    fn len(self) -> usize {
        self.iter.len()
    }

    #[rune::function(keep, protocol = NEXT_BACK)]
    fn next_back(&mut self) -> Option<Value> {
        // SAFETY: We're holding onto the reference guard.
        unsafe { Some((*self.iter.next_back()?).clone()) }
    }
}

impl iter::Iterator for Iter {
    type Item = Value;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        Iter::next(self)
    }
}

impl iter::DoubleEndedIterator for Iter {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        Iter::next_back(self)
    }
}

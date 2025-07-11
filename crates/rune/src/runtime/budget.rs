//! Budgeting module for Runestick.
//!
//! This module contains methods which allows for limiting the execution of the
//! virtual machine to abide by the specified budget.
//!
//! By default the budget is disabled, but can be enabled by wrapping your
//! function call in [with].

#[cfg_attr(feature = "std", path = "budget/std.rs")]
mod no_std;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use pin_project::pin_project;
use rune_alloc::callable::Callable;

/// Wrapper for something being [budgeted].
///
/// See [with].
///
/// [budgeted]: self
#[pin_project]
pub struct Budget<T> {
    /// Instruction budget.
    budget: usize,
    /// The thing being budgeted.
    #[pin]
    value: T,
}

/// Wrap the given value with a budget.
///
/// Budgeting is only performed on a per-instruction basis in the virtual
/// machine. What exactly constitutes an instruction might be a bit vague. But
/// important to note is that without explicit co-operation from native
/// functions the budget cannot be enforced. So care must be taken with the
/// native functions that you provide to Rune to ensure that the limits you
/// impose cannot be circumvented.
///
/// The following things can be wrapped:
/// * A [`FnOnce`] closure, like `with(|| println!("Hello World")).call()`.
/// * A [`Future`], like `with(async { /* async work */ }).await`;
///
/// It's also possible to wrap other wrappers which implement [`Callable`].
///
/// # Examples
///
/// ```no_run
/// use rune::runtime::budget;
/// use rune::Vm;
///
/// let mut vm: Vm = todo!();
/// // The virtual machine and any tasks associated with it is only allowed to execute 100 budget.
/// budget::with(100, || vm.call(&["main"], ())).call()?;
/// # Ok::<(), rune::support::Error>(())
/// ```
///
/// This budget can be conveniently combined with the memory [`limit`] module
/// due to both wrappers implementing [`Callable`].
///
/// [`limit`]: crate::alloc::limit
///
/// ```
/// use rune::runtime::budget;
/// use rune::alloc::{limit, Vec};
///
/// #[derive(Debug, PartialEq)]
/// struct Marker;
///
/// // Limit the given closure to run one instruction and allocate 1024 bytes.
/// let f = budget::with(1, limit::with(1024, || {
///     let mut budget = budget::acquire();
///     assert!(budget.take());
///     assert!(!budget.take());
///     assert!(Vec::<u8>::try_with_capacity(1).is_ok());
///     assert!(Vec::<u8>::try_with_capacity(1024).is_ok());
///     assert!(Vec::<u8>::try_with_capacity(1025).is_err());
///     Marker
/// }));
///
/// assert_eq!(f.call(), Marker);
/// ```
pub fn with<T>(budget: usize, value: T) -> Budget<T> {
    tracing::trace!(?budget);
    Budget { budget, value }
}

/// Replace the current budget returning a guard that will release it.
///
/// Use [`BudgetGuard::take`] to take permites from the returned budget.
#[inline(never)]
pub fn replace(budget: usize) -> BudgetGuard {
    BudgetGuard(self::no_std::rune_budget_replace(budget))
}

/// Acquire the current budget.
///
/// Use [`BudgetGuard::take`] to take permites from the returned budget.
#[inline(never)]
pub fn acquire() -> BudgetGuard {
    BudgetGuard(self::no_std::rune_budget_replace(usize::MAX))
}

/// A locally acquired budget.
///
/// This guard is acquired by calling [`take`] and can be used to take permits.
///
/// [`take`]: BudgetGuard::take
#[repr(transparent)]
pub struct BudgetGuard(usize);

impl BudgetGuard {
    /// Take a ticker from the budget.
    #[inline]
    pub fn take(&mut self) -> bool {
        if self.0 == usize::MAX {
            return true;
        }

        if self.0 == 0 {
            return false;
        }

        self.0 -= 1;
        true
    }
}

impl Drop for BudgetGuard {
    #[inline]
    fn drop(&mut self) {
        let _ = self::no_std::rune_budget_replace(self.0);
    }
}

impl<T> Budget<T>
where
    T: Callable,
{
    /// Call the budgeted function.
    #[inline]
    pub fn call(self) -> T::Output {
        Callable::call(self)
    }
}

impl<T> Callable for Budget<T>
where
    T: Callable,
{
    type Output = T::Output;

    #[inline]
    fn call(self) -> Self::Output {
        let _guard = BudgetGuard(self::no_std::rune_budget_replace(self.budget));
        self.value.call()
    }
}

impl<T> Future for Budget<T>
where
    T: Future,
{
    type Output = T::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let _guard = BudgetGuard(self::no_std::rune_budget_replace(*this.budget));
        let poll = this.value.poll(cx);
        *this.budget = self::no_std::rune_budget_get();
        poll
    }
}

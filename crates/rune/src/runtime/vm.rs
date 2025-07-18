use core::cmp::Ordering;
use core::fmt;
use core::mem::replace;
use core::ptr::NonNull;

use crate::alloc::prelude::*;
use crate::alloc::{self, String};
use crate::hash::{Hash, IntoHash, ToTypeHash};
use crate::modules::{cmp, option, result};
use crate::runtime;
use crate::sync::Arc;

mod ops;
use self::ops::*;

use super::{
    budget, inst, Address, AnySequence, Args, Awaited, BorrowMut, Bytes, Call, ControlFlow,
    DynArgs, DynGuardedArgs, Format, FormatSpec, Formatter, FromValue, Function, Future, Generator,
    GeneratorState, GuardedArgs, Inline, InstArithmeticOp, InstBitwiseOp, InstOp, InstRange,
    InstShiftOp, InstTarget, InstValue, Object, Output, OwnedTuple, Pair, Panic, Protocol,
    ProtocolCaller, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo, RangeToInclusive, Repr,
    RttiKind, RuntimeContext, Select, SelectFuture, Stack, Stream, Type, TypeHash, TypeInfo,
    TypeOf, Unit, UnitFn, UnitStorage, Value, Vec, VmDiagnostics, VmDiagnosticsObj, VmError,
    VmErrorKind, VmExecution, VmHalt, VmIntegerRepr, VmOutcome, VmSendExecution,
};

/// Helper to take a value, replacing the old one with empty.
#[inline(always)]
fn take(value: &mut Value) -> Value {
    replace(value, Value::empty())
}

#[inline(always)]
fn consume(value: &mut Value) {
    *value = Value::empty();
}

/// Indicating the kind of isolation that is present for a frame.
#[derive(Debug, Clone, Copy)]
pub enum Isolated {
    /// The frame is isolated, once pop it will cause the execution to complete.
    Isolated,
    /// No isolation is present, the vm will continue executing.
    None,
}

impl Isolated {
    #[inline]
    pub(crate) fn new(value: bool) -> Self {
        if value {
            Self::Isolated
        } else {
            Self::None
        }
    }

    #[inline]
    pub(crate) fn then_some<T>(self, value: T) -> Option<T> {
        match self {
            Self::Isolated => Some(value),
            Self::None => None,
        }
    }
}

impl fmt::Display for Isolated {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Isolated => write!(f, "isolated"),
            Self::None => write!(f, "none"),
        }
    }
}

/// The result from a dynamic call. Indicates if the attempted operation is
/// supported.
#[derive(Debug)]
pub(crate) enum CallResultOnly<T> {
    /// Call successful. Return value is on the stack.
    Ok(T),
    /// Call failed because function was missing so the method is unsupported.
    /// Contains target value.
    Unsupported(Value),
}

/// The result from a dynamic call. Indicates if the attempted operation is
/// supported.
#[derive(Debug)]
pub(crate) enum CallResult<T> {
    /// Call successful. Return value is on the stack.
    Ok(T),
    /// A call frame was pushed onto the virtual machine, which needs to be
    /// advanced to produce the result.
    Frame,
    /// Call failed because function was missing so the method is unsupported.
    /// Contains target value.
    Unsupported(Value),
}

/// A stack which references variables indirectly from a slab.
#[derive(Debug)]
pub struct Vm {
    /// Context associated with virtual machine.
    context: Arc<RuntimeContext>,
    /// Unit associated with virtual machine.
    unit: Arc<Unit>,
    /// The current instruction pointer.
    ip: usize,
    /// The length of the last instruction pointer.
    last_ip_len: u8,
    /// The current stack.
    stack: Stack,
    /// Frames relative to the stack.
    call_frames: alloc::Vec<CallFrame>,
}

impl Vm {
    /// Construct a new virtual machine.
    ///
    /// Constructing a virtual machine is a cheap constant-time operation.
    ///
    /// See [`unit_mut`] and [`context_mut`] documentation for information on
    /// how to re-use existing [`Vm`]'s.
    ///
    /// [`unit_mut`]: Vm::unit_mut
    /// [`context_mut`]: Vm::context_mut
    pub const fn new(context: Arc<RuntimeContext>, unit: Arc<Unit>) -> Self {
        Self::with_stack(context, unit, Stack::new())
    }

    /// Construct a new virtual machine with a custom stack.
    pub const fn with_stack(context: Arc<RuntimeContext>, unit: Arc<Unit>, stack: Stack) -> Self {
        Self {
            context,
            unit,
            ip: 0,
            last_ip_len: 0,
            stack,
            call_frames: alloc::Vec::new(),
        }
    }

    /// Construct a vm with a default empty [RuntimeContext]. This is useful
    /// when the [Unit] was constructed with an empty
    /// [Context][crate::compile::Context].
    pub fn without_runtime(unit: Arc<Unit>) -> alloc::Result<Self> {
        Ok(Self::new(Arc::try_new(RuntimeContext::default())?, unit))
    }

    /// Test if the virtual machine is the same context and unit as specified.
    pub fn is_same(&self, context: &Arc<RuntimeContext>, unit: &Arc<Unit>) -> bool {
        Arc::ptr_eq(&self.context, context) && Arc::ptr_eq(&self.unit, unit)
    }

    /// Test if the virtual machine is the same context.
    pub fn is_same_context(&self, context: &Arc<RuntimeContext>) -> bool {
        Arc::ptr_eq(&self.context, context)
    }

    /// Test if the virtual machine is the same context.
    pub fn is_same_unit(&self, unit: &Arc<Unit>) -> bool {
        Arc::ptr_eq(&self.unit, unit)
    }

    /// Set  the current instruction pointer.
    #[inline]
    pub fn set_ip(&mut self, ip: usize) {
        self.ip = ip;
    }

    /// Get the stack.
    #[inline]
    pub fn call_frames(&self) -> &[CallFrame] {
        &self.call_frames
    }

    /// Get the stack.
    #[inline]
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// Get the stack mutably.
    #[inline]
    pub fn stack_mut(&mut self) -> &mut Stack {
        &mut self.stack
    }

    /// Access the context related to the virtual machine mutably.
    ///
    /// Note that this can be used to swap out the [`RuntimeContext`] associated
    /// with the running vm. Note that this is only necessary if the underlying
    /// [`Context`] is different or has been modified. In contrast to
    /// constructing a [`new`] vm, this allows for amortised re-use of any
    /// allocations.
    ///
    /// After doing this, it's important to call [`clear`] to clean up any
    /// residual state.
    ///
    /// [`clear`]: Vm::clear
    /// [`Context`]: crate::Context
    /// [`new`]: Vm::new
    #[inline]
    pub fn context_mut(&mut self) -> &mut Arc<RuntimeContext> {
        &mut self.context
    }

    /// Access the context related to the virtual machine.
    #[inline]
    pub fn context(&self) -> &Arc<RuntimeContext> {
        &self.context
    }

    /// Access the underlying unit of the virtual machine mutably.
    ///
    /// Note that this can be used to swap out the [`Unit`] of execution in the
    /// running vm. In contrast to constructing a [`new`] vm, this allows for
    /// amortised re-use of any allocations.
    ///
    /// After doing this, it's important to call [`clear`] to clean up any
    /// residual state.
    ///
    /// [`clear`]: Vm::clear
    /// [`new`]: Vm::new
    #[inline]
    pub fn unit_mut(&mut self) -> &mut Arc<Unit> {
        &mut self.unit
    }

    /// Access the underlying unit of the virtual machine.
    #[inline]
    pub fn unit(&self) -> &Arc<Unit> {
        &self.unit
    }

    /// Access the current instruction pointer.
    #[inline]
    pub fn ip(&self) -> usize {
        self.ip
    }

    /// Access the last instruction that was executed.
    #[inline]
    pub fn last_ip(&self) -> usize {
        self.ip.wrapping_sub(self.last_ip_len as usize)
    }

    /// Reset this virtual machine, freeing all memory used.
    pub fn clear(&mut self) {
        self.ip = 0;
        self.stack.clear();
        self.call_frames.clear();
    }

    /// Look up a function in the virtual machine by its name.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rune::sync::Arc;
    /// use rune::{Context, Unit, Vm};
    ///
    /// let mut sources = rune::sources! {
    ///     entry => {
    ///         pub fn max(a, b) {
    ///             if a > b {
    ///                 a
    ///             } else {
    ///                 b
    ///             }
    ///         }
    ///     }
    /// };
    ///
    /// let context = Context::with_default_modules()?;
    /// let runtime = Arc::try_new(context.runtime()?)?;
    ///
    /// let unit = rune::prepare(&mut sources).build()?;
    /// let unit = Arc::try_new(unit)?;
    ///
    /// let vm = Vm::new(runtime, unit);
    ///
    /// // Looking up an item from the source.
    /// let dynamic_max = vm.lookup_function(["max"])?;
    ///
    /// let value = dynamic_max.call::<i64>((10, 20))?;
    /// assert_eq!(value, 20);
    ///
    /// // Building an item buffer to lookup an `::std` item.
    /// let item = rune::item!(::std::i64::max);
    /// let max = vm.lookup_function(item)?;
    ///
    /// let value = max.call::<i64>((10, 20))?;
    /// assert_eq!(value, 20);
    /// # Ok::<_, rune::support::Error>(())
    /// ```
    pub fn lookup_function<N>(&self, name: N) -> Result<Function, VmError>
    where
        N: ToTypeHash,
    {
        Ok(self.lookup_function_by_hash(name.to_type_hash())?)
    }

    /// Convert into an execution.
    pub(crate) fn into_execution(self) -> VmExecution<Self> {
        VmExecution::new(self)
    }

    /// Run the given vm to completion.
    ///
    /// # Errors
    ///
    /// If any non-completing outcomes like yielding or awaiting are
    /// encountered, this will error.
    pub fn complete(self) -> Result<Value, VmError> {
        self.into_execution().complete()
    }

    /// Run the given vm to completion with support for async functions.
    pub async fn async_complete(self) -> Result<Value, VmError> {
        self.into_execution().resume().await?.into_complete()
    }

    /// Call the function identified by the given name.
    ///
    /// Computing the function hash from the name can be a bit costly, so it's
    /// worth noting that it can be precalculated:
    ///
    /// ```
    /// use rune::Hash;
    ///
    /// let name = Hash::type_hash(["main"]);
    /// ```
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rune::sync::Arc;
    /// use rune::{Context, Unit, Vm};
    ///
    /// let unit = Arc::try_new(Unit::default())?;
    /// let mut vm = Vm::without_runtime(unit)?;
    ///
    /// let output = vm.execute(["main"], (33i64,))?.complete()?;
    /// let output: i64 = rune::from_value(output)?;
    ///
    /// println!("output: {}", output);
    /// # Ok::<_, rune::support::Error>(())
    /// ```
    ///
    /// You can use a `Vec<Value>` to provide a variadic collection of
    /// arguments.
    ///
    /// ```no_run
    /// use rune::sync::Arc;
    /// use rune::{Context, Unit, Vm};
    ///
    /// // Normally the unit would be created by compiling some source,
    /// // and since this one is empty it won't do anything.
    /// let unit = Arc::try_new(Unit::default())?;
    /// let mut vm = Vm::without_runtime(unit)?;
    ///
    /// let mut args = Vec::new();
    /// args.push(rune::to_value(1u32)?);
    /// args.push(rune::to_value(String::from("Hello World"))?);
    ///
    /// let output = vm.execute(["main"], args)?.complete()?;
    /// let output: i64 = rune::from_value(output)?;
    ///
    /// println!("output: {}", output);
    /// # Ok::<_, rune::support::Error>(())
    /// ```
    pub fn execute(
        &mut self,
        name: impl ToTypeHash,
        args: impl Args,
    ) -> Result<VmExecution<&mut Self>, VmError> {
        self.set_entrypoint(name, args.count())?;
        args.into_stack(&mut self.stack)?;
        Ok(VmExecution::new(self))
    }

    /// An `execute` variant that returns an execution which implements
    /// [`Send`], allowing it to be sent and executed on a different thread.
    ///
    /// This is accomplished by preventing values escaping from being
    /// non-exclusively sent with the execution or escaping the execution. We
    /// only support encoding arguments which themselves are `Send`.
    pub fn send_execute(
        mut self,
        name: impl ToTypeHash,
        args: impl Args + Send,
    ) -> Result<VmSendExecution, VmError> {
        // Safety: make sure the stack is clear, preventing any values from
        // being sent along with the virtual machine.
        self.stack.clear();

        self.set_entrypoint(name, args.count())?;
        args.into_stack(&mut self.stack)?;
        Ok(VmSendExecution(VmExecution::new(self)))
    }

    /// Call the given function immediately, returning the produced value.
    ///
    /// This function permits for using references since it doesn't defer its
    /// execution.
    pub fn call(
        &mut self,
        name: impl ToTypeHash,
        args: impl GuardedArgs,
    ) -> Result<Value, VmError> {
        self.set_entrypoint(name, args.count())?;

        // Safety: We hold onto the guard until the vm has completed and
        // `VmExecution` will clear the stack before this function returns.
        // Erronously or not.
        let guard = unsafe { args.guarded_into_stack(&mut self.stack)? };

        let value = {
            // Clearing the stack here on panics has safety implications - see
            // above.
            let vm = ClearStack(self);
            VmExecution::new(&mut *vm.0).complete()?
        };

        // Note: this might panic if something in the vm is holding on to a
        // reference of the value. We should prevent it from being possible to
        // take any owned references to values held by this.
        drop(guard);
        Ok(value)
    }

    /// Call the given function immediately, returning the produced value.
    ///
    /// This function permits for using references since it doesn't defer its
    /// execution.
    pub fn call_with_diagnostics(
        &mut self,
        name: impl ToTypeHash,
        args: impl GuardedArgs,
        diagnostics: &mut dyn VmDiagnostics,
    ) -> Result<Value, VmError> {
        self.set_entrypoint(name, args.count())?;

        // Safety: We hold onto the guard until the vm has completed and
        // `VmExecution` will clear the stack before this function returns.
        // Erronously or not.
        let guard = unsafe { args.guarded_into_stack(&mut self.stack)? };

        let value = {
            // Clearing the stack here on panics has safety implications - see
            // above.
            let vm = ClearStack(self);
            VmExecution::new(&mut *vm.0)
                .resume()
                .with_diagnostics(diagnostics)
                .complete()
                .and_then(VmOutcome::into_complete)
        };

        // Note: this might panic if something in the vm is holding on to a
        // reference of the value. We should prevent it from being possible to
        // take any owned references to values held by this.
        drop(guard);
        value
    }

    /// Call the given function immediately asynchronously, returning the
    /// produced value.
    ///
    /// This function permits for using references since it doesn't defer its
    /// execution.
    pub async fn async_call<A, N>(&mut self, name: N, args: A) -> Result<Value, VmError>
    where
        N: ToTypeHash,
        A: GuardedArgs,
    {
        self.set_entrypoint(name, args.count())?;

        // Safety: We hold onto the guard until the vm has completed and
        // `VmExecution` will clear the stack before this function returns.
        // Erronously or not.
        let guard = unsafe { args.guarded_into_stack(&mut self.stack)? };

        let value = {
            // Clearing the stack here on panics has safety implications - see
            // above.
            let vm = ClearStack(self);
            VmExecution::new(&mut *vm.0)
                .resume()
                .await
                .and_then(VmOutcome::into_complete)
        };

        // Note: this might panic if something in the vm is holding on to a
        // reference of the value. We should prevent it from being possible to
        // take any owned references to values held by this.
        drop(guard);
        value
    }

    /// Update the instruction pointer to match the function matching the given
    /// name and check that the number of argument matches.
    fn set_entrypoint<N>(&mut self, name: N, count: usize) -> Result<(), VmErrorKind>
    where
        N: ToTypeHash,
    {
        let hash = name.to_type_hash();

        let Some(info) = self.unit.function(&hash) else {
            return Err(if let Some(item) = name.to_item()? {
                VmErrorKind::MissingEntry { hash, item }
            } else {
                VmErrorKind::MissingEntryHash { hash }
            });
        };

        let offset = match info {
            // NB: we ignore the calling convention.
            // everything is just async when called externally.
            UnitFn::Offset {
                offset,
                args: expected,
                ..
            } => {
                check_args(count, *expected)?;
                *offset
            }
            _ => {
                return Err(VmErrorKind::MissingFunction { hash });
            }
        };

        self.ip = offset;
        self.stack.clear();
        self.call_frames.clear();
        Ok(())
    }

    /// Helper function to call an instance function.
    #[inline]
    pub(crate) fn call_instance_fn(
        &mut self,
        isolated: Isolated,
        target: Value,
        hash: impl ToTypeHash,
        args: &mut dyn DynArgs,
        out: Output,
    ) -> Result<CallResult<()>, VmError> {
        let count = args.count().wrapping_add(1);
        let type_hash = target.type_hash();
        let hash = Hash::associated_function(type_hash, hash.to_type_hash());
        self.call_hash_with(isolated, hash, target, args, count, out)
    }

    /// Helper to call a field function.
    #[inline]
    fn call_field_fn(
        &mut self,
        protocol: impl IntoHash,
        target: Value,
        name: impl IntoHash,
        args: &mut dyn DynArgs,
        out: Output,
    ) -> Result<CallResult<()>, VmError> {
        let count = args.count().wrapping_add(1);
        let hash = Hash::field_function(protocol, target.type_hash(), name);
        self.call_hash_with(Isolated::None, hash, target, args, count, out)
    }

    /// Helper to call an index function.
    #[inline]
    fn call_index_fn(
        &mut self,
        protocol: impl IntoHash,
        target: Value,
        index: usize,
        args: &mut dyn DynArgs,
        out: Output,
    ) -> Result<CallResult<()>, VmError> {
        let count = args.count().wrapping_add(1);
        let hash = Hash::index_function(protocol, target.type_hash(), Hash::index(index));
        self.call_hash_with(Isolated::None, hash, target, args, count, out)
    }

    fn called_function_hook(&self, hash: Hash) -> Result<(), VmError> {
        runtime::env::exclusive(|_, _, diagnostics| {
            if let Some(diagnostics) = diagnostics {
                diagnostics.function_used(hash, self.ip())?;
            }

            Ok(())
        })
    }

    #[inline(never)]
    fn call_hash_with(
        &mut self,
        isolated: Isolated,
        hash: Hash,
        target: Value,
        args: &mut dyn DynArgs,
        count: usize,
        out: Output,
    ) -> Result<CallResult<()>, VmError> {
        if let Some(handler) = self.context.function(&hash) {
            let addr = self.stack.addr();

            self.called_function_hook(hash)?;
            self.stack.push(target)?;
            args.push_to_stack(&mut self.stack)?;

            let result = handler.call(&mut self.stack, addr, count, out);
            self.stack.truncate(addr);
            result?;
            return Ok(CallResult::Ok(()));
        }

        if let Some(UnitFn::Offset {
            offset,
            call,
            args: expected,
            ..
        }) = self.unit.function(&hash)
        {
            check_args(count, *expected)?;

            let addr = self.stack.addr();

            self.called_function_hook(hash)?;
            self.stack.push(target)?;
            args.push_to_stack(&mut self.stack)?;

            let result = self.call_offset_fn(*offset, *call, addr, count, isolated, out);

            if result? {
                self.stack.truncate(addr);
                return Ok(CallResult::Frame);
            } else {
                return Ok(CallResult::Ok(()));
            }
        }

        Ok(CallResult::Unsupported(target))
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn internal_cmp(
        &mut self,
        match_ordering: fn(Ordering) -> bool,
        lhs: Address,
        rhs: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let rhs = self.stack.at(rhs);
        let lhs = self.stack.at(lhs);

        let ordering = match (lhs.as_inline_unchecked(), rhs.as_inline_unchecked()) {
            (Some(lhs), Some(rhs)) => lhs.partial_cmp(rhs)?,
            _ => {
                let lhs = lhs.clone();
                let rhs = rhs.clone();
                Value::partial_cmp_with(&lhs, &rhs, self)?
            }
        };

        self.stack.store(out, || match ordering {
            Some(ordering) => match_ordering(ordering),
            None => false,
        })?;

        Ok(())
    }

    /// Push a new call frame.
    ///
    /// This will cause the `args` number of elements on the stack to be
    /// associated and accessible to the new call frame.
    #[tracing::instrument(skip(self), fields(call_frames = self.call_frames.len(), top = self.stack.top(), stack = self.stack.len(), self.ip))]
    pub(crate) fn push_call_frame(
        &mut self,
        ip: usize,
        addr: Address,
        args: usize,
        isolated: Isolated,
        out: Output,
    ) -> Result<(), VmErrorKind> {
        tracing::trace!("pushing call frame");

        let top = self.stack.swap_top(addr, args)?;
        let ip = replace(&mut self.ip, ip);

        let frame = CallFrame {
            ip,
            top,
            isolated,
            out,
        };

        self.call_frames.try_push(frame)?;
        Ok(())
    }

    /// Pop a call frame from an internal call, which needs the current stack
    /// pointer to be returned and does not check for context isolation through
    /// [`CallFrame::isolated`].
    #[tracing::instrument(skip(self), fields(call_frames = self.call_frames.len(), top = self.stack.top(), stack = self.stack.len(), self.ip))]
    pub(crate) fn pop_call_frame_from_call(&mut self) -> Option<usize> {
        tracing::trace!("popping call frame from call");
        let frame = self.call_frames.pop()?;
        tracing::trace!(?frame);
        self.stack.pop_stack_top(frame.top);
        Some(replace(&mut self.ip, frame.ip))
    }

    /// Pop a call frame and return it.
    #[tracing::instrument(skip(self), fields(call_frames = self.call_frames.len(), top = self.stack.top(), stack = self.stack.len(), self.ip))]
    pub(crate) fn pop_call_frame(&mut self) -> (Isolated, Option<Output>) {
        tracing::trace!("popping call frame");

        let Some(frame) = self.call_frames.pop() else {
            self.stack.pop_stack_top(0);
            return (Isolated::Isolated, None);
        };

        tracing::trace!(?frame);
        self.stack.pop_stack_top(frame.top);
        self.ip = frame.ip;
        (frame.isolated, Some(frame.out))
    }

    /// Implementation of getting a string index on an object-like type.
    fn try_object_like_index_get(target: &Value, field: &str) -> Result<Option<Value>, VmError> {
        match target.as_ref() {
            Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Struct) => {
                let Some(value) = data.get_field_ref(field)? else {
                    return Err(VmError::new(VmErrorKind::MissingField {
                        target: data.type_info(),
                        field: field.try_to_owned()?,
                    }));
                };

                Ok(Some(value.clone()))
            }
            Repr::Any(value) => match value.type_hash() {
                Object::HASH => {
                    let target = value.borrow_ref::<Object>()?;

                    let Some(value) = target.get(field) else {
                        return Err(VmError::new(VmErrorKind::MissingField {
                            target: TypeInfo::any::<Object>(),
                            field: field.try_to_owned()?,
                        }));
                    };

                    Ok(Some(value.clone()))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Implementation of getting a string index on an object-like type.
    fn try_tuple_like_index_get(target: &Value, index: usize) -> Result<Option<Value>, VmError> {
        let result = match target.as_ref() {
            Repr::Inline(target) => match target {
                Inline::Unit => Err(target.type_info()),
                _ => return Ok(None),
            },
            Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Tuple) => {
                match data.get_ref(index)? {
                    Some(value) => Ok(value.clone()),
                    None => Err(data.type_info()),
                }
            }
            Repr::Dynamic(data) => Err(data.type_info()),
            Repr::Any(target) => match target.type_hash() {
                Result::<Value, Value>::HASH => {
                    match (index, &*target.borrow_ref::<Result<Value, Value>>()?) {
                        (0, Ok(value)) => Ok(value.clone()),
                        (0, Err(value)) => Ok(value.clone()),
                        _ => Err(target.type_info()),
                    }
                }
                Option::<Value>::HASH => match (index, &*target.borrow_ref::<Option<Value>>()?) {
                    (0, Some(value)) => Ok(value.clone()),
                    _ => Err(target.type_info()),
                },
                GeneratorState::HASH => match (index, &*target.borrow_ref()?) {
                    (0, GeneratorState::Yielded(value)) => Ok(value.clone()),
                    (0, GeneratorState::Complete(value)) => Ok(value.clone()),
                    _ => Err(target.type_info()),
                },
                runtime::Vec::HASH => {
                    let vec = target.borrow_ref::<runtime::Vec>()?;

                    match vec.get(index) {
                        Some(value) => Ok(value.clone()),
                        None => Err(target.type_info()),
                    }
                }
                runtime::OwnedTuple::HASH => {
                    let tuple = target.borrow_ref::<runtime::OwnedTuple>()?;

                    match tuple.get(index) {
                        Some(value) => Ok(value.clone()),
                        None => Err(target.type_info()),
                    }
                }
                _ => {
                    return Ok(None);
                }
            },
        };

        match result {
            Ok(value) => Ok(Some(value)),
            Err(target) => Err(VmError::new(VmErrorKind::MissingIndexInteger {
                target,
                index: VmIntegerRepr::from(index),
            })),
        }
    }

    /// Implementation of getting a string index on an object-like type.
    fn try_tuple_like_index_set(
        target: &Value,
        index: usize,
        from: &Value,
    ) -> Result<bool, VmError> {
        match target.as_ref() {
            Repr::Inline(target) => match target {
                Inline::Unit => Ok(false),
                _ => Ok(false),
            },
            Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Tuple) => {
                if let Some(target) = data.borrow_mut()?.get_mut(index) {
                    target.clone_from(from);
                    return Ok(true);
                }

                Ok(false)
            }
            Repr::Dynamic(..) => Ok(false),
            Repr::Any(value) => match value.type_hash() {
                Result::<Value, Value>::HASH => {
                    let mut result = value.borrow_mut::<Result<Value, Value>>()?;

                    let target = match &mut *result {
                        Ok(ok) if index == 0 => ok,
                        Err(err) if index == 1 => err,
                        _ => return Ok(false),
                    };

                    target.clone_from(from);
                    Ok(true)
                }
                Option::<Value>::HASH => {
                    let mut option = value.borrow_mut::<Option<Value>>()?;

                    let target = match &mut *option {
                        Some(some) if index == 0 => some,
                        _ => return Ok(false),
                    };

                    target.clone_from(from);
                    Ok(true)
                }
                runtime::Vec::HASH => {
                    let mut vec = value.borrow_mut::<runtime::Vec>()?;

                    if let Some(target) = vec.get_mut(index) {
                        target.clone_from(from);
                        return Ok(true);
                    }

                    Ok(false)
                }
                runtime::OwnedTuple::HASH => {
                    let mut tuple = value.borrow_mut::<runtime::OwnedTuple>()?;

                    if let Some(target) = tuple.get_mut(index) {
                        target.clone_from(from);
                        return Ok(true);
                    }

                    Ok(false)
                }
                _ => Ok(false),
            },
        }
    }

    fn try_object_slot_index_set(
        target: &Value,
        field: &str,
        value: &Value,
    ) -> Result<bool, VmErrorKind> {
        match target.as_ref() {
            Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Struct) => {
                if let Some(mut v) = data.get_field_mut(field)? {
                    v.clone_from(value);
                    return Ok(true);
                }

                Err(VmErrorKind::MissingField {
                    target: data.type_info(),
                    field: field.try_to_owned()?,
                })
            }
            Repr::Any(target) => match target.type_hash() {
                Object::HASH => {
                    let mut target = target.borrow_mut::<Object>()?;

                    if let Some(target) = target.get_mut(field) {
                        target.clone_from(value);
                    } else {
                        let key = field.try_to_owned()?;
                        target.insert(key, value.clone())?;
                    }

                    Ok(true)
                }
                _ => Ok(false),
            },
            target => Err(VmErrorKind::MissingField {
                target: target.type_info(),
                field: field.try_to_owned()?,
            }),
        }
    }

    /// Internal implementation of the instance check.
    fn as_op(&mut self, lhs: Address, rhs: Address) -> Result<Value, VmError> {
        let b = self.stack.at(rhs);
        let a = self.stack.at(lhs);

        let Repr::Inline(Inline::Type(ty)) = b.as_ref() else {
            return Err(VmError::new(VmErrorKind::UnsupportedIs {
                value: a.type_info(),
                test_type: b.type_info(),
            }));
        };

        macro_rules! convert {
            ($from:ty, $value:expr) => {
                match ty.into_hash() {
                    f64::HASH => Value::from($value as f64),
                    u64::HASH => Value::from($value as u64),
                    i64::HASH => Value::from($value as i64),
                    ty => {
                        return Err(VmError::new(VmErrorKind::UnsupportedAs {
                            value: TypeInfo::from(<$from as TypeOf>::STATIC_TYPE_INFO),
                            type_hash: ty,
                        }));
                    }
                }
            };
        }

        let value = match a.as_ref() {
            Repr::Inline(Inline::Unsigned(a)) => convert!(u64, *a),
            Repr::Inline(Inline::Signed(a)) => convert!(i64, *a),
            Repr::Inline(Inline::Float(a)) => convert!(f64, *a),
            value => {
                return Err(VmError::new(VmErrorKind::UnsupportedAs {
                    value: value.type_info(),
                    type_hash: ty.into_hash(),
                }));
            }
        };

        Ok(value)
    }

    /// Internal implementation of the instance check.
    fn test_is_instance(&mut self, lhs: Address, rhs: Address) -> Result<bool, VmError> {
        let b = self.stack.at(rhs);
        let a = self.stack.at(lhs);

        let Some(Inline::Type(ty)) = b.as_inline() else {
            return Err(VmError::new(VmErrorKind::UnsupportedIs {
                value: a.type_info(),
                test_type: b.type_info(),
            }));
        };

        Ok(a.type_hash() == ty.into_hash())
    }

    fn internal_bool(
        &mut self,
        bool_op: impl FnOnce(bool, bool) -> bool,
        op: &'static str,
        lhs: Address,
        rhs: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let rhs = self.stack.at(rhs);
        let lhs = self.stack.at(lhs);

        let inline = match (lhs.as_ref(), rhs.as_ref()) {
            (Repr::Inline(Inline::Bool(lhs)), Repr::Inline(Inline::Bool(rhs))) => {
                let value = bool_op(*lhs, *rhs);
                Inline::Bool(value)
            }
            (lhs, rhs) => {
                return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                    op,
                    lhs: lhs.type_info(),
                    rhs: rhs.type_info(),
                }));
            }
        };

        self.stack.store(out, inline)?;
        Ok(())
    }

    /// Construct a future from calling an async function.
    fn call_generator_fn(
        &mut self,
        offset: usize,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<(), VmErrorKind> {
        let values = self.stack.slice_at_mut(addr, args)?;

        if let Some(at) = out.as_addr() {
            let stack = values.iter_mut().map(take).try_collect::<Stack>()?;
            let mut vm = Self::with_stack(self.context.clone(), self.unit.clone(), stack);
            vm.ip = offset;
            *self.stack.at_mut(at)? = Value::try_from(Generator::new(vm))?;
        } else {
            values.iter_mut().for_each(consume);
        }

        Ok(())
    }

    /// Construct a stream from calling a function.
    fn call_stream_fn(
        &mut self,
        offset: usize,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<(), VmErrorKind> {
        let values = self.stack.slice_at_mut(addr, args)?;

        if let Some(at) = out.as_addr() {
            let stack = values.iter_mut().map(take).try_collect::<Stack>()?;
            let mut vm = Self::with_stack(self.context.clone(), self.unit.clone(), stack);
            vm.ip = offset;
            *self.stack.at_mut(at)? = Value::try_from(Stream::new(vm))?;
        } else {
            values.iter_mut().for_each(consume);
        }

        Ok(())
    }

    /// Construct a future from calling a function.
    fn call_async_fn(
        &mut self,
        offset: usize,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<(), VmErrorKind> {
        let values = self.stack.slice_at_mut(addr, args)?;

        if let Some(at) = out.as_addr() {
            let stack = values.iter_mut().map(take).try_collect::<Stack>()?;
            let mut vm = Self::with_stack(self.context.clone(), self.unit.clone(), stack);
            vm.ip = offset;
            let mut execution = vm.into_execution();
            let future = Future::new(async move { execution.resume().await?.into_complete() })?;
            *self.stack.at_mut(at)? = Value::try_from(future)?;
        } else {
            values.iter_mut().for_each(consume);
        }

        Ok(())
    }

    /// Helper function to call the function at the given offset.
    #[cfg_attr(feature = "bench", inline(never))]
    fn call_offset_fn(
        &mut self,
        offset: usize,
        call: Call,
        addr: Address,
        args: usize,
        isolated: Isolated,
        out: Output,
    ) -> Result<bool, VmErrorKind> {
        let moved = match call {
            Call::Async => {
                self.call_async_fn(offset, addr, args, out)?;
                false
            }
            Call::Immediate => {
                self.push_call_frame(offset, addr, args, isolated, out)?;
                true
            }
            Call::Stream => {
                self.call_stream_fn(offset, addr, args, out)?;
                false
            }
            Call::Generator => {
                self.call_generator_fn(offset, addr, args, out)?;
                false
            }
        };

        Ok(moved)
    }

    /// Execute a fallback operation.
    #[cfg_attr(feature = "bench", inline(never))]
    fn target_fallback_assign(
        &mut self,
        fallback: TargetFallback,
        protocol: &Protocol,
    ) -> Result<(), VmError> {
        match fallback {
            TargetFallback::Value(lhs, rhs) => {
                let mut args = DynGuardedArgs::new((rhs.clone(),));

                if let CallResult::Unsupported(lhs) = self.call_instance_fn(
                    Isolated::None,
                    lhs,
                    protocol.hash,
                    &mut args,
                    Output::discard(),
                )? {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: protocol.name,
                        lhs: lhs.type_info(),
                        rhs: rhs.type_info(),
                    }));
                };
            }
            TargetFallback::Field(lhs, hash, slot, rhs) => {
                let mut args = DynGuardedArgs::new((rhs,));

                if let CallResult::Unsupported(lhs) =
                    self.call_field_fn(protocol, lhs.clone(), hash, &mut args, Output::discard())?
                {
                    let Some(field) = self.unit.lookup_string(slot) else {
                        return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
                    };

                    return Err(VmError::new(VmErrorKind::UnsupportedObjectSlotIndexGet {
                        target: lhs.type_info(),
                        field: field.clone(),
                    }));
                }
            }
            TargetFallback::Index(lhs, index, rhs) => {
                let mut args = DynGuardedArgs::new((rhs,));

                if let CallResult::Unsupported(lhs) = self.call_index_fn(
                    protocol.hash,
                    lhs.clone(),
                    index,
                    &mut args,
                    Output::discard(),
                )? {
                    return Err(VmError::new(VmErrorKind::UnsupportedTupleIndexGet {
                        target: lhs.type_info(),
                        index,
                    }));
                }
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_await(&mut self, addr: Address) -> Result<Future, VmError> {
        Ok(self.stack.at(addr).clone().into_future()?)
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_select(
        &mut self,
        addr: Address,
        len: usize,
        value: Output,
    ) -> Result<Option<Select>, VmError> {
        let futures = futures_util::stream::FuturesUnordered::new();

        for (branch, value) in self.stack.slice_at(addr, len)?.iter().enumerate() {
            let future = value.clone().into_mut::<Future>()?;

            if !future.is_completed() {
                futures.push(SelectFuture::new(self.ip + branch, future));
            }
        }

        if futures.is_empty() {
            self.stack.store(value, ())?;
            self.ip = self.ip.wrapping_add(len);
            return Ok(None);
        }

        Ok(Some(Select::new(futures)))
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_store(&mut self, value: InstValue, out: Output) -> Result<(), VmError> {
        self.stack.store(out, value.into_value())?;
        Ok(())
    }

    /// Copy a value from a position relative to the top of the stack, to the
    /// top of the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_copy(&mut self, addr: Address, out: Output) -> Result<(), VmError> {
        self.stack.copy(addr, out)?;
        Ok(())
    }

    /// Move a value from a position relative to the top of the stack, to the
    /// top of the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_move(&mut self, addr: Address, out: Output) -> Result<(), VmError> {
        let value = self.stack.at(addr).clone();
        let value = value.move_()?;
        self.stack.store(out, value)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_drop(&mut self, set: usize) -> Result<(), VmError> {
        let Some(addresses) = self.unit.lookup_drop_set(set) else {
            return Err(VmError::new(VmErrorKind::MissingDropSet { set }));
        };

        for &addr in addresses {
            *self.stack.at_mut(addr)? = Value::empty();
        }

        Ok(())
    }

    /// Swap two values on the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_swap(&mut self, a: Address, b: Address) -> Result<(), VmError> {
        self.stack.swap(a, b)?;
        Ok(())
    }

    /// Perform a jump operation.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_jump(&mut self, jump: usize) -> Result<(), VmError> {
        self.ip = self.unit.translate(jump)?;
        Ok(())
    }

    /// Perform a conditional jump operation.
    #[cfg_attr(feature = "bench", inline(never))]
    #[cfg_attr(not(feature = "bench"), inline)]
    fn op_jump_if(&mut self, cond: Address, jump: usize) -> Result<(), VmErrorKind> {
        if matches!(
            self.stack.at(cond).as_ref(),
            Repr::Inline(Inline::Bool(true))
        ) {
            self.ip = self.unit.translate(jump)?;
        }

        Ok(())
    }

    /// pop-and-jump-if-not instruction.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_jump_if_not(&mut self, cond: Address, jump: usize) -> Result<(), VmErrorKind> {
        if matches!(
            self.stack.at(cond).as_ref(),
            Repr::Inline(Inline::Bool(false))
        ) {
            self.ip = self.unit.translate(jump)?;
        }

        Ok(())
    }

    /// Construct a new vec.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_vec(&mut self, addr: Address, count: usize, out: Output) -> Result<(), VmError> {
        let vec = self.stack.slice_at_mut(addr, count)?;
        let vec = vec
            .iter_mut()
            .map(take)
            .try_collect::<alloc::Vec<Value>>()?;
        self.stack.store(out, Vec::from(vec))?;
        Ok(())
    }

    /// Construct a new tuple.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_tuple(&mut self, addr: Address, count: usize, out: Output) -> Result<(), VmError> {
        let tuple = self.stack.slice_at_mut(addr, count)?;

        let tuple = tuple
            .iter_mut()
            .map(take)
            .try_collect::<alloc::Vec<Value>>()?;

        self.stack.store(out, || OwnedTuple::try_from(tuple))?;
        Ok(())
    }

    /// Construct a new tuple with a fixed number of arguments.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_tuple_n(&mut self, addr: &[Address], out: Output) -> Result<(), VmError> {
        let mut tuple = alloc::Vec::<Value>::try_with_capacity(addr.len())?;

        for &arg in addr {
            let value = self.stack.at(arg).clone();
            tuple.try_push(value)?;
        }

        self.stack.store(out, || OwnedTuple::try_from(tuple))?;
        Ok(())
    }

    /// Push the tuple that is on top of the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_environment(&mut self, addr: Address, count: usize, out: Output) -> Result<(), VmError> {
        let tuple = self.stack.at(addr).clone();
        let tuple = tuple.borrow_tuple_ref()?;

        if tuple.len() != count {
            return Err(VmError::new(VmErrorKind::BadEnvironmentCount {
                expected: count,
                actual: tuple.len(),
            }));
        }

        if let Some(addr) = out.as_addr() {
            let out = self.stack.slice_at_mut(addr, count)?;

            for (value, out) in tuple.iter().zip(out.iter_mut()) {
                out.clone_from(value);
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_allocate(&mut self, size: usize) -> Result<(), VmError> {
        self.stack.resize(size)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_not(&mut self, addr: Address, out: Output) -> Result<(), VmError> {
        self.unary(addr, out, &Protocol::NOT, |inline| match *inline {
            Inline::Bool(value) => Some(Inline::Bool(!value)),
            Inline::Unsigned(value) => Some(Inline::Unsigned(!value)),
            Inline::Signed(value) => Some(Inline::Signed(!value)),
            _ => None,
        })
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_neg(&mut self, addr: Address, out: Output) -> Result<(), VmError> {
        self.unary(addr, out, &Protocol::NEG, |inline| match *inline {
            Inline::Signed(value) => Some(Inline::Signed(-value)),
            Inline::Float(value) => Some(Inline::Float(-value)),
            _ => None,
        })
    }

    fn unary(
        &mut self,
        operand: Address,
        out: Output,
        protocol: &'static Protocol,
        op: impl FnOnce(&Inline) -> Option<Inline>,
    ) -> Result<(), VmError> {
        let operand = self.stack.at(operand);

        'fallback: {
            let store = match operand.as_ref() {
                Repr::Inline(inline) => op(inline),
                Repr::Any(..) => break 'fallback,
                _ => None,
            };

            let Some(store) = store else {
                return Err(VmError::new(VmErrorKind::UnsupportedUnaryOperation {
                    op: protocol.name,
                    operand: operand.type_info(),
                }));
            };

            self.stack.store(out, store)?;
            return Ok(());
        };

        let operand = operand.clone();

        if let CallResult::Unsupported(operand) =
            self.call_instance_fn(Isolated::None, operand, protocol, &mut (), out)?
        {
            return Err(VmError::new(VmErrorKind::UnsupportedUnaryOperation {
                op: protocol.name,
                operand: operand.type_info(),
            }));
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_op(
        &mut self,
        op: InstOp,
        lhs: Address,
        rhs: Address,
        out: Output,
    ) -> Result<(), VmError> {
        match op {
            InstOp::Lt => {
                self.internal_cmp(|o| matches!(o, Ordering::Less), lhs, rhs, out)?;
            }
            InstOp::Le => {
                self.internal_cmp(
                    |o| matches!(o, Ordering::Less | Ordering::Equal),
                    lhs,
                    rhs,
                    out,
                )?;
            }
            InstOp::Gt => {
                self.internal_cmp(|o| matches!(o, Ordering::Greater), lhs, rhs, out)?;
            }
            InstOp::Ge => {
                self.internal_cmp(
                    |o| matches!(o, Ordering::Greater | Ordering::Equal),
                    lhs,
                    rhs,
                    out,
                )?;
            }
            InstOp::Eq => {
                let rhs = self.stack.at(rhs);
                let lhs = self.stack.at(lhs);

                let test = if let (Some(lhs), Some(rhs)) = (lhs.as_inline(), rhs.as_inline()) {
                    lhs.partial_eq(rhs)?
                } else {
                    let lhs = lhs.clone();
                    let rhs = rhs.clone();
                    Value::partial_eq_with(&lhs, &rhs, self)?
                };

                self.stack.store(out, test)?;
            }
            InstOp::Neq => {
                let rhs = self.stack.at(rhs);
                let lhs = self.stack.at(lhs);

                let test = if let (Some(lhs), Some(rhs)) = (lhs.as_inline(), rhs.as_inline()) {
                    lhs.partial_eq(rhs)?
                } else {
                    let lhs = lhs.clone();
                    let rhs = rhs.clone();
                    Value::partial_eq_with(&lhs, &rhs, self)?
                };

                self.stack.store(out, !test)?;
            }
            InstOp::And => {
                self.internal_bool(|a, b| a && b, "&&", lhs, rhs, out)?;
            }
            InstOp::Or => {
                self.internal_bool(|a, b| a || b, "||", lhs, rhs, out)?;
            }
            InstOp::As => {
                let value = self.as_op(lhs, rhs)?;
                self.stack.store(out, value)?;
            }
            InstOp::Is => {
                let is_instance = self.test_is_instance(lhs, rhs)?;
                self.stack.store(out, is_instance)?;
            }
            InstOp::IsNot => {
                let is_instance = self.test_is_instance(lhs, rhs)?;
                self.stack.store(out, !is_instance)?;
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_arithmetic(
        &mut self,
        op: InstArithmeticOp,
        lhs: Address,
        rhs: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let ops = ArithmeticOps::from_op(op);

        let lhs = self.stack.at(lhs);
        let rhs = self.stack.at(rhs);

        'fallback: {
            let inline = match (lhs.as_ref(), rhs.as_ref()) {
                (Repr::Inline(lhs), Repr::Inline(rhs)) => match (lhs, rhs) {
                    (Inline::Unsigned(lhs), rhs) => {
                        let rhs = rhs.as_integer()?;
                        let value = (ops.u64)(*lhs, rhs).ok_or_else(ops.error)?;
                        Inline::Unsigned(value)
                    }
                    (Inline::Signed(lhs), rhs) => {
                        let rhs = rhs.as_integer()?;
                        let value = (ops.i64)(*lhs, rhs).ok_or_else(ops.error)?;
                        Inline::Signed(value)
                    }
                    (Inline::Float(lhs), Inline::Float(rhs)) => {
                        let value = (ops.f64)(*lhs, *rhs);
                        Inline::Float(value)
                    }
                    (lhs, rhs) => {
                        return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                            op: ops.protocol.name,
                            lhs: lhs.type_info(),
                            rhs: rhs.type_info(),
                        }));
                    }
                },
                (Repr::Any(..), ..) => {
                    break 'fallback;
                }
                (lhs, rhs) => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: lhs.type_info(),
                        rhs: rhs.type_info(),
                    }));
                }
            };

            self.stack.store(out, inline)?;
            return Ok(());
        }

        let lhs = lhs.clone();
        let rhs = rhs.clone();

        let mut args = DynGuardedArgs::new((rhs.clone(),));

        if let CallResult::Unsupported(lhs) =
            self.call_instance_fn(Isolated::None, lhs, &ops.protocol, &mut args, out)?
        {
            return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                op: ops.protocol.name,
                lhs: lhs.type_info(),
                rhs: rhs.type_info(),
            }));
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_bitwise(
        &mut self,
        op: InstBitwiseOp,
        lhs: Address,
        rhs: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let ops = BitwiseOps::from_op(op);

        let lhs = self.stack.at(lhs);
        let rhs = self.stack.at(rhs);

        'fallback: {
            let inline = match (lhs.as_ref(), rhs.as_ref()) {
                (Repr::Inline(Inline::Unsigned(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    let value = (ops.u64)(*lhs, rhs);
                    Inline::Unsigned(value)
                }
                (Repr::Inline(Inline::Signed(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    let value = (ops.i64)(*lhs, rhs);
                    Inline::Signed(value)
                }
                (Repr::Inline(Inline::Bool(lhs)), Repr::Inline(Inline::Bool(rhs))) => {
                    let value = (ops.bool)(*lhs, *rhs);
                    Inline::Bool(value)
                }
                (Repr::Any(_), _) => {
                    break 'fallback;
                }
                (lhs, rhs) => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: lhs.type_info(),
                        rhs: rhs.type_info(),
                    }));
                }
            };

            self.stack.store(out, inline)?;
            return Ok(());
        };

        let lhs = lhs.clone();
        let rhs = rhs.clone();

        let mut args = DynGuardedArgs::new((&rhs,));

        if let CallResult::Unsupported(lhs) =
            self.call_instance_fn(Isolated::None, lhs, &ops.protocol, &mut args, out)?
        {
            return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                op: ops.protocol.name,
                lhs: lhs.type_info(),
                rhs: rhs.type_info(),
            }));
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_shift(
        &mut self,
        op: InstShiftOp,
        lhs: Address,
        rhs: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let ops = ShiftOps::from_op(op);

        let (lhs, rhs) = 'fallback: {
            let inline = {
                match self.stack.pair(lhs, rhs)? {
                    Pair::Same(value) => match value.as_mut() {
                        Repr::Inline(Inline::Unsigned(value)) => {
                            let shift = u32::try_from(*value).ok().ok_or_else(ops.error)?;
                            let value = (ops.u64)(*value, shift).ok_or_else(ops.error)?;
                            Inline::Unsigned(value)
                        }
                        Repr::Inline(Inline::Signed(value)) => {
                            let shift = u32::try_from(*value).ok().ok_or_else(ops.error)?;
                            let value = (ops.i64)(*value, shift).ok_or_else(ops.error)?;
                            Inline::Signed(value)
                        }
                        Repr::Any(..) => break 'fallback (value.clone(), value.clone()),
                        value => {
                            return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                                op: ops.protocol.name,
                                lhs: value.type_info(),
                                rhs: value.type_info(),
                            }));
                        }
                    },
                    Pair::Pair(lhs, rhs) => match (lhs.as_mut(), rhs.as_ref()) {
                        (Repr::Inline(Inline::Unsigned(lhs)), Repr::Inline(rhs)) => {
                            let rhs = rhs.as_integer()?;
                            let value = (ops.u64)(*lhs, rhs).ok_or_else(ops.error)?;
                            Inline::Unsigned(value)
                        }
                        (Repr::Inline(Inline::Signed(lhs)), Repr::Inline(rhs)) => {
                            let rhs = rhs.as_integer()?;
                            let value = (ops.i64)(*lhs, rhs).ok_or_else(ops.error)?;
                            Inline::Signed(value)
                        }
                        (Repr::Any(..), _) => {
                            break 'fallback (lhs.clone(), rhs.clone());
                        }
                        (lhs, rhs) => {
                            return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                                op: ops.protocol.name,
                                lhs: lhs.type_info(),
                                rhs: rhs.type_info(),
                            }));
                        }
                    },
                }
            };

            self.stack.store(out, inline)?;
            return Ok(());
        };

        let mut args = DynGuardedArgs::new((rhs.clone(),));

        if let CallResult::Unsupported(lhs) =
            self.call_instance_fn(Isolated::None, lhs, &ops.protocol, &mut args, out)?
        {
            return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                op: ops.protocol.name,
                lhs: lhs.type_info(),
                rhs: rhs.type_info(),
            }));
        }

        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_assign_arithmetic(
        &mut self,
        op: InstArithmeticOp,
        target: InstTarget,
        rhs: Address,
    ) -> Result<(), VmError> {
        let ops = AssignArithmeticOps::from_op(op);

        let fallback = match target_value(&mut self.stack, &self.unit, target, rhs)? {
            TargetValue::Same(value) => match value.as_mut() {
                Repr::Inline(Inline::Signed(value)) => {
                    let out = (ops.i64)(*value, *value).ok_or_else(ops.error)?;
                    *value = out;
                    return Ok(());
                }
                Repr::Inline(Inline::Unsigned(value)) => {
                    let out = (ops.u64)(*value, *value).ok_or_else(ops.error)?;
                    *value = out;
                    return Ok(());
                }
                Repr::Inline(Inline::Float(value)) => {
                    let out = (ops.f64)(*value, *value);
                    *value = out;
                    return Ok(());
                }
                Repr::Any(..) => TargetFallback::Value(value.clone(), value.clone()),
                value => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: value.type_info(),
                        rhs: value.type_info(),
                    }));
                }
            },
            TargetValue::Pair(mut lhs, rhs) => match (lhs.as_mut(), rhs.as_ref()) {
                (Repr::Inline(Inline::Signed(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    let out = (ops.i64)(*lhs, rhs).ok_or_else(ops.error)?;
                    *lhs = out;
                    return Ok(());
                }
                (Repr::Inline(Inline::Unsigned(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    let out = (ops.u64)(*lhs, rhs).ok_or_else(ops.error)?;
                    *lhs = out;
                    return Ok(());
                }
                (Repr::Inline(Inline::Float(lhs)), Repr::Inline(Inline::Float(rhs))) => {
                    let out = (ops.f64)(*lhs, *rhs);
                    *lhs = out;
                    return Ok(());
                }
                (Repr::Any(..), _) => TargetFallback::Value(lhs.clone(), rhs.clone()),
                (lhs, rhs) => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: lhs.type_info(),
                        rhs: rhs.type_info(),
                    }));
                }
            },
            TargetValue::Fallback(fallback) => fallback,
        };

        self.target_fallback_assign(fallback, &ops.protocol)
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_assign_bitwise(
        &mut self,
        op: InstBitwiseOp,
        target: InstTarget,
        rhs: Address,
    ) -> Result<(), VmError> {
        let ops = AssignBitwiseOps::from_ops(op);

        let fallback = match target_value(&mut self.stack, &self.unit, target, rhs)? {
            TargetValue::Same(value) => match value.as_mut() {
                Repr::Inline(Inline::Unsigned(value)) => {
                    let rhs = *value;
                    (ops.u64)(value, rhs);
                    return Ok(());
                }
                Repr::Inline(Inline::Signed(value)) => {
                    let rhs = *value;
                    (ops.i64)(value, rhs);
                    return Ok(());
                }
                Repr::Inline(Inline::Bool(value)) => {
                    let rhs = *value;
                    (ops.bool)(value, rhs);
                    return Ok(());
                }
                Repr::Any(..) => TargetFallback::Value(value.clone(), value.clone()),
                value => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: value.type_info(),
                        rhs: value.type_info(),
                    }));
                }
            },
            TargetValue::Pair(mut lhs, rhs) => match (lhs.as_mut(), rhs.as_ref()) {
                (Repr::Inline(Inline::Unsigned(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    (ops.u64)(lhs, rhs);
                    return Ok(());
                }
                (Repr::Inline(Inline::Signed(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    (ops.i64)(lhs, rhs);
                    return Ok(());
                }
                (Repr::Inline(Inline::Bool(lhs)), Repr::Inline(Inline::Bool(rhs))) => {
                    (ops.bool)(lhs, *rhs);
                    return Ok(());
                }
                (Repr::Any(..), ..) => TargetFallback::Value(lhs.clone(), rhs.clone()),
                (lhs, rhs) => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: lhs.type_info(),
                        rhs: rhs.type_info(),
                    }));
                }
            },
            TargetValue::Fallback(fallback) => fallback,
        };

        self.target_fallback_assign(fallback, &ops.protocol)
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_assign_shift(
        &mut self,
        op: InstShiftOp,
        target: InstTarget,
        rhs: Address,
    ) -> Result<(), VmError> {
        let ops = AssignShiftOps::from_op(op);

        let fallback = match target_value(&mut self.stack, &self.unit, target, rhs)? {
            TargetValue::Same(value) => match value.as_mut() {
                Repr::Inline(Inline::Unsigned(value)) => {
                    let shift = u32::try_from(*value).ok().ok_or_else(ops.error)?;
                    let out = (ops.u64)(*value, shift).ok_or_else(ops.error)?;
                    *value = out;
                    return Ok(());
                }
                Repr::Inline(Inline::Signed(value)) => {
                    let shift = u32::try_from(*value).ok().ok_or_else(ops.error)?;
                    let out = (ops.i64)(*value, shift).ok_or_else(ops.error)?;
                    *value = out;
                    return Ok(());
                }
                Repr::Any(..) => TargetFallback::Value(value.clone(), value.clone()),
                value => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: value.type_info(),
                        rhs: value.type_info(),
                    }));
                }
            },
            TargetValue::Pair(mut lhs, rhs) => match (lhs.as_mut(), rhs.as_ref()) {
                (Repr::Inline(Inline::Unsigned(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    let out = (ops.u64)(*lhs, rhs).ok_or_else(ops.error)?;
                    *lhs = out;
                    return Ok(());
                }
                (Repr::Inline(Inline::Signed(lhs)), Repr::Inline(rhs)) => {
                    let rhs = rhs.as_integer()?;
                    let out = (ops.i64)(*lhs, rhs).ok_or_else(ops.error)?;
                    *lhs = out;
                    return Ok(());
                }
                (Repr::Any(..), _) => TargetFallback::Value(lhs.clone(), rhs.clone()),
                (lhs, rhs) => {
                    return Err(VmError::new(VmErrorKind::UnsupportedBinaryOperation {
                        op: ops.protocol.name,
                        lhs: lhs.type_info(),
                        rhs: rhs.type_info(),
                    }));
                }
            },
            TargetValue::Fallback(fallback) => fallback,
        };

        self.target_fallback_assign(fallback, &ops.protocol)
    }

    /// Perform an index set operation.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_index_set(
        &mut self,
        target: Address,
        index: Address,
        value: Address,
    ) -> Result<(), VmError> {
        let target = self.stack.at(target);
        let index = self.stack.at(index);
        let value = self.stack.at(value);

        if let Some(field) = index.try_borrow_ref::<String>()? {
            if Self::try_object_slot_index_set(target, &field, value)? {
                return Ok(());
            }
        }

        let target = target.clone();
        let index = index.clone();
        let value = value.clone();

        let mut args = DynGuardedArgs::new((&index, &value));

        if let CallResult::Unsupported(target) = self.call_instance_fn(
            Isolated::None,
            target,
            &Protocol::INDEX_SET,
            &mut args,
            Output::discard(),
        )? {
            return Err(VmError::new(VmErrorKind::UnsupportedIndexSet {
                target: target.type_info(),
                index: index.type_info(),
                value: value.type_info(),
            }));
        }

        Ok(())
    }

    #[inline]
    #[tracing::instrument(skip(self, return_value))]
    fn op_return_internal(&mut self, return_value: Value) -> Result<Option<Output>, VmError> {
        let (exit, out) = self.pop_call_frame();

        let out = if let Some(out) = out {
            self.stack.store(out, return_value)?;
            out
        } else {
            let addr = self.stack.addr();
            self.stack.push(return_value)?;
            addr.output()
        };

        Ok(exit.then_some(out))
    }

    fn lookup_function_by_hash(&self, hash: Hash) -> Result<Function, VmErrorKind> {
        let Some(info) = self.unit.function(&hash) else {
            let Some(handler) = self.context.function(&hash) else {
                return Err(VmErrorKind::MissingContextFunction { hash });
            };

            return Ok(Function::from_handler(handler.clone(), hash));
        };

        let f = match info {
            UnitFn::Offset {
                offset, call, args, ..
            } => Function::from_vm_offset(
                self.context.clone(),
                self.unit.clone(),
                *offset,
                *call,
                *args,
                hash,
            ),
            UnitFn::EmptyStruct { hash } => {
                let Some(rtti) = self.unit.lookup_rtti(hash) else {
                    return Err(VmErrorKind::MissingRtti { hash: *hash });
                };

                Function::from_unit_struct(rtti.clone())
            }
            UnitFn::TupleStruct { hash, args } => {
                let Some(rtti) = self.unit.lookup_rtti(hash) else {
                    return Err(VmErrorKind::MissingRtti { hash: *hash });
                };

                Function::from_tuple_struct(rtti.clone(), *args)
            }
        };

        Ok(f)
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_return(&mut self, addr: Address) -> Result<Option<Output>, VmError> {
        let return_value = self.stack.at(addr).clone();
        self.op_return_internal(return_value)
    }

    #[cfg_attr(feature = "bench", inline(never))]
    #[tracing::instrument(skip(self))]
    fn op_return_unit(&mut self) -> Result<Option<Output>, VmError> {
        let (exit, out) = self.pop_call_frame();

        let out = if let Some(out) = out {
            self.stack.store(out, ())?;
            out
        } else {
            let addr = self.stack.addr();
            self.stack.push(())?;
            addr.output()
        };

        Ok(exit.then_some(out))
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_load_instance_fn(
        &mut self,
        addr: Address,
        hash: Hash,
        out: Output,
    ) -> Result<(), VmError> {
        let instance = self.stack.at(addr);
        let ty = instance.type_hash();
        let hash = Hash::associated_function(ty, hash);
        self.stack.store(out, || Type::new(hash))?;
        Ok(())
    }

    /// Perform an index get operation.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_index_get(
        &mut self,
        target: Address,
        index: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let value = 'store: {
            let index = self.stack.at(index);
            let target = self.stack.at(target);

            match index.as_ref() {
                Repr::Inline(inline) => {
                    let index = inline.as_integer::<usize>()?;

                    if let Some(value) = Self::try_tuple_like_index_get(target, index)? {
                        break 'store value;
                    }
                }
                Repr::Any(value) => {
                    if let Some(index) = value.try_borrow_ref::<String>()? {
                        if let Some(value) =
                            Self::try_object_like_index_get(target, index.as_str())?
                        {
                            break 'store value;
                        }
                    }
                }
                _ => {}
            }

            let target = target.clone();
            let index = index.clone();

            let mut args = DynGuardedArgs::new((&index,));

            if let CallResult::Unsupported(target) =
                self.call_instance_fn(Isolated::None, target, &Protocol::INDEX_GET, &mut args, out)?
            {
                return Err(VmError::new(VmErrorKind::UnsupportedIndexGet {
                    target: target.type_info(),
                    index: index.type_info(),
                }));
            }

            return Ok(());
        };

        self.stack.store(out, value)?;
        Ok(())
    }

    /// Perform an index get operation specialized for tuples.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_tuple_index_set(
        &mut self,
        target: Address,
        index: usize,
        value: Address,
    ) -> Result<(), VmError> {
        let value = self.stack.at(value);
        let target = self.stack.at(target);

        if Self::try_tuple_like_index_set(target, index, value)? {
            return Ok(());
        }

        Err(VmError::new(VmErrorKind::UnsupportedTupleIndexSet {
            target: target.type_info(),
        }))
    }

    /// Perform an index get operation specialized for tuples.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_tuple_index_get_at(
        &mut self,
        addr: Address,
        index: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let value = self.stack.at(addr);

        if let Some(value) = Self::try_tuple_like_index_get(value, index)? {
            self.stack.store(out, value)?;
            return Ok(());
        }

        let value = value.clone();

        if let CallResult::Unsupported(value) =
            self.call_index_fn(&Protocol::GET, value, index, &mut (), out)?
        {
            return Err(VmError::new(VmErrorKind::UnsupportedTupleIndexGet {
                target: value.type_info(),
                index,
            }));
        }

        Ok(())
    }

    /// Perform a specialized index set operation on an object.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_object_index_set(
        &mut self,
        target: Address,
        slot: usize,
        value: Address,
    ) -> Result<(), VmError> {
        let target = self.stack.at(target);
        let value = self.stack.at(value);

        let Some(field) = self.unit.lookup_string(slot) else {
            return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
        };

        if Self::try_object_slot_index_set(target, field, value)? {
            return Ok(());
        }

        let target = target.clone();
        let value = value.clone();

        let hash = field.hash();

        let mut args = DynGuardedArgs::new((value,));

        let result =
            self.call_field_fn(&Protocol::SET, target, hash, &mut args, Output::discard())?;

        if let CallResult::Unsupported(target) = result {
            let Some(field) = self.unit.lookup_string(slot) else {
                return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
            };

            return Err(VmError::new(VmErrorKind::UnsupportedObjectSlotIndexSet {
                target: target.type_info(),
                field: field.clone(),
            }));
        };

        Ok(())
    }

    /// Perform a specialized index get operation on an object.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_object_index_get_at(
        &mut self,
        addr: Address,
        slot: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let target = self.stack.at(addr);

        let Some(index) = self.unit.lookup_string(slot) else {
            return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
        };

        match target.as_ref() {
            Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Struct) => {
                let Some(value) = data.get_field_ref(index.as_str())? else {
                    return Err(VmError::new(VmErrorKind::ObjectIndexMissing { slot }));
                };

                let value = value.clone();
                self.stack.store(out, value)?;
                return Ok(());
            }
            Repr::Any(value) if value.type_hash() == Object::HASH => {
                let object = value.borrow_ref::<Object>()?;

                let Some(value) = object.get(index.as_str()) else {
                    return Err(VmError::new(VmErrorKind::ObjectIndexMissing { slot }));
                };

                let value = value.clone();
                self.stack.store(out, value)?;
                return Ok(());
            }
            Repr::Any(..) => {}
            target => {
                return Err(VmError::new(VmErrorKind::UnsupportedObjectSlotIndexGet {
                    target: target.type_info(),
                    field: index.clone(),
                }));
            }
        }

        let target = target.clone();

        if let CallResult::Unsupported(target) =
            self.call_field_fn(&Protocol::GET, target, index.hash(), &mut (), out)?
        {
            let Some(field) = self.unit.lookup_string(slot) else {
                return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
            };

            return Err(VmError::new(VmErrorKind::UnsupportedObjectSlotIndexGet {
                target: target.type_info(),
                field: field.clone(),
            }));
        }

        Ok(())
    }

    /// Operation to allocate an object.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_object(&mut self, addr: Address, slot: usize, out: Output) -> Result<(), VmError> {
        let Some(keys) = self.unit.lookup_object_keys(slot) else {
            return Err(VmError::new(VmErrorKind::MissingStaticObjectKeys { slot }));
        };

        let mut object = Object::with_capacity(keys.len())?;
        let values = self.stack.slice_at_mut(addr, keys.len())?;

        for (key, value) in keys.iter().zip(values) {
            let key = String::try_from(key.as_str())?;
            object.insert(key, take(value))?;
        }

        self.stack.store(out, object)?;
        Ok(())
    }

    /// Operation to allocate an object.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_range(&mut self, range: InstRange, out: Output) -> Result<(), VmError> {
        let value = match range {
            InstRange::RangeFrom { start } => {
                let s = self.stack.at(start).clone();
                Value::new(RangeFrom::new(s.clone()))?
            }
            InstRange::RangeFull => Value::new(RangeFull::new())?,
            InstRange::RangeInclusive { start, end } => {
                let s = self.stack.at(start).clone();
                let e = self.stack.at(end).clone();
                Value::new(RangeInclusive::new(s.clone(), e.clone()))?
            }
            InstRange::RangeToInclusive { end } => {
                let e = self.stack.at(end).clone();
                Value::new(RangeToInclusive::new(e.clone()))?
            }
            InstRange::RangeTo { end } => {
                let e = self.stack.at(end).clone();
                Value::new(RangeTo::new(e.clone()))?
            }
            InstRange::Range { start, end } => {
                let s = self.stack.at(start).clone();
                let e = self.stack.at(end).clone();
                Value::new(Range::new(s.clone(), e.clone()))?
            }
        };

        self.stack.store(out, value)?;
        Ok(())
    }

    /// Operation to allocate an object struct.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_struct(&mut self, addr: Address, hash: Hash, out: Output) -> Result<(), VmError> {
        let Some(rtti) = self.unit.lookup_rtti(&hash) else {
            return Err(VmError::new(VmErrorKind::MissingRtti { hash }));
        };

        let values = self.stack.slice_at_mut(addr, rtti.fields.len())?;
        let value = AnySequence::new(rtti.clone(), values.iter_mut().map(take))?;
        self.stack.store(out, value)?;
        Ok(())
    }

    /// Operation to allocate a constant value from an array of values.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_const_construct(
        &mut self,
        addr: Address,
        hash: Hash,
        count: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let values = self.stack.slice_at_mut(addr, count)?;

        let Some(construct) = self.context.construct(&hash) else {
            return Err(VmError::new(VmErrorKind::MissingConstantConstructor {
                hash,
            }));
        };

        let value = construct.runtime_construct(values)?;
        self.stack.store(out, value)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_string(&mut self, slot: usize, out: Output) -> Result<(), VmError> {
        let Some(string) = self.unit.lookup_string(slot) else {
            return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
        };

        self.stack.store(out, string.as_str())?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_bytes(&mut self, slot: usize, out: Output) -> Result<(), VmError> {
        let Some(bytes) = self.unit.lookup_bytes(slot) else {
            return Err(VmError::new(VmErrorKind::MissingStaticBytes { slot }));
        };

        self.stack.store(out, bytes)?;
        Ok(())
    }

    /// Optimize operation to perform string concatenation.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_string_concat(
        &mut self,
        addr: Address,
        len: usize,
        size_hint: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let values = self.stack.slice_at(addr, len)?;
        let values = values.iter().cloned().try_collect::<alloc::Vec<_>>()?;

        let mut s = String::try_with_capacity(size_hint)?;

        Formatter::format_with(&mut s, |f| {
            for value in values {
                value.display_fmt_with(f, &mut *self)?;
            }

            Ok::<_, VmError>(())
        })?;

        self.stack.store(out, s)?;
        Ok(())
    }

    /// Push a format specification onto the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_format(&mut self, addr: Address, spec: FormatSpec, out: Output) -> Result<(), VmError> {
        let value = self.stack.at(addr).clone();
        self.stack.store(out, || Format { value, spec })?;
        Ok(())
    }

    /// Perform the try operation on the given stack location.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_try(&mut self, addr: Address, out: Output) -> Result<Option<Output>, VmError> {
        let result = 'out: {
            let value = {
                let value = self.stack.at(addr);

                if let Repr::Any(value) = value.as_ref() {
                    match value.type_hash() {
                        Result::<Value, Value>::HASH => {
                            let result = value.borrow_ref::<Result<Value, Value>>()?;
                            break 'out result::result_try(&result)?;
                        }
                        Option::<Value>::HASH => {
                            let option = value.borrow_ref::<Option<Value>>()?;
                            break 'out option::option_try(&option)?;
                        }
                        _ => {}
                    }
                }

                value.clone()
            };

            match self.try_call_protocol_fn(&Protocol::TRY, value, &mut ())? {
                CallResultOnly::Ok(value) => ControlFlow::from_value(value)?,
                CallResultOnly::Unsupported(target) => {
                    return Err(VmError::new(VmErrorKind::UnsupportedTryOperand {
                        actual: target.type_info(),
                    }));
                }
            }
        };

        match result {
            ControlFlow::Continue(value) => {
                self.stack.store(out, value)?;
                Ok(None)
            }
            ControlFlow::Break(error) => Ok(self.op_return_internal(error)?),
        }
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_eq_character(&mut self, addr: Address, value: char, out: Output) -> Result<(), VmError> {
        let v = self.stack.at(addr);

        let is_match = match v.as_inline() {
            Some(Inline::Char(actual)) => *actual == value,
            _ => false,
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_eq_unsigned(&mut self, addr: Address, value: u64, out: Output) -> Result<(), VmError> {
        let v = self.stack.at(addr);

        let is_match = match v.as_inline() {
            Some(Inline::Unsigned(actual)) => *actual == value,
            _ => false,
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_eq_signed(&mut self, addr: Address, value: i64, out: Output) -> Result<(), VmError> {
        let is_match = match self.stack.at(addr).as_inline() {
            Some(Inline::Signed(actual)) => *actual == value,
            _ => false,
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_eq_bool(&mut self, addr: Address, value: bool, out: Output) -> Result<(), VmError> {
        let v = self.stack.at(addr);

        let is_match = match v.as_inline() {
            Some(Inline::Bool(actual)) => *actual == value,
            _ => false,
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    /// Test if the top of stack is equal to the string at the given static
    /// string slot.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_eq_string(&mut self, addr: Address, slot: usize, out: Output) -> Result<(), VmError> {
        let v = self.stack.at(addr);

        let is_match = 'out: {
            let Some(actual) = v.try_borrow_ref::<String>()? else {
                break 'out false;
            };

            let Some(string) = self.unit.lookup_string(slot) else {
                return Err(VmError::new(VmErrorKind::MissingStaticString { slot }));
            };

            actual.as_str() == string.as_str()
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    /// Test if the top of stack is equal to the string at the given static
    /// bytes slot.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_eq_bytes(&mut self, addr: Address, slot: usize, out: Output) -> Result<(), VmError> {
        let v = self.stack.at(addr);

        let is_match = 'out: {
            let Some(value) = v.try_borrow_ref::<Bytes>()? else {
                break 'out false;
            };

            let Some(bytes) = self.unit.lookup_bytes(slot) else {
                return Err(VmError::new(VmErrorKind::MissingStaticBytes { slot }));
            };

            value.as_slice() == bytes
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_match_type(
        &mut self,
        hash: Hash,
        variant_hash: Hash,
        addr: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let value = self.stack.at(addr);

        let type_hash = value.type_hash();

        let is_match = 'out: {
            if type_hash != hash {
                break 'out false;
            }

            // No variant to check.
            if variant_hash == Hash::EMPTY {
                break 'out true;
            }

            match value.as_ref() {
                Repr::Inline(Inline::Ordering(ordering)) => {
                    break 'out cmp::ordering_is_variant(*ordering, variant_hash);
                }
                Repr::Dynamic(value) => {
                    break 'out value.rtti().is(hash, variant_hash);
                }
                Repr::Any(any) => match hash {
                    Result::<Value, Value>::HASH => {
                        let result = any.borrow_ref::<Result<Value, Value>>()?;
                        break 'out result::is_variant(&result, variant_hash);
                    }
                    Option::<Value>::HASH => {
                        let option = any.borrow_ref::<Option<Value>>()?;
                        break 'out option::is_variant(&option, variant_hash);
                    }
                    _ => {}
                },
                _ => break 'out true,
            }

            let value = value.clone();

            match self.try_call_protocol_fn(
                &Protocol::IS_VARIANT,
                value,
                &mut Some((variant_hash,)),
            )? {
                CallResultOnly::Ok(value) => bool::from_value(value)?,
                CallResultOnly::Unsupported(..) => false,
            }
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_match_sequence(
        &mut self,
        hash: Hash,
        len: usize,
        exact: bool,
        addr: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let value = self.stack.at(addr);

        let type_hash = value.type_hash();

        let is_match = 'out: {
            if type_hash != hash {
                break 'out false;
            }

            let actual = match value.as_ref() {
                Repr::Inline(Inline::Unit) => 0,
                Repr::Any(any) => match type_hash {
                    runtime::Vec::HASH => any.borrow_ref::<runtime::Vec>()?.len(),
                    runtime::OwnedTuple::HASH => any.borrow_ref::<runtime::OwnedTuple>()?.len(),
                    _ => break 'out false,
                },
                _ => break 'out false,
            };

            actual >= len && (!exact || actual == len)
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_match_object(
        &mut self,
        slot: usize,
        exact: bool,
        addr: Address,
        out: Output,
    ) -> Result<(), VmError> {
        let is_match = 'is_match: {
            let Some(object) = self.stack.at(addr).try_borrow_ref::<Object>()? else {
                break 'is_match false;
            };

            let Some(keys) = self.unit.lookup_object_keys(slot) else {
                return Err(VmError::new(VmErrorKind::MissingStaticObjectKeys { slot }));
            };

            if object.len() < keys.len() || (exact && object.len() != keys.len()) {
                break 'is_match false;
            }

            for key in keys {
                if !object.contains_key(key.as_str()) {
                    break 'is_match false;
                }
            }

            true
        };

        self.stack.store(out, is_match)?;
        Ok(())
    }

    /// Load a function as a value onto the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_load_fn(&mut self, hash: Hash, out: Output) -> Result<(), VmError> {
        let function = self.lookup_function_by_hash(hash)?;
        self.stack.store(out, function)?;
        Ok(())
    }

    /// Construct a closure on the top of the stack.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_closure(
        &mut self,
        hash: Hash,
        addr: Address,
        count: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let Some(UnitFn::Offset {
            offset,
            call,
            args,
            captures: Some(captures),
        }) = self.unit.function(&hash)
        else {
            return Err(VmError::new(VmErrorKind::MissingFunction { hash }));
        };

        if *captures != count {
            return Err(VmError::new(VmErrorKind::BadEnvironmentCount {
                expected: *captures,
                actual: count,
            }));
        }

        let environment = self.stack.slice_at(addr, count)?;
        let environment = environment
            .iter()
            .cloned()
            .try_collect::<alloc::Vec<Value>>()?;
        let environment = environment.try_into_boxed_slice()?;

        let function = Function::from_vm_closure(
            self.context.clone(),
            self.unit.clone(),
            *offset,
            *call,
            *args,
            environment,
            hash,
        );

        self.stack.store(out, function)?;
        Ok(())
    }

    /// Implementation of a function call.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_call(
        &mut self,
        hash: Hash,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let Some(info) = self.unit.function(&hash) else {
            let Some(handler) = self.context.function(&hash) else {
                return Err(VmError::new(VmErrorKind::MissingFunction { hash }));
            };

            handler.call(&mut self.stack, addr, args, out)?;
            return Ok(());
        };

        match info {
            UnitFn::Offset {
                offset,
                call,
                args: expected,
                ..
            } => {
                check_args(args, *expected)?;
                self.call_offset_fn(*offset, *call, addr, args, Isolated::None, out)?;
            }
            UnitFn::EmptyStruct { hash } => {
                check_args(args, 0)?;

                let Some(rtti) = self.unit.lookup_rtti(hash) else {
                    return Err(VmError::new(VmErrorKind::MissingRtti { hash: *hash }));
                };

                self.stack
                    .store(out, || Value::empty_struct(rtti.clone()))?;
            }
            UnitFn::TupleStruct {
                hash,
                args: expected,
            } => {
                check_args(args, *expected)?;

                let Some(rtti) = self.unit.lookup_rtti(hash) else {
                    return Err(VmError::new(VmErrorKind::MissingRtti { hash: *hash }));
                };

                let tuple = self.stack.slice_at_mut(addr, args)?;
                let data = tuple.iter_mut().map(take);
                let value = AnySequence::new(rtti.clone(), data)?;
                self.stack.store(out, value)?;
            }
        }

        Ok(())
    }

    /// Call a function at the given offset with the given number of arguments.
    #[cfg_attr(feature = "bench", inline(never))]
    fn op_call_offset(
        &mut self,
        offset: usize,
        call: Call,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<(), VmError> {
        self.call_offset_fn(offset, call, addr, args, Isolated::None, out)?;
        Ok(())
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_call_associated(
        &mut self,
        hash: Hash,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<(), VmError> {
        let instance = self.stack.at(addr);
        let type_hash = instance.type_hash();
        let hash = Hash::associated_function(type_hash, hash);

        if let Some(handler) = self.context.function(&hash) {
            self.called_function_hook(hash)?;
            handler.call(&mut self.stack, addr, args, out)?;
            return Ok(());
        }

        if let Some(UnitFn::Offset {
            offset,
            call,
            args: expected,
            ..
        }) = self.unit.function(&hash)
        {
            self.called_function_hook(hash)?;
            check_args(args, *expected)?;
            self.call_offset_fn(*offset, *call, addr, args, Isolated::None, out)?;
            return Ok(());
        }

        Err(VmError::new(VmErrorKind::MissingInstanceFunction {
            instance: instance.type_info(),
            hash,
        }))
    }

    #[cfg_attr(feature = "bench", inline(never))]
    #[tracing::instrument(skip(self))]
    fn op_call_fn(
        &mut self,
        function: Address,
        addr: Address,
        args: usize,
        out: Output,
    ) -> Result<Option<VmHalt>, VmError> {
        let function = self.stack.at(function);

        match function.as_ref() {
            Repr::Inline(Inline::Type(ty)) => {
                self.op_call(ty.into_hash(), addr, args, out)?;
                Ok(None)
            }
            Repr::Any(value) if value.type_hash() == Function::HASH => {
                let value = value.clone();
                let f = value.borrow_ref::<Function>()?;
                f.call_with_vm(self, addr, args, out)
            }
            value => Err(VmError::new(VmErrorKind::UnsupportedCallFn {
                actual: value.type_info(),
            })),
        }
    }

    #[cfg_attr(feature = "bench", inline(never))]
    fn op_iter_next(&mut self, addr: Address, jump: usize, out: Output) -> Result<(), VmError> {
        let value = self.stack.at(addr);

        let some = match value.as_ref() {
            Repr::Any(value) => match value.type_hash() {
                Option::<Value>::HASH => {
                    let option = value.borrow_ref::<Option<Value>>()?;

                    let Some(some) = &*option else {
                        self.ip = self.unit.translate(jump)?;
                        return Ok(());
                    };

                    some.clone()
                }
                _ => {
                    return Err(VmError::new(VmErrorKind::UnsupportedIterNextOperand {
                        actual: value.type_info(),
                    }));
                }
            },
            actual => {
                return Err(VmError::new(VmErrorKind::UnsupportedIterNextOperand {
                    actual: actual.type_info(),
                }));
            }
        };

        self.stack.store(out, some)?;
        Ok(())
    }

    /// Call the provided closure within the context of this virtual machine.
    ///
    /// This allows for calling protocol function helpers like
    /// [Value::display_fmt] which requires access to a virtual machine.
    ///
    /// ```no_run
    /// use rune::{Value, Vm};
    /// use rune::runtime::{Formatter, VmError};
    ///
    /// fn use_with(vm: &Vm, output: &Value, f: &mut Formatter) -> Result<(), VmError> {
    ///     vm.with(|| output.display_fmt(f))?;
    ///     Ok(())
    /// }
    /// ```
    pub fn with<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _guard = runtime::env::Guard::new(self.context.clone(), self.unit.clone(), None);
        f()
    }

    /// Evaluate a single instruction.
    pub(crate) fn run(
        &mut self,
        diagnostics: Option<&mut dyn VmDiagnostics>,
    ) -> Result<VmHalt, VmError> {
        let mut vm_diagnostics_obj;

        let diagnostics = match diagnostics {
            Some(diagnostics) => {
                vm_diagnostics_obj = VmDiagnosticsObj::new(diagnostics);
                Some(NonNull::from(&mut vm_diagnostics_obj))
            }
            None => None,
        };

        // NB: set up environment so that native function can access context and
        // unit.
        let _guard = runtime::env::Guard::new(self.context.clone(), self.unit.clone(), diagnostics);

        let mut budget = budget::acquire();

        loop {
            if !budget.take() {
                return Ok(VmHalt::Limited);
            }

            let Some((inst, inst_len)) = self.unit.instruction_at(self.ip)? else {
                return Err(VmError::new(VmErrorKind::IpOutOfBounds {
                    ip: self.ip,
                    length: self.unit.instructions().end(),
                }));
            };

            tracing::trace!(ip = ?self.ip, ?inst);

            self.ip = self.ip.wrapping_add(inst_len);
            self.last_ip_len = inst_len as u8;

            match inst.kind {
                inst::Kind::Allocate { size } => {
                    self.op_allocate(size)?;
                }
                inst::Kind::Not { addr, out } => {
                    self.op_not(addr, out)?;
                }
                inst::Kind::Neg { addr, out } => {
                    self.op_neg(addr, out)?;
                }
                inst::Kind::Closure {
                    hash,
                    addr,
                    count,
                    out,
                } => {
                    self.op_closure(hash, addr, count, out)?;
                }
                inst::Kind::Call {
                    hash,
                    addr,
                    args,
                    out,
                } => {
                    self.op_call(hash, addr, args, out)?;
                }
                inst::Kind::CallOffset {
                    offset,
                    call,
                    addr,
                    args,
                    out,
                } => {
                    self.op_call_offset(offset, call, addr, args, out)?;
                }
                inst::Kind::CallAssociated {
                    hash,
                    addr,
                    args,
                    out,
                } => {
                    self.op_call_associated(hash, addr, args, out)?;
                }
                inst::Kind::CallFn {
                    function,
                    addr,
                    args,
                    out,
                } => {
                    if let Some(reason) = self.op_call_fn(function, addr, args, out)? {
                        return Ok(reason);
                    }
                }
                inst::Kind::LoadInstanceFn { addr, hash, out } => {
                    self.op_load_instance_fn(addr, hash, out)?;
                }
                inst::Kind::IndexGet { target, index, out } => {
                    self.op_index_get(target, index, out)?;
                }
                inst::Kind::TupleIndexSet {
                    target,
                    index,
                    value,
                } => {
                    self.op_tuple_index_set(target, index, value)?;
                }
                inst::Kind::TupleIndexGetAt { addr, index, out } => {
                    self.op_tuple_index_get_at(addr, index, out)?;
                }
                inst::Kind::ObjectIndexSet {
                    target,
                    slot,
                    value,
                } => {
                    self.op_object_index_set(target, slot, value)?;
                }
                inst::Kind::ObjectIndexGetAt { addr, slot, out } => {
                    self.op_object_index_get_at(addr, slot, out)?;
                }
                inst::Kind::IndexSet {
                    target,
                    index,
                    value,
                } => {
                    self.op_index_set(target, index, value)?;
                }
                inst::Kind::Return { addr } => {
                    if let Some(out) = self.op_return(addr)? {
                        return Ok(VmHalt::Exited(out.as_addr()));
                    }
                }
                inst::Kind::ReturnUnit => {
                    if let Some(out) = self.op_return_unit()? {
                        return Ok(VmHalt::Exited(out.as_addr()));
                    }
                }
                inst::Kind::Await { addr, out } => {
                    let future = self.op_await(addr)?;
                    return Ok(VmHalt::Awaited(Awaited::Future(future, out)));
                }
                inst::Kind::Select { addr, len, value } => {
                    if let Some(select) = self.op_select(addr, len, value)? {
                        return Ok(VmHalt::Awaited(Awaited::Select(select, value)));
                    }
                }
                inst::Kind::LoadFn { hash, out } => {
                    self.op_load_fn(hash, out)?;
                }
                inst::Kind::Store { value, out } => {
                    self.op_store(value, out)?;
                }
                inst::Kind::Copy { addr, out } => {
                    self.op_copy(addr, out)?;
                }
                inst::Kind::Move { addr, out } => {
                    self.op_move(addr, out)?;
                }
                inst::Kind::Drop { set } => {
                    self.op_drop(set)?;
                }
                inst::Kind::Swap { a, b } => {
                    self.op_swap(a, b)?;
                }
                inst::Kind::Jump { jump } => {
                    self.op_jump(jump)?;
                }
                inst::Kind::JumpIf { cond, jump } => {
                    self.op_jump_if(cond, jump)?;
                }
                inst::Kind::JumpIfNot { cond, jump } => {
                    self.op_jump_if_not(cond, jump)?;
                }
                inst::Kind::Vec { addr, count, out } => {
                    self.op_vec(addr, count, out)?;
                }
                inst::Kind::Tuple { addr, count, out } => {
                    self.op_tuple(addr, count, out)?;
                }
                inst::Kind::Tuple1 { addr, out } => {
                    self.op_tuple_n(&addr[..], out)?;
                }
                inst::Kind::Tuple2 { addr, out } => {
                    self.op_tuple_n(&addr[..], out)?;
                }
                inst::Kind::Tuple3 { addr, out } => {
                    self.op_tuple_n(&addr[..], out)?;
                }
                inst::Kind::Tuple4 { addr, out } => {
                    self.op_tuple_n(&addr[..], out)?;
                }
                inst::Kind::Environment { addr, count, out } => {
                    self.op_environment(addr, count, out)?;
                }
                inst::Kind::Object { addr, slot, out } => {
                    self.op_object(addr, slot, out)?;
                }
                inst::Kind::Range { range, out } => {
                    self.op_range(range, out)?;
                }
                inst::Kind::Struct { addr, hash, out } => {
                    self.op_struct(addr, hash, out)?;
                }
                inst::Kind::ConstConstruct {
                    addr,
                    hash,
                    count,
                    out,
                } => {
                    self.op_const_construct(addr, hash, count, out)?;
                }
                inst::Kind::String { slot, out } => {
                    self.op_string(slot, out)?;
                }
                inst::Kind::Bytes { slot, out } => {
                    self.op_bytes(slot, out)?;
                }
                inst::Kind::StringConcat {
                    addr,
                    len,
                    size_hint,
                    out,
                } => {
                    self.op_string_concat(addr, len, size_hint, out)?;
                }
                inst::Kind::Format { addr, spec, out } => {
                    self.op_format(addr, spec, out)?;
                }
                inst::Kind::Try { addr, out } => {
                    if let Some(out) = self.op_try(addr, out)? {
                        return Ok(VmHalt::Exited(out.as_addr()));
                    }
                }
                inst::Kind::EqChar { addr, value, out } => {
                    self.op_eq_character(addr, value, out)?;
                }
                inst::Kind::EqUnsigned { addr, value, out } => {
                    self.op_eq_unsigned(addr, value, out)?;
                }
                inst::Kind::EqSigned { addr, value, out } => {
                    self.op_eq_signed(addr, value, out)?;
                }
                inst::Kind::EqBool {
                    addr,
                    value: boolean,
                    out,
                } => {
                    self.op_eq_bool(addr, boolean, out)?;
                }
                inst::Kind::EqString { addr, slot, out } => {
                    self.op_eq_string(addr, slot, out)?;
                }
                inst::Kind::EqBytes { addr, slot, out } => {
                    self.op_eq_bytes(addr, slot, out)?;
                }
                inst::Kind::MatchType {
                    hash,
                    variant_hash,
                    addr,
                    out,
                } => {
                    self.op_match_type(hash, variant_hash, addr, out)?;
                }
                inst::Kind::MatchSequence {
                    hash,
                    len,
                    exact,
                    addr,
                    out,
                } => {
                    self.op_match_sequence(hash, len, exact, addr, out)?;
                }
                inst::Kind::MatchObject {
                    slot,
                    exact,
                    addr,
                    out,
                } => {
                    self.op_match_object(slot, exact, addr, out)?;
                }
                inst::Kind::Yield { addr, out } => {
                    return Ok(VmHalt::Yielded(Some(addr), out));
                }
                inst::Kind::YieldUnit { out } => {
                    return Ok(VmHalt::Yielded(None, out));
                }
                inst::Kind::Op { op, a, b, out } => {
                    self.op_op(op, a, b, out)?;
                }
                inst::Kind::Arithmetic { op, a, b, out } => {
                    self.op_arithmetic(op, a, b, out)?;
                }
                inst::Kind::Bitwise { op, a, b, out } => {
                    self.op_bitwise(op, a, b, out)?;
                }
                inst::Kind::Shift { op, a, b, out } => {
                    self.op_shift(op, a, b, out)?;
                }
                inst::Kind::AssignArithmetic { op, target, rhs } => {
                    self.op_assign_arithmetic(op, target, rhs)?;
                }
                inst::Kind::AssignBitwise { op, target, rhs } => {
                    self.op_assign_bitwise(op, target, rhs)?;
                }
                inst::Kind::AssignShift { op, target, rhs } => {
                    self.op_assign_shift(op, target, rhs)?;
                }
                inst::Kind::IterNext { addr, jump, out } => {
                    self.op_iter_next(addr, jump, out)?;
                }
                inst::Kind::Panic { reason } => {
                    return Err(VmError::new(VmErrorKind::Panic {
                        reason: Panic::from(reason),
                    }));
                }
            }
        }
    }
}

impl TryClone for Vm {
    fn try_clone(&self) -> alloc::Result<Self> {
        Ok(Self {
            context: self.context.clone(),
            unit: self.unit.clone(),
            ip: self.ip,
            last_ip_len: self.last_ip_len,
            stack: self.stack.try_clone()?,
            call_frames: self.call_frames.try_clone()?,
        })
    }
}

impl AsMut<Vm> for Vm {
    #[inline]
    fn as_mut(&mut self) -> &mut Vm {
        self
    }
}

impl AsRef<Vm> for Vm {
    #[inline]
    fn as_ref(&self) -> &Vm {
        self
    }
}

/// A call frame.
///
/// This is used to store the return point after an instruction has been run.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct CallFrame {
    /// The stored instruction pointer.
    pub ip: usize,
    /// The top of the stack at the time of the call to ensure stack isolation
    /// across function calls.
    ///
    /// I.e. a function should not be able to manipulate the size of any other
    /// stack than its own.
    pub top: usize,
    /// Indicates that the call frame is isolated and should force an exit into
    /// the vm execution context.
    pub isolated: Isolated,
    /// Keep the value produced from the call frame.
    pub out: Output,
}

impl TryClone for CallFrame {
    #[inline]
    fn try_clone(&self) -> alloc::Result<Self> {
        Ok(*self)
    }
}

/// Clear stack on drop.
struct ClearStack<'a>(&'a mut Vm);

impl Drop for ClearStack<'_> {
    fn drop(&mut self) {
        self.0.stack.clear();
    }
}

/// Check that arguments matches expected or raise the appropriate error.
#[inline(always)]
fn check_args(args: usize, expected: usize) -> Result<(), VmErrorKind> {
    if args != expected {
        return Err(VmErrorKind::BadArgumentCount {
            actual: args,
            expected,
        });
    }

    Ok(())
}

enum TargetFallback {
    Value(Value, Value),
    Field(Value, Hash, usize, Value),
    Index(Value, usize, Value),
}

enum TargetValue<'a> {
    /// Resolved internal target to mutable value.
    Same(&'a mut Value),
    /// Resolved internal target to mutable value.
    Pair(BorrowMut<'a, Value>, &'a Value),
    /// Fallback to a different kind of operation.
    Fallback(TargetFallback),
}

#[inline]
fn target_value<'a>(
    stack: &'a mut Stack,
    unit: &Unit,
    target: InstTarget,
    rhs: Address,
) -> Result<TargetValue<'a>, VmErrorKind> {
    match target {
        InstTarget::Address(addr) => match stack.pair(addr, rhs)? {
            Pair::Same(value) => Ok(TargetValue::Same(value)),
            Pair::Pair(lhs, rhs) => Ok(TargetValue::Pair(BorrowMut::from_ref(lhs), rhs)),
        },
        InstTarget::TupleField(lhs, index) => {
            let lhs = stack.at(lhs);
            let rhs = stack.at(rhs);

            if let Some(value) = try_tuple_like_index_get_mut(lhs, index)? {
                Ok(TargetValue::Pair(value, rhs))
            } else {
                Ok(TargetValue::Fallback(TargetFallback::Index(
                    lhs.clone(),
                    index,
                    rhs.clone(),
                )))
            }
        }
        InstTarget::Field(lhs, slot) => {
            let rhs = stack.at(rhs);

            let Some(field) = unit.lookup_string(slot) else {
                return Err(VmErrorKind::MissingStaticString { slot });
            };

            let lhs = stack.at(lhs);

            if let Some(value) = try_object_like_index_get_mut(lhs, field)? {
                Ok(TargetValue::Pair(value, rhs))
            } else {
                Ok(TargetValue::Fallback(TargetFallback::Field(
                    lhs.clone(),
                    field.hash(),
                    slot,
                    rhs.clone(),
                )))
            }
        }
    }
}

/// Implementation of getting a mutable value out of a tuple-like value.
fn try_tuple_like_index_get_mut(
    target: &Value,
    index: usize,
) -> Result<Option<BorrowMut<'_, Value>>, VmErrorKind> {
    match target.as_ref() {
        Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Tuple) => {
            let Some(value) = data.get_mut(index)? else {
                return Err(VmErrorKind::MissingIndexInteger {
                    target: data.type_info(),
                    index: VmIntegerRepr::from(index),
                });
            };

            Ok(Some(value))
        }
        Repr::Dynamic(data) => Err(VmErrorKind::MissingIndexInteger {
            target: data.type_info(),
            index: VmIntegerRepr::from(index),
        }),
        Repr::Any(value) => match value.type_hash() {
            Result::<Value, Value>::HASH => {
                let result = BorrowMut::try_map(
                    value.borrow_mut::<Result<Value, Value>>()?,
                    |value| match (index, value) {
                        (0, Ok(value)) => Some(value),
                        (0, Err(value)) => Some(value),
                        _ => None,
                    },
                );

                if let Ok(value) = result {
                    return Ok(Some(value));
                }

                Err(VmErrorKind::MissingIndexInteger {
                    target: TypeInfo::any::<Result<Value, Value>>(),
                    index: VmIntegerRepr::from(index),
                })
            }
            Option::<Value>::HASH => {
                let result =
                    BorrowMut::try_map(value.borrow_mut::<Option<Value>>()?, |value| {
                        match (index, value) {
                            (0, Some(value)) => Some(value),
                            _ => None,
                        }
                    });

                if let Ok(value) = result {
                    return Ok(Some(value));
                }

                Err(VmErrorKind::MissingIndexInteger {
                    target: TypeInfo::any::<Option<Value>>(),
                    index: VmIntegerRepr::from(index),
                })
            }
            GeneratorState::HASH => {
                let result = BorrowMut::try_map(value.borrow_mut::<GeneratorState>()?, |value| {
                    match (index, value) {
                        (0, GeneratorState::Yielded(value)) => Some(value),
                        (0, GeneratorState::Complete(value)) => Some(value),
                        _ => None,
                    }
                });

                if let Ok(value) = result {
                    return Ok(Some(value));
                }

                Err(VmErrorKind::MissingIndexInteger {
                    target: TypeInfo::any::<GeneratorState>(),
                    index: VmIntegerRepr::from(index),
                })
            }
            runtime::Vec::HASH => {
                let vec = value.borrow_mut::<runtime::Vec>()?;
                let result = BorrowMut::try_map(vec, |vec| vec.get_mut(index));

                if let Ok(value) = result {
                    return Ok(Some(value));
                }

                Err(VmErrorKind::MissingIndexInteger {
                    target: TypeInfo::any::<runtime::Vec>(),
                    index: VmIntegerRepr::from(index),
                })
            }
            runtime::OwnedTuple::HASH => {
                let tuple = value.borrow_mut::<runtime::OwnedTuple>()?;
                let result = BorrowMut::try_map(tuple, |tuple| tuple.get_mut(index));

                if let Ok(value) = result {
                    return Ok(Some(value));
                }

                Err(VmErrorKind::MissingIndexInteger {
                    target: TypeInfo::any::<runtime::OwnedTuple>(),
                    index: VmIntegerRepr::from(index),
                })
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Implementation of getting a mutable string index on an object-like type.
fn try_object_like_index_get_mut<'a>(
    target: &'a Value,
    field: &str,
) -> Result<Option<BorrowMut<'a, Value>>, VmErrorKind> {
    match target.as_ref() {
        Repr::Inline(value) => Err(VmErrorKind::MissingField {
            target: value.type_info(),
            field: field.try_to_owned()?,
        }),
        Repr::Dynamic(data) if matches!(data.rtti().kind, RttiKind::Struct) => {
            Ok(data.get_field_mut(field)?)
        }
        Repr::Dynamic(data) => Err(VmErrorKind::MissingField {
            target: data.type_info(),
            field: field.try_to_owned()?,
        }),
        Repr::Any(value) => match value.type_hash() {
            Object::HASH => {
                let object = value.borrow_mut::<Object>()?;

                let Ok(value) = BorrowMut::try_map(object, |object| object.get_mut(field)) else {
                    return Err(VmErrorKind::MissingField {
                        target: value.type_info(),
                        field: field.try_to_owned()?,
                    });
                };

                Ok(Some(value))
            }
            _ => Ok(None),
        },
    }
}

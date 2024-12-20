use core::any;

use crate::compile::Named;
use crate::runtime::{AnyTypeInfo, TypeHash};

/// Macro to mark a value as external, which will implement all the appropriate
/// traits.
///
/// This is required to support the external type as a type argument in a
/// registered function.
///
/// ## `#[rune(item = <path>)]`
///
/// Specify the item prefix which contains this time.
///
/// This is required in order to calculate the correct type hash, if this is
/// omitted and the item is defined in a nested module the type hash won't match
/// the expected path hash.
///
/// ```
/// use rune::Any;
///
/// #[derive(Any)]
/// #[rune(item = ::process)]
/// struct Process {
///     /* .. */
/// }
///
/// fn install() -> Result<rune::Module, rune::ContextError> {
///     let mut module = rune::Module::with_crate("process")?;
///     module.ty::<Process>()?;
///     Ok(module)
/// }
/// ```
///
/// ## `#[rune(name = <ident>)]` attribute
///
/// The name of a type defaults to its identifiers, so `struct Foo {}` would be
/// given the name `Foo`.
///
/// This can be overrided with the `#[rune(name = <ident>)]` attribute:
///
/// ```
/// use rune::Any;
///
/// #[derive(Any)]
/// #[rune(name = Bar)]
/// struct Foo {
/// }
///
/// fn install() -> Result<rune::Module, rune::ContextError> {
///     let mut module = rune::Module::new();
///     module.ty::<Foo>()?;
///     Ok(module)
/// }
/// ```
pub use rune_macros::Any;

/// Derive for types which can be used inside of Rune.
///
/// Rune only supports two types, *built-in* types [`String`] and *external*
/// types which derive `Any`. Before they can be used they must be registered in
/// [`Context::install`] through a [`Module`].
///
/// This is typically used in combination with declarative macros to register
/// functions and macros, such as:
///
/// * [`#[rune::function]`]
/// * [`#[rune::macro_]`]
///
/// [`AnyObj`]: crate::runtime::AnyObj
/// [`Context::install`]: crate::Context::install
/// [`Module`]: crate::Module
/// [`String`]: std::string::String
/// [`#[rune::function]`]: macro@crate::function
/// [`#[rune::macro_]`]: crate::macro_
///
/// # Examples
///
/// ```
/// use rune::Any;
///
/// #[derive(Any)]
/// struct Npc {
///     #[rune(get)]
///     health: u32,
/// }
///
/// impl Npc {
///     /// Construct a new NPC.
///     #[rune::function(path = Self::new)]
///     fn new(health: u32) -> Self {
///         Self {
///             health
///         }
///     }
///
///     /// Damage the NPC with the given `amount`.
///     #[rune::function]
///     fn damage(&mut self, amount: u32) {
///         self.health -= amount;
///     }
/// }
///
/// fn install() -> Result<rune::Module, rune::ContextError> {
///     let mut module = rune::Module::new();
///     module.ty::<Npc>()?;
///     module.function_meta(Npc::new)?;
///     module.function_meta(Npc::damage)?;
///     Ok(module)
/// }
/// ```
pub trait Any: TypeHash + Named + any::Any {
    /// The compile-time type information know for the type.
    const ANY_TYPE_INFO: AnyTypeInfo = AnyTypeInfo::new(Self::full_name, Self::HASH);
}

/// Trait implemented for types which can be automatically converted to a
/// [`Value`].
///
/// We can't use a blanked implementation over `T: Any` because it only governs
/// what can be stored in any [`AnyObj`].
///
/// This trait in contrast is selectively implemented for types which we want to
/// generate [`ToValue`] and [`FromValue`] implementations for.
///
/// [`Value`]: crate::runtime::Value
/// [`AnyObj`]: crate::runtime::AnyObj
/// [`ToValue`]: crate::runtime::ToValue
/// [`FromValue`]: crate::runtime::FromValue
///
/// Note that you are *not* supposed to implement this directly. Make use of the
/// [`Any`] derive instead.
///
/// [`Any`]: derive@Any
pub trait AnyMarker: Any {}

//! This example showcases overloading the multiplication protocol for a
//! specific type `Foo`.

use rune::runtime::Protocol;
use rune::sync::Arc;
use rune::{Any, ContextError, Diagnostics, Module, Vm};

#[derive(Debug, Default, Any)]
struct Foo {
    field: u32,
}

impl Foo {
    fn mul(self, b: u32) -> Self {
        Self {
            field: self.field * b,
        }
    }
}

fn main() -> rune::support::Result<()> {
    let m = module()?;

    let mut context = rune_modules::default_context()?;
    context.install(m)?;

    let runtime = Arc::try_new(context.runtime()?)?;

    let mut sources = rune::sources! {
        entry => {
            pub fn main(foo) {
                foo * 5
            }
        }
    };

    let mut diagnostics = Diagnostics::new();

    let result = rune::prepare(&mut sources)
        .with_context(&context)
        .with_diagnostics(&mut diagnostics)
        .build();

    let unit = result?;
    let unit = Arc::try_new(unit)?;
    let mut vm = Vm::new(runtime, unit);

    let output = vm.call(["main"], (Foo { field: 5 },))?;
    let output: Foo = rune::from_value(output)?;

    println!("output: {output:?}");
    Ok(())
}

fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_item(["module"])?;
    m.ty::<Foo>()?;
    m.associated_function(&Protocol::MUL, Foo::mul)?;
    Ok(m)
}

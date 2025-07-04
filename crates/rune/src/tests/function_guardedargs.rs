prelude!();

#[derive(Any)]
struct MyAny;

fn get_vm() -> crate::support::Result<Vm> {
    let mut sources = crate::sources! {
        entry => {
            enum Enum {
                Variant(internal)
            }

            struct Tuple(internal);

            pub fn function(argument) {}
        }
    };

    let unit = crate::prepare(&mut sources).build()?;
    let unit = Arc::try_new(unit)?;
    Ok(Vm::without_runtime(unit)?)
}

#[test]
fn references_allowed_for_function_calls() {
    let vm = get_vm().unwrap();
    let function = vm.lookup_function(["function"]).unwrap();

    let value_result = function.call::<crate::Value>((crate::Value::unit(),));
    assert!(value_result.is_ok());

    let mut mine = MyAny;

    let ref_result = function.call::<crate::Value>((&mine,));
    assert!(ref_result.is_ok());

    let mut_result = function.call::<crate::Value>((&mut mine,));
    assert!(mut_result.is_ok());

    let any_result = function.call::<crate::Value>((mine,));
    assert!(any_result.is_ok());
}

#[test]
fn references_disallowed_for_tuple_variant() {
    let vm = get_vm().unwrap();
    let constructor = vm.lookup_function(["Enum", "Variant"]).unwrap();

    let value_result = constructor.call::<crate::Value>((crate::Value::unit(),));
    assert!(value_result.is_ok());

    let mut mine = MyAny;

    let tuple = constructor.call::<DynamicTuple>((&mine,)).unwrap();
    assert!(tuple.first().unwrap().borrow_ref::<MyAny>().is_err());

    let tuple = constructor.call::<DynamicTuple>((&mut mine,)).unwrap();
    assert!(tuple.first().unwrap().borrow_ref::<MyAny>().is_err());

    let tuple = constructor.call::<DynamicTuple>((mine,)).unwrap();
    assert!(tuple.first().unwrap().borrow_ref::<MyAny>().is_ok());
}

#[test]
fn references_disallowed_for_tuple_struct() {
    let vm = get_vm().unwrap();
    let constructor = vm.lookup_function(["Tuple"]).unwrap();

    let value_result = constructor.call::<crate::Value>((crate::Value::unit(),));
    assert!(value_result.is_ok());

    let mut mine = MyAny;

    let st = constructor.call::<DynamicTuple>((&mine,)).unwrap();
    assert!(st.first().unwrap().borrow_ref::<MyAny>().is_err());

    let st = constructor.call::<DynamicTuple>((&mut mine,)).unwrap();
    assert!(st.first().unwrap().borrow_ref::<MyAny>().is_err());

    let st = constructor.call::<DynamicTuple>((mine,)).unwrap();
    assert!(st.first().unwrap().borrow_ref::<MyAny>().is_ok());
}

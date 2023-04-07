use std::io::Write;
use std::string::FromUtf8Error;
use std::sync::Arc;
use std::sync::Mutex;

use rune::runtime::{Stack, VmError, VmResult};
use rune::{ContextError, Module, Value};

#[derive(Default, Clone)]
pub struct CaptureIo {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl CaptureIo {
    /// Construct a new capture.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain all captured I/O that has been written to output functions.
    pub fn drain(&self) -> Vec<u8> {
        let mut o = self.inner.lock().unwrap();
        std::mem::take(&mut *o)
    }

    /// Drain all captured I/O that has been written to output functions and try
    /// to decode as UTF-8.
    pub fn drain_utf8(&self) -> Result<String, FromUtf8Error> {
        String::from_utf8(self.drain())
    }
}

/// Provide a bunch of `std` functions that can be used during tests to capture output.
pub fn module(io: &CaptureIo) -> Result<Module, ContextError> {
    let mut module = Module::with_crate_item("std", ["io"]);

    let o = io.clone();

    module.function(["print"], move |m: &str| {
        match write!(o.inner.lock().unwrap(), "{}", m) {
            Ok(()) => VmResult::Ok(()),
            Err(error) => VmResult::panic(error),
        }
    })?;

    let o = io.clone();

    module.function(["println"], move |m: &str| {
        match writeln!(o.inner.lock().unwrap(), "{}", m) {
            Ok(()) => VmResult::Ok(()),
            Err(error) => VmResult::panic(error),
        }
    })?;

    let o = io.clone();

    module.raw_fn(["dbg"], move |stack, args| {
        let mut o = o.inner.lock().unwrap();
        dbg_impl(&mut *o, stack, args)
    })?;

    Ok(module)
}

fn dbg_impl<O>(o: &mut O, stack: &mut Stack, args: usize) -> VmResult<()>
where
    O: Write,
{
    for value in rune::vm_try!(stack.drain(args)) {
        rune::vm_try!(writeln!(o, "{:?}", value).map_err(VmError::panic));
    }

    stack.push(Value::Unit);
    VmResult::Ok(())
}
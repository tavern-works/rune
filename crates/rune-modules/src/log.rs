use rune::ast;
use rune::compile;
use rune::macros::{quote, MacroContext, TokenStream};
use rune::parse::Parser;
use rune::{ContextError, Module};

#[rune::module(::log)]
pub fn module(_stdio: bool) -> Result<Module, ContextError> {
    let mut module = Module::from_meta(self::module_meta)?;
    module.function_meta(info_formatted)?;
    module.macro_meta(info)?;
    Ok(module)
}

#[rune::function]
fn info_formatted(formatted: &str) {
    log::info!("{formatted}");
}

#[rune::macro_(path = info)]
pub(crate) fn info(
    context: &mut MacroContext<'_, '_, '_>,
    stream: &TokenStream,
) -> compile::Result<TokenStream> {
    let mut parser = Parser::from_token_stream(stream, context.input_span());
    let mut output = quote!();
    while let Ok(arg) = parser.parse::<ast::Expr>() {
        if parser.parse::<ast::Comma>().is_ok() {
            output = quote!(#output #arg,);
        } else {
            output = quote!(#output #arg)
        }
    }
    parser.eof()?;
    let output = quote!(log::info_formatted(format!(
        "rune|{}:{}| {}",
        file!(),
        line!(),
        format!(#output)
    )));

    Ok(output.into_token_stream(context)?)
}

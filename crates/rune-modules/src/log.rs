use rune::ast;
use rune::compile;
use rune::macros::{quote, MacroContext, Quote, TokenStream};
use rune::parse::Parser;
use rune::{ContextError, Module};

#[rune::module(::log)]
pub fn module(_stdio: bool) -> Result<Module, ContextError> {
    let mut module = Module::from_meta(self::module__meta)?;
    module.function_meta(error_formatted)?;
    module.function_meta(info_formatted)?;
    module.function_meta(warn_formatted)?;
    module.macro_meta(error)?;
    module.macro_meta(info)?;
    module.macro_meta(warn)?;
    Ok(module)
}

#[rune::function]
fn error_formatted(formatted: &str) {
    log::error!("{formatted}");
}

#[rune::function]
fn info_formatted(formatted: &str) {
    log::info!("{formatted}");
}

#[rune::function]
fn warn_formatted(formatted: &str) {
    log::warn!("{formatted}");
}

fn quote_error(
    context: &mut MacroContext<'_, '_, '_>,
    formatted: Quote<'_>,
) -> compile::Result<TokenStream> {
    Ok(quote!(log::error_formatted(#formatted)).into_token_stream(context)?)
}

fn quote_info(
    context: &mut MacroContext<'_, '_, '_>,
    formatted: Quote<'_>,
) -> compile::Result<TokenStream> {
    Ok(quote!(log::info_formatted(#formatted)).into_token_stream(context)?)
}

fn quote_warn(
    context: &mut MacroContext<'_, '_, '_>,
    formatted: Quote<'_>,
) -> compile::Result<TokenStream> {
    Ok(quote!(log::warn_formatted(#formatted)).into_token_stream(context)?)
}

fn macro_common(
    context: &mut MacroContext<'_, '_, '_>,
    stream: &TokenStream,
    quoter: impl Fn(&mut MacroContext<'_, '_, '_>, Quote<'_>) -> compile::Result<TokenStream>,
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

    let output = quoter(
        context,
        quote!(format!(
            "rune|{}:{}| {}",
            file!(),
            line!(),
            format!(#output)
        )),
    )?;

    Ok(output)
}

#[rune::macro_(path = error)]
pub(crate) fn error(
    context: &mut MacroContext<'_, '_, '_>,
    stream: &TokenStream,
) -> compile::Result<TokenStream> {
    macro_common(context, stream, quote_error)
}

#[rune::macro_(path = info)]
pub(crate) fn info(
    context: &mut MacroContext<'_, '_, '_>,
    stream: &TokenStream,
) -> compile::Result<TokenStream> {
    macro_common(context, stream, quote_info)
}

#[rune::macro_(path = warn)]
pub(crate) fn warn(
    context: &mut MacroContext<'_, '_, '_>,
    stream: &TokenStream,
) -> compile::Result<TokenStream> {
    macro_common(context, stream, quote_warn)
}

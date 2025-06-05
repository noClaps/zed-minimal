use proc_macro::TokenStream;
use quote::quote;
use syn::{LitStr, parse_macro_input};

/// This macro replaces the path separator `/` with `\` for Windows.
/// But if the target OS is not Windows, the path is returned as is.
///
/// # Example
/// ```rust
/// # use util_macros::separator;
/// let path = separator!("path/to/file");
/// assert_eq!(path, "path/to/file");
/// ```
#[proc_macro]
pub fn separator(input: TokenStream) -> TokenStream {
    let path = parse_macro_input!(input as LitStr);
    let path = path.value();

    TokenStream::from(quote! {
        #path
    })
}

/// This macro replaces the path prefix `file:///` with `file:///C:/` for Windows.
/// But if the target OS is not Windows, the URI is returned as is.
///
/// # Example
/// ```rust
/// use util_macros::uri;
///
/// let uri = uri!("file:///path/to/file");
/// assert_eq!(uri, "file:///path/to/file");
/// ```
#[proc_macro]
pub fn uri(input: TokenStream) -> TokenStream {
    let uri = parse_macro_input!(input as LitStr);
    let uri = uri.value();

    TokenStream::from(quote! {
        #uri
    })
}

/// This macro replaces the line endings `\n` with `\r\n` for Windows.
/// But if the target OS is not Windows, the line endings are returned as is.
///
/// # Example
/// ```rust
/// use util_macros::line_endings;
///
/// let text = line_endings!("Hello\nWorld");
/// assert_eq!(text, "Hello\nWorld");
/// ```
#[proc_macro]
pub fn line_endings(input: TokenStream) -> TokenStream {
    let text = parse_macro_input!(input as LitStr);
    let text = text.value();

    TokenStream::from(quote! {
        #text
    })
}

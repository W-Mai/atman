//! Attribute macros for binding Rust functions as Atman tools.

mod common;
mod rt;
mod runtime;
mod value;

#[proc_macro_attribute]
pub fn rt_tools(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    rt::expand(attr, item)
}

#[proc_macro_attribute]
pub fn runtime_tools(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    runtime::expand(attr, item)
}

#[proc_macro_attribute]
pub fn value(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    value::expand(attr, item)
}

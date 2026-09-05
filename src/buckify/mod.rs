mod actions;
pub mod cross;
mod deps;
mod emit;
mod rules;
mod windows;

pub use rules::{
    CARGO_MANIFEST_SYMBOL, WRAPPER_SYMBOLS, buckify_dep_node, buckify_root_node, gen_buck_content,
    gen_buck_content_with_loads, render_rule, vendor_package,
};

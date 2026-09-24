fn main() {
    // Element debug info lets UI tests find elements (by accessible label, id…).
    // Only for dev/test builds; release binaries don't carry it.
    let debug = std::env::var("PROFILE").as_deref() == Ok("debug");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile Slint UI");
}

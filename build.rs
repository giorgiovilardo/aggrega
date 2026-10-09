fn main() {
    // Element debug info lets UI tests find elements (by accessible label, id…).
    // Only for dev/test builds; release binaries don't carry it.
    let debug = std::env::var("PROFILE").as_deref() == Ok("debug");
    // `@tr()` strings are translated from translations/<lang>/LC_MESSAGES/aggrega.po,
    // bundled into the binary. Entries carry no msgctxt.
    let config = slint_build::CompilerConfiguration::new()
        .with_debug_info(debug)
        .with_bundled_translations("translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile Slint UI");
}

fn main() {
    println!("cargo:rerun-if-changed=i18n");
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("i18n")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile dogi Slint UI");
}

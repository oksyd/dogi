fn main() {
    println!("cargo:rerun-if-env-changed=DOGI_DISTRIBUTION_BUILD");
    let channel = if std::env::var_os("DOGI_DISTRIBUTION_BUILD").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        "stable"
    } else {
        "development"
    };
    println!("cargo:rustc-env=DOGI_BUILD_CHANNEL={channel}");

    println!("cargo:rerun-if-changed=src/ui/i18n");
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("src/ui/i18n")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("src/ui/views/app.slint", config)
        .expect("failed to compile dogi Slint UI");
}

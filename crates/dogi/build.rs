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
}

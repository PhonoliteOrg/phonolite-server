fn main() {
    if std::env::var_os("CARGO_FEATURE_FFI_OPUS").is_none() {
        return;
    }

    let source = std::path::Path::new("../../vendor/opus");
    assert!(
        source.join("CMakeLists.txt").is_file(),
        "Opus sources are missing; run git submodule update --init --recursive"
    );
    println!("cargo:rerun-if-changed={}", source.display());

    let mut config = cmake::Config::new(source);
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // Opus selects the debug CRT for CMake's Debug configuration, whereas
        // Rust uses the non-debug CRT even in dev/test builds. Keep native
        // debug symbols without importing debug-runtime-only functions.
        if config.get_profile() == "Debug" {
            config.profile("RelWithDebInfo").cflag("/Z7").cflag("/Od");
        }
        let static_crt = std::env::var("CARGO_CFG_TARGET_FEATURE")
            .unwrap_or_default()
            .split(',')
            .any(|feature| feature == "crt-static");
        config
            .static_crt(static_crt)
            .define("OPUS_STATIC_RUNTIME", if static_crt { "ON" } else { "OFF" });
    }

    // Upstream selects compatible floating-point/SIMD sources and detects the
    // platform headers and CPUID backend. Recursively compiling all C files mixes
    // mutually exclusive implementations and includes standalone programs.
    let destination = config
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_FIXED_POINT", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "OFF")
        .define("OPUS_INSTALL_CMAKE_CONFIG_MODULE", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        .build();

    println!(
        "cargo:rustc-link-search=native={}",
        destination.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=opus");
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("unix") {
        println!("cargo:rustc-link-lib=m");
    }
}

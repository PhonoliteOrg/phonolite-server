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

    // Upstream selects compatible floating-point/SIMD sources and detects the
    // platform headers and CPUID backend. Recursively compiling all C files mixes
    // mutually exclusive implementations and includes standalone programs.
    let destination = cmake::Config::new(source)
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

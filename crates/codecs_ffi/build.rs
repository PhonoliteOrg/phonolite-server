fn main() {
    if std::env::var("CARGO_FEATURE_FFI_OPUS").is_ok() {
        println!("cargo:rerun-if-changed=../../vendor/opus");
        let mut build = cc::Build::new();
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            build.out_dir(out_dir);
        }
        build
            .include("../../vendor/opus")
            .include("../../vendor/opus/include")
            .include("../../vendor/opus/celt")
            .include("../../vendor/opus/silk")
            .include("../../vendor/opus/silk/float")
            .define("OPUS_BUILD", None)
            .define("HAVE_LRINTF", None)
            .define("HAVE_LRINT", None)
            .define("USE_ALLOCA", None)
            .flag_if_supported("-std=c99");

        let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let allow_x86 = matches!(target_arch.as_str(), "x86" | "x86_64");
        if allow_x86 {
            // Enable Opus runtime CPU detection for x86 SIMD paths.
            build
                .define("OPUS_HAVE_RTCD", None)
                .define("OPUS_X86_MAY_HAVE_SSE", None)
                .define("OPUS_X86_MAY_HAVE_SSE2", None)
                .flag_if_supported("-msse")
                .flag_if_supported("-msse2");
        }

        let root = std::path::Path::new("../../vendor/opus");
        let mut files = Vec::new();
        let skip_arm = std::env::var("CARGO_CFG_TARGET_ARCH")
            .map(|arch| arch != "arm" && arch != "aarch64")
            .unwrap_or(true);
        collect_c_files(root, &mut files, skip_arm, allow_x86);
        if files.is_empty() {
            println!("cargo:warning=ffi-opus enabled but no libopus C sources found under vendor/opus");
            build.file("src/opus_stub.c");
            build.compile("rustyfin_opus_stub");
            return;
        }
        for file in files {
            build.file(file);
        }
        build.compile("rustyfin_opus");
    }
}

fn collect_c_files(
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
    skip_arm: bool,
    allow_x86: bool,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if skip_arm && name == "arm" {
                continue;
            }
            if !allow_x86 && name == "x86" {
                continue;
            }
            if name == "doc"
                || name == "docs"
                || name == "test"
                || name == "tests"
                || name == "examples"
                || name == "apps"
                || name == "tools"
                || name == "dump_modes"
                || name == "cmake"
                || name == "dnn"
            {
                continue;
            }
            collect_c_files(&path, out, skip_arm, allow_x86);
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("c"))
            .unwrap_or(false)
        {
            if allow_x86 {
                let is_x86_path = path
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("x86"));
                if is_x86_path {
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if name.contains("sse4_1") || name.contains("avx2") || name.contains("avx") {
                        continue;
                    }
                }
            } else {
                let is_x86_path = path
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("x86"));
                if is_x86_path {
                    continue;
                }
            }
            out.push(path);
        }
    }
}

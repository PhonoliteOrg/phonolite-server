fn main() {
    if std::env::var("CARGO_FEATURE_FFI_OPUS").is_ok() {
        println!("cargo:rerun-if-changed=../../vendor/opus");
        let mut build = cc::Build::new();
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            build.out_dir(out_dir);
        }
        build
            .include("../../vendor/opus/include")
            .include("../../vendor/opus/celt")
            .include("../../vendor/opus/silk")
            .include("../../vendor/opus/silk/float")
            .define("OPUS_BUILD", None)
            .define("HAVE_LRINTF", None)
            .define("HAVE_LRINT", None)
            .define("USE_ALLOCA", None)
            .flag_if_supported("-std=c99");

        let root = std::path::Path::new("../../vendor/opus");
        let mut files = Vec::new();
        let skip_arm = std::env::var("CARGO_CFG_TARGET_ARCH")
            .map(|arch| arch != "arm" && arch != "aarch64")
            .unwrap_or(true);
        collect_c_files(root, &mut files, skip_arm);
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
                || name == "x86"
            {
                continue;
            }
            collect_c_files(&path, out, skip_arm);
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("c"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

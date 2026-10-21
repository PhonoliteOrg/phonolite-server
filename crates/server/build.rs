use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> io::Result<()> {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let web = manifest.join("../../web").canonicalize()?;
    compile_windows_icon(&manifest, &web)?;
    let mut assets = Vec::new();
    for directory in ["templates", "static"] {
        let directory = web.join(directory);
        // Cargo watches directory contents recursively, including added/removed files.
        println!("cargo:rerun-if-changed={}", directory.display());
        collect_assets(&web, &directory, &mut assets)?;
    }
    assets.sort_by(|left, right| left.0.cmp(&right.0));

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("web_assets.rs");
    let mut output = File::create(output)?;
    writeln!(output, "static EMBEDDED_WEB_ASSETS: &[(&str, &[u8])] = &[")?;
    for (name, path) in assets {
        writeln!(output, "    ({name:?}, include_bytes!({path:?})),")?;
    }
    writeln!(output, "];\n")?;
    Ok(())
}

fn compile_windows_icon(manifest: &Path, web: &Path) -> io::Result<()> {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let resource = manifest.join("resources/phonolite-server.rc");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed={}", resource.display());
    println!(
        "cargo:rerun-if-changed={}",
        web.join("static/img/favicon.ico").display()
    );
    println!("cargo:rerun-if-env-changed=WINDRES");

    let target = env::var("TARGET").unwrap();
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let compiled = output.join(if msvc {
        "phonolite-server.res"
    } else {
        "phonolite-server.o"
    });
    let mut compiler = if msvc {
        let mut command =
            cc::windows_registry::find(&target, "rc.exe").unwrap_or_else(|| Command::new("rc.exe"));
        command
            .arg("/nologo")
            .arg("/fo")
            .arg(&compiled)
            .arg(&resource);
        command
    } else {
        let mut command = Command::new(env::var_os("WINDRES").unwrap_or_else(|| "windres".into()));
        command
            .arg("-i")
            .arg(&resource)
            .args(["-O", "coff", "-o"])
            .arg(&compiled);
        command
    };
    let result = compiler.current_dir(manifest).output()?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "failed to compile the server icon: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    println!("cargo:rustc-link-arg-bin=phonolite-server={}", compiled.display());
    Ok(())
}

fn collect_assets(
    web: &Path,
    directory: &Path,
    assets: &mut Vec<(String, PathBuf)>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_assets(web, &path, assets)?;
        } else if kind.is_file() {
            let name = path
                .strip_prefix(web)
                .unwrap()
                .to_str()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 web asset path")
                })?
                .replace('\\', "/");
            assets.push((name, path));
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "web assets must be regular files or directories: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

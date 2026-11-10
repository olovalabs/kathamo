use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let prepare = manifest.join("prepare-runtime.py");
    println!("cargo:rerun-if-changed={}", prepare.display());
    println!("cargo:rerun-if-changed=oauth-callback.html");
    println!("cargo:rerun-if-changed=../assets/logo/kathamo.ico");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    let output = std::process::Command::new(python)
        .arg(&prepare)
        .args([&target_os, &target_arch])
        .arg(manifest.join("../target/kathamo-provider"))
        .output()
        .unwrap_or_else(|error| {
            panic!("Python 3 is required to bundle the Kathamo provider: {error}")
        });
    assert!(
        output.status.success(),
        "Could not prepare the Kathamo provider:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).expect("provider bundle metadata is UTF-8");
    let mut lines = output.lines();
    let bundle = lines.next().expect("provider bundle path");
    let unpacked: u64 = lines.next().expect("provider size").parse().unwrap();
    let version = lines.next().expect("provider version");
    println!("cargo:rerun-if-changed={bundle}");

    // Put the payload directly in the object file to keep it out of Rust crate metadata.
    let (section, symbol) = match target_os.as_str() {
        "macos" => (".section __TEXT,__const", "_kathamo_provider_payload"),
        "windows" => (".section .rdata,\"dr\"", "kathamo_provider_payload"),
        _ => (".section .rodata", "kathamo_provider_payload"),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let size = fs::metadata(&bundle).unwrap().len();
    let bundle = bundle.replace('\\', "/").replace('"', "\\\"");
    fs::write(
        out.join("provider.s"),
        format!("{section}\n.globl {symbol}\n{symbol}:\n.incbin \"{bundle}\"\n"),
    )
    .unwrap();
    fs::write(
        out.join("provider.rs"),
        format!(
            "const BUNDLE_SIZE: usize = {size};\nconst UNPACKED_SIZE: u64 = {unpacked};\nconst VERSION: &str = {version:?};\n"
        ),
    )
    .unwrap();
}

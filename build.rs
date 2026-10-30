use std::path::PathBuf;

fn main() {
    // App icon and other Win32 resources (needs rc.exe from the Windows SDK).
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/best-pdf.ico");
    embed_resource::compile("assets/app.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("compile assets/app.rc");

    // Copy pdfium.dll next to the built exe so `cargo run` works in place.
    let dll = PathBuf::from("vendor/pdfium/bin/pdfium.dll");
    println!("cargo:rerun-if-changed={}", dll.display());
    if !dll.exists() {
        println!("cargo:warning=pdfium.dll missing; run scripts/fetch-pdfium.ps1");
        return;
    }
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out → exe lives in target/<profile>
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out.ancestors().nth(3).unwrap();
    std::fs::copy(&dll, profile_dir.join("pdfium.dll")).expect("copy pdfium.dll");
}

//! Иконка exe на Windows: готовый ресурс `assets/branding/pooprusteek.res`
//! отдаётся линкеру MSVC. Без зависимостей и без rc.exe на машине сборки.

fn main() {
    const RESOURCE: &str = "assets/branding/pooprusteek.res";
    println!("cargo:rerun-if-changed={RESOURCE}");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        let resource = std::path::Path::new(&manifest_dir).join(RESOURCE);
        println!("cargo:rustc-link-arg-bins={}", resource.display());
    }
}

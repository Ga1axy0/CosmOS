fn target_path() -> String {
    let target =
        std::env::var("TARGET").unwrap_or_else(|_| "riscv64gc-unknown-none-elf".to_string());
    format!("../user/target/{}/release/", target)
}

fn main() {
    println!("cargo:rerun-if-changed=src/linker.ld");
    println!("cargo:rerun-if-changed=src/linker-loongarch64.ld");
    if std::env::var_os("CARGO_FEATURE_PLATFORM_LS2K1000_NEBULA").is_some() {
        println!("cargo:rustc-link-arg=--defsym=NEBULA_BOOT=1");
    }
    println!("cargo:rerun-if-changed=../user/src/");
    println!("cargo:rerun-if-changed={}", target_path());
}

fn target_path() -> String {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "riscv64gc-unknown-none-elf".to_string());
    format!("../user/target/{}/release/", target)
}

fn main() {
    println!("cargo:rerun-if-changed=../user/src/");
    println!("cargo:rerun-if-changed={}", target_path());
}

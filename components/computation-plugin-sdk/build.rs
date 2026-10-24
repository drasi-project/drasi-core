fn main() {
    println!(
        "cargo:rustc-env=DRASI_COMPUTATION_TARGET={}",
        std::env::var("TARGET").expect("Cargo supplies TARGET")
    );
}

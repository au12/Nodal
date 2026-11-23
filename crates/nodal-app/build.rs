fn main() {
    // The learned matcher statically links ONNX Runtime, whose Core ML code checks the OS version
    // with `@available`. Clang turns that into calls to `__isPlatformVersionAtLeast` from its
    // builtins library, which rustc does not link on its own (it passes -nodefaultlibs), so ask
    // the linker driver for it. See https://ort.pyke.io/perf/execution-providers#coreml
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") && std::env::var_os("CARGO_FEATURE_ML").is_some() {
        println!("cargo:rustc-link-arg=-fapple-link-rtlib");
    }
}

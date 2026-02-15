fn main() {
    // Minimal build script for N-API compatibility
    // The napi-rs crate handles most platform-specific setup
    
    // Just ensure we rebuild if this file changes
    println!("cargo:rerun-if-changed=build.rs");
}

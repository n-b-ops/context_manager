use std::env;

fn main() {
    // Expose the build profile to the binary at compile time (optional convenience).
    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=CONTEXT_BUILDER_PROFILE={}", profile);
}

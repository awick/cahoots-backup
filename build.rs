// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

fn main() {
    // Windows gives the main thread 1 MB of stack where macOS and Linux give 8.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
        println!("cargo:rustc-link-arg-bins={}", if msvc { "/STACK:8388608" } else { "-Wl,--stack,8388608" });
    }
}

// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Windows gives the main thread 1 MB of stack where macOS and Linux give 8.
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    println!("cargo:rustc-link-arg-bins={}", if msvc { "/STACK:8388608" } else { "-Wl,--stack,8388608" });
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Cahoots Backup");
        res.set("FileDescription", "Cahoots Backup");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no icon in the program: {e}");
        }
    }
}

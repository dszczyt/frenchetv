fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // The Widevine CDM host shim only builds against Linux/macOS's dlfcn.h
    // (dlopen/dlsym) and assumes the Itanium C++ ABI to match libwidevinecdm's
    // vtable layout — see drm/cdm.rs for why it isn't ported to Windows.
    if os == "linux" || os == "macos" {
        cc::Build::new()
            .cpp(true)
            .file("cdm_shim.cpp")
            .flag_if_supported("-std=c++14")
            .flag_if_supported("-fno-exceptions")
            .flag_if_supported("-fno-rtti")
            .compile("cdm_shim");

        // libdl is required for dlopen/dlsym used in cdm_shim.cpp (Linux only;
        // macOS's dlopen/dlsym live in libSystem and are linked by default).
        if os == "linux" {
            println!("cargo:rustc-link-lib=dl");
        }
    }

    // Re-run if shim source changes.
    println!("cargo:rerun-if-changed=cdm_shim.cpp");
}

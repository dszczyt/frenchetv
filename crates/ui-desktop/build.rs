fn main() {
    // Compile the Widevine CDM host shim (C++14, no exceptions, no RTTI).
    cc::Build::new()
        .cpp(true)
        .file("cdm_shim.cpp")
        .flag_if_supported("-std=c++14")
        .flag_if_supported("-fno-exceptions")
        .flag_if_supported("-fno-rtti")
        .compile("cdm_shim");

    // libdl is required for dlopen/dlsym used in cdm_shim.cpp.
    println!("cargo:rustc-link-lib=dl");
    // Re-run if shim source changes.
    println!("cargo:rerun-if-changed=cdm_shim.cpp");
}

fn main() {
    let mut build = cc::Build::new();
    build
        .file("src/sqlite-vec.c")
        .define("SQLITE_CORE", None)
        .warnings(false);

    // Hardening flags for security.
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") || cfg!(target_os = "freebsd") {
        build
            .flag("-fstack-protector-strong")
            .flag("-D_FORTIFY_SOURCE=2")
            .flag("-fPIE")
            .flag("-fno-omit-frame-pointer");
    }

    // Additional warnings as errors for our own code quality.
    build.flag("-Wall").flag("-Wextra");

    build.compile("sqlite_vec0");
}

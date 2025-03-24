fn main() {
    cc::Build::new()
        .file("src/sqlite-vec.c")
        .define("SQLITE_CORE", None)
        .compile("sqlite_vec0");
}

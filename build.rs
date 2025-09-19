fn main() {
    println!("cargo:rerun-if-changed=src/c/nvcapture.c");
    println!("cargo:rerun-if-changed=src/c/NvFBC.h");
    cc::Build::new()
        .file("src/c/nvcapture.c")
        .compile("nvcapture");
}

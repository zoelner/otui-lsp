use std::path::PathBuf;

fn main() {
    let src_dir: PathBuf = ["src"].iter().collect();

    let mut build = cc::Build::new();
    build.include(&src_dir);
    build
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs");

    let parser = src_dir.join("parser.c");
    let scanner = src_dir.join("scanner.c");

    build.file(&parser);
    build.file(&scanner);

    println!("cargo:rerun-if-changed={}", parser.display());
    println!("cargo:rerun-if-changed={}", scanner.display());

    build.compile("tree-sitter-otui");
}

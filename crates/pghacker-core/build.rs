fn main() {
    // The migrations are embedded with include_dir!, which doesn't notice new files.
    println!("cargo:rerun-if-changed=../../migrations");
    println!("cargo:rerun-if-changed=sql");
}

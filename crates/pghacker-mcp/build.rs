// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

fn main() {
    // The skills are embedded with include_dir!, which doesn't notice new files.
    println!("cargo:rerun-if-changed=../../skills");
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

fn main() {
    let source = Path::new("static_atoms.txt");
    println!("cargo:rerun-if-changed={}", source.display());

    let atoms = BufReader::new(File::open(source).expect("open static_atoms.txt"))
        .lines()
        .map(|line| line.expect("read static atom"));
    let output = Path::new(&env::var_os("OUT_DIR").expect("OUT_DIR")).join("atom.rs");

    string_cache_codegen::AtomType::new("generated::Atom", "wild_buzzard_static_atom!")
        .atoms(atoms)
        .write_to_file(&output)
        .expect("generate static atoms");
}

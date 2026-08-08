/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::env;
use std::path::Path;
use std::process::{exit, Command};
use std::sync::LazyLock;
use walkdir::WalkDir;

#[cfg(feature = "gecko")]
compile_error!("Gecko property generation is prohibited in the Wild Buzzard Stylo workspace");

#[cfg(not(feature = "wild_buzzard"))]
compile_error!("the style build requires the wild_buzzard feature");

pub static PYTHON: LazyLock<String> = LazyLock::new(|| {
    env::var("PYTHON3").ok().unwrap_or_else(|| {
        let candidates = ["python3"];
        for &name in &candidates {
            if Command::new(name)
                .arg("--version")
                .output()
                .ok()
                .is_some_and(|out| out.status.success())
            {
                return name.to_owned();
            }
        }
        panic!(
            "Can't find python (tried {})! Try fixing PATH or setting the PYTHON3 env var",
            candidates.join(", ")
        )
    })
});

fn generate_properties(engine: &str) {
    for entry in WalkDir::new("properties") {
        let entry = entry.unwrap();
        match entry.path().extension().and_then(|e| e.to_str()) {
            Some("mako") | Some("rs") | Some("py") | Some("zip") | Some("toml") => {
                println!("cargo:rerun-if-changed={}", entry.path().display());
            }
            _ => {}
        }
    }

    let script = Path::new(&env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("properties")
        .join("build.py");

    let status = Command::new(&*PYTHON)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(&script)
        .arg(engine)
        .arg("style-crate")
        .status()
        .unwrap();
    if !status.success() {
        exit(1)
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:out_dir={}", env::var("OUT_DIR").unwrap());
    generate_properties("servo");
}

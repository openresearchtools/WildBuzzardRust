/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#[test]
fn exported_atom_macro_uses_the_generated_static_set() {
    assert_eq!(stylo_atoms::atom!("grid"), stylo_atoms::Atom::from("grid"));
}

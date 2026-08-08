/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Strongly typed name, namespace, and prefix atoms used by Stylo selectors.

#![forbid(unsafe_code)]
#![deny(missing_docs, warnings)]

use string_cache::{EmptyStaticAtomSet, PhfStrSet, StaticAtomSet};

macro_rules! atom_domain {
    ($set:ident, $atom:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Eq, Ord, PartialEq, PartialOrd)]
        pub struct $set;

        impl StaticAtomSet for $set {
            fn get() -> &'static PhfStrSet {
                EmptyStaticAtomSet::get()
            }

            fn empty_string_index() -> u32 {
                EmptyStaticAtomSet::empty_string_index()
            }
        }

        #[doc = $docs]
        pub type $atom = string_cache::Atom<$set>;
    };
}

atom_domain!(
    LocalNameStaticSet,
    LocalName,
    "An interned element or attribute local name."
);
atom_domain!(NamespaceStaticSet, Namespace, "An interned namespace URL.");
atom_domain!(PrefixStaticSet, Prefix, "An interned namespace prefix.");

/// Interns a local name in the local-name atom domain.
#[macro_export]
macro_rules! local_name {
    ($value:expr) => {
        $crate::LocalName::from($value)
    };
}

/// Interns a standards namespace URL in the namespace atom domain.
#[macro_export]
macro_rules! ns {
    () => {
        $crate::Namespace::from("")
    };
    (html) => {
        $crate::Namespace::from("http://www.w3.org/1999/xhtml")
    };
    (mathml) => {
        $crate::Namespace::from("http://www.w3.org/1998/Math/MathML")
    };
    (svg) => {
        $crate::Namespace::from("http://www.w3.org/2000/svg")
    };
    (xlink) => {
        $crate::Namespace::from("http://www.w3.org/1999/xlink")
    };
    (xml) => {
        $crate::Namespace::from("http://www.w3.org/XML/1998/namespace")
    };
    (xmlns) => {
        $crate::Namespace::from("http://www.w3.org/2000/xmlns/")
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn namespace_macros_use_standard_urls() {
        assert_eq!(&*ns!(), "");
        assert_eq!(&*ns!(xml), "http://www.w3.org/XML/1998/namespace");
        assert_ne!(ns!(html), ns!(svg));
    }

    #[test]
    fn local_names_are_interned() {
        assert_eq!(local_name!("class"), local_name!(String::from("class")));
    }
}

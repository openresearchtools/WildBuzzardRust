/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Coverage for the actual selector policy exposed by Wild Buzzard's default Stylo profile.

use style::selector_parser::SelectorParser;
use style::stylesheets::UrlExtraData;
use url::Url;

fn parses(selector: &str) -> bool {
    let url_data = UrlExtraData::from(Url::parse("https://example.invalid/style.css").unwrap());
    SelectorParser::parse_author_origin_no_namespace(selector, &url_data).is_ok()
}

#[test]
fn default_style_parser_accepts_firefox_facing_selector_features() {
    assert!(parses("section#main.panel > p.note.active:nth-child(2)"));
    assert!(parses("section > p + p.active"));
    assert!(parses(":is(p, div).active:not(.disabled):hover"));
    assert!(parses("section:has(> p.active)"));
    assert!(parses("p:nth-child(1 of .active)"));
}

#[test]
fn default_style_parser_rejects_malformed_level_four_selectors() {
    assert!(!parses("section:has()"));
    assert!(!parses("p:nth-child(1 of)"));
}

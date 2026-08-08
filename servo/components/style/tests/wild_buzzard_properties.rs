/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Wild Buzzard coverage for Stylo's generated declaration parser and CSSOM serializer.

use style::context::QuirksMode;
use style::properties::{parse_style_attribute, Importance, PropertyDeclarationBlock, PropertyId};
use style::stylesheets::{CssRuleType, UrlExtraData};
use url::Url;

fn parse(input: &str) -> PropertyDeclarationBlock {
    let url_data = UrlExtraData::from(Url::parse("https://example.invalid/base/").unwrap());
    parse_style_attribute(
        input,
        &url_data,
        None,
        QuirksMode::NoQuirks,
        CssRuleType::Style,
    )
}

fn property_value(block: &PropertyDeclarationBlock, name: &str) -> String {
    let property = PropertyId::parse_unchecked_for_testing(name).unwrap();
    let mut serialized = String::new();
    block
        .property_value_to_css(&property, &mut serialized)
        .unwrap();
    serialized
}

fn serialize(block: &PropertyDeclarationBlock) -> String {
    let mut serialized = String::new();
    block.to_css(&mut serialized).unwrap();
    serialized
}

#[test]
fn generated_shorthand_parser_round_trips_through_longhands() {
    let block = parse("margin: 1px 2px 3px 4px; overflow: auto hidden;");

    assert_eq!(block.len(), 6);
    assert_eq!(property_value(&block, "margin"), "1px 2px 3px 4px");
    assert_eq!(property_value(&block, "margin-top"), "1px");
    assert_eq!(property_value(&block, "margin-right"), "2px");
    assert_eq!(property_value(&block, "margin-bottom"), "3px");
    assert_eq!(property_value(&block, "margin-left"), "4px");
    assert_eq!(property_value(&block, "overflow"), "auto hidden");

    let serialized = serialize(&block);
    let reparsed = parse(&serialized);
    assert_eq!(property_value(&reparsed, "margin"), "1px 2px 3px 4px");
    assert_eq!(property_value(&reparsed, "overflow"), "auto hidden");
}

#[test]
fn cssom_priority_and_source_order_are_preserved() {
    let block = parse(
        "width: 1px; width: 2px; height: 3px !important; height: 4px; \
         not-a-property: ignored; color: rgb(255 0 0 / 50%);",
    );

    assert_eq!(property_value(&block, "width"), "2px");
    assert_eq!(property_value(&block, "height"), "3px");
    assert_eq!(
        block.property_priority(&PropertyId::parse_unchecked_for_testing("height").unwrap()),
        Importance::Important,
    );
    assert_eq!(property_value(&block, "color"), "rgba(255, 0, 0, 0.5)");
    assert!(serialize(&block).contains("height: 3px !important;"));
}

#[test]
fn custom_properties_and_var_references_survive_serialization() {
    let block = parse("--space: 6px; padding: var(--space); width: calc(50% - 3px);");

    assert_eq!(property_value(&block, "--space"), "6px");
    assert_eq!(property_value(&block, "padding"), "var(--space)");
    assert_eq!(property_value(&block, "width"), "calc(50% - 3px)");

    let serialized = serialize(&block);
    let reparsed = parse(&serialized);
    assert_eq!(property_value(&reparsed, "--space"), "6px");
    assert_eq!(property_value(&reparsed, "padding"), "var(--space)");
    assert_eq!(property_value(&reparsed, "width"), "calc(50% - 3px)");
}

#[test]
fn css_wide_keyword_expands_and_serializes_as_a_shorthand() {
    let block = parse("border: initial;");

    assert_eq!(property_value(&block, "border-top-width"), "initial");
    assert_eq!(property_value(&block, "border-right-style"), "initial");
    assert_eq!(property_value(&block, "border-bottom-color"), "initial");
    assert_eq!(property_value(&block, "border"), "initial");
}

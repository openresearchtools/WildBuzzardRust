/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Loss-checked projection from Stylo computed values into wave-two layout values.

use num_traits::ToPrimitive;
use style::properties::ComputedValues;
use style::properties::longhands::{
    box_sizing, direction, flex_direction, flex_wrap, text_wrap_mode, white_space_collapse,
};
use style::values::computed::length::NonNegativeLengthPercentageOrNormal;
use style::values::computed::length_percentage::Unpacked;
use style::values::computed::{
    BorderStyle, Display as StyloDisplay, FlexBasis as StyloFlexBasis, Image as StyloImage, Length,
    LengthPercentage, LineHeight, Margin, MaxSize, NonNegativeNumber, Size, WritingModeProperty,
};
use style::values::generics::flex::GenericFlexBasis;
use style::values::generics::length::{
    GenericLengthPercentageOrNormal, GenericMargin, GenericMaxSize, GenericSize,
};
use style::values::specified::align::AlignFlags;
use wild_buzzard_dom::NodeId;
use wild_buzzard_layout::{
    AlignItems, AlignSelf, Au, AutomaticMarginEdges, BackgroundImageLayers, BoxSizing, Color,
    ComputedStyle, Display, Edges, EffectiveContainment, FlexBasis, FlexDirection, FlexFactor,
    FlexStyle, FlexWrap, InlineDirection, JustifyContent,
    LengthPercentage as LayoutLengthPercentage, MaxSizeValue, PercentageEdges, SizeValue,
    WhiteSpace, WritingMode,
};

use crate::error::{StyleAdapterError, UnsupportedComputedValue};

pub(crate) fn translate_computed_style(
    node: NodeId,
    values: &ComputedValues,
) -> Result<ComputedStyle, StyleAdapterError> {
    let display = translate_display(node, values.clone_display())?;
    let flex = translate_flex(node, values, display == Display::Flex)?;
    let margin = translate_margin_edges(node, values)?;
    let padding = translate_padding_edges(node, values)?;
    let sizing = translate_sizing(node, values)?;
    let text = translate_text(node, values)?;
    let colors = translate_colors(node, values)?;

    Ok(ComputedStyle {
        display,
        flex,
        margin: margin.absolute,
        margin_percentage: margin.percentage,
        automatic_margin: margin.automatic,
        border: translate_border_edges(values),
        padding: padding.absolute,
        padding_percentage: padding.percentage,
        width: sizing.width,
        height: sizing.height,
        min_width: sizing.min_width,
        min_height: sizing.min_height,
        max_width: sizing.max_width,
        max_height: sizing.max_height,
        box_sizing: sizing.box_sizing,
        writing_mode: sizing.writing_mode,
        inline_direction: translate_inline_direction(values.clone_direction()),
        font_size: text.font_size,
        line_height: text.line_height,
        color: colors.foreground,
        background_color: colors.background,
        background_image_layers: translate_background_image_layers(values),
        effective_containment: translate_effective_containment(values),
        white_space: text.white_space,
    })
}

fn translate_background_image_layers(values: &ComputedValues) -> BackgroundImageLayers {
    let images = values.clone_background_image();
    if images.0.len() == 1 && matches!(images.0.first(), Some(StyloImage::None)) {
        BackgroundImageLayers::SingleNone
    } else {
        BackgroundImageLayers::Meaningful
    }
}

fn translate_effective_containment(values: &ComputedValues) -> EffectiveContainment {
    let declared = values.clone_contain();
    let container_type = values.clone_container_type();
    if !declared.is_empty() || container_type.is_size_container_type() {
        EffectiveContainment::Any
    } else {
        EffectiveContainment::None
    }
}

fn translate_inline_direction(value: direction::computed_value::T) -> InlineDirection {
    match value {
        direction::computed_value::T::Ltr => InlineDirection::Ltr,
        direction::computed_value::T::Rtl => InlineDirection::Rtl,
    }
}

struct TranslatedEdges {
    absolute: Edges,
    percentage: PercentageEdges,
}

struct TranslatedMargins {
    absolute: Edges,
    percentage: PercentageEdges,
    automatic: AutomaticMarginEdges,
}

struct TranslatedMargin {
    absolute: Au,
    percentage: i32,
    automatic: bool,
}

struct TranslatedSizing {
    width: SizeValue,
    height: SizeValue,
    min_width: SizeValue,
    min_height: SizeValue,
    max_width: MaxSizeValue,
    max_height: MaxSizeValue,
    box_sizing: BoxSizing,
    writing_mode: WritingMode,
}

struct TranslatedText {
    font_size: Au,
    line_height: Au,
    white_space: WhiteSpace,
}

struct TranslatedColors {
    foreground: Color,
    background: Color,
}

fn translate_margin_edges(
    node: NodeId,
    values: &ComputedValues,
) -> Result<TranslatedMargins, StyleAdapterError> {
    let top = translate_margin(node, "margin-top", values.clone_margin_top())?;
    let right = translate_margin(node, "margin-right", values.clone_margin_right())?;
    let bottom = translate_margin(node, "margin-bottom", values.clone_margin_bottom())?;
    let left = translate_margin(node, "margin-left", values.clone_margin_left())?;
    Ok(TranslatedMargins {
        absolute: Edges {
            top: top.absolute,
            right: right.absolute,
            bottom: bottom.absolute,
            left: left.absolute,
        },
        percentage: PercentageEdges {
            top: top.percentage,
            right: right.percentage,
            bottom: bottom.percentage,
            left: left.percentage,
        },
        automatic: AutomaticMarginEdges {
            top: top.automatic,
            right: right.automatic,
            bottom: bottom.automatic,
            left: left.automatic,
        },
    })
}

fn translate_padding_edges(
    node: NodeId,
    values: &ComputedValues,
) -> Result<TranslatedEdges, StyleAdapterError> {
    let top = translate_length_percentage(node, "padding-top", &values.clone_padding_top().0)?;
    let right =
        translate_length_percentage(node, "padding-right", &values.clone_padding_right().0)?;
    let bottom =
        translate_length_percentage(node, "padding-bottom", &values.clone_padding_bottom().0)?;
    let left = translate_length_percentage(node, "padding-left", &values.clone_padding_left().0)?;
    Ok(TranslatedEdges {
        absolute: Edges {
            top: top.0,
            right: right.0,
            bottom: bottom.0,
            left: left.0,
        },
        percentage: PercentageEdges {
            top: top.1,
            right: right.1,
            bottom: bottom.1,
            left: left.1,
        },
    })
}

fn translate_border_edges(values: &ComputedValues) -> Edges {
    Edges {
        top: translate_border(
            values.clone_border_top_style(),
            values.clone_border_top_width().0,
        ),
        right: translate_border(
            values.clone_border_right_style(),
            values.clone_border_right_width().0,
        ),
        bottom: translate_border(
            values.clone_border_bottom_style(),
            values.clone_border_bottom_width().0,
        ),
        left: translate_border(
            values.clone_border_left_style(),
            values.clone_border_left_width().0,
        ),
    }
}

fn translate_sizing(
    node: NodeId,
    values: &ComputedValues,
) -> Result<TranslatedSizing, StyleAdapterError> {
    let box_sizing = match values.clone_box_sizing() {
        box_sizing::computed_value::T::ContentBox => BoxSizing::ContentBox,
        box_sizing::computed_value::T::BorderBox => BoxSizing::BorderBox,
    };
    let writing_mode = match values.clone_writing_mode() {
        WritingModeProperty::HorizontalTb => WritingMode::HorizontalTb,
        WritingModeProperty::VerticalRl => WritingMode::VerticalRl,
        WritingModeProperty::VerticalLr => WritingMode::VerticalLr,
    };
    Ok(TranslatedSizing {
        width: translate_size(node, "width", values.clone_width())?,
        height: translate_size(node, "height", values.clone_height())?,
        min_width: translate_size(node, "min-width", values.clone_min_width())?,
        min_height: translate_size(node, "min-height", values.clone_min_height())?,
        max_width: translate_max_size(node, "max-width", values.clone_max_width())?,
        max_height: translate_max_size(node, "max-height", values.clone_max_height())?,
        box_sizing,
        writing_mode,
    })
}

fn translate_text(
    node: NodeId,
    values: &ComputedValues,
) -> Result<TranslatedText, StyleAdapterError> {
    let font_size = translate_px(node, "font-size", values.clone_font_size().computed_size())?;
    let line_height = match values.clone_line_height() {
        // This is an explicit early-layout used-value policy, not a change to
        // Stylo's computed value. A font backend will replace it with metrics.
        LineHeight::Normal => font_size.scale(6, 5),
        LineHeight::Number(number) => {
            translate_scaled_au(node, "line-height", font_size, number.0)?
        }
        LineHeight::Length(length) => translate_px(node, "line-height", length.0)?,
    };
    let collapse = values.clone_white_space_collapse();
    let wrapping = values.clone_text_wrap_mode();
    let white_space = match (collapse, wrapping) {
        (
            white_space_collapse::computed_value::T::Collapse,
            text_wrap_mode::computed_value::T::Wrap,
        ) => WhiteSpace::Normal,
        (
            white_space_collapse::computed_value::T::Collapse,
            text_wrap_mode::computed_value::T::Nowrap,
        ) => WhiteSpace::Nowrap,
        (
            white_space_collapse::computed_value::T::Preserve,
            text_wrap_mode::computed_value::T::Nowrap,
        ) => WhiteSpace::Pre,
        _ => {
            return Err(StyleAdapterError::UnsupportedComputedValue {
                node,
                value: UnsupportedComputedValue::WhiteSpace(format!("{collapse:?}/{wrapping:?}")),
            });
        }
    };
    Ok(TranslatedText {
        font_size,
        line_height,
        white_space,
    })
}

fn translate_colors(
    node: NodeId,
    values: &ComputedValues,
) -> Result<TranslatedColors, StyleAdapterError> {
    let foreground = values.clone_color();
    let background = values
        .clone_background_color()
        .resolve_to_absolute(&foreground);
    Ok(TranslatedColors {
        foreground: translate_color(node, "color", foreground)?,
        background: translate_color(node, "background-color", background)?,
    })
}

fn translate_size(
    node: NodeId,
    property: &'static str,
    value: Size,
) -> Result<SizeValue, StyleAdapterError> {
    match value {
        GenericSize::Auto => Ok(SizeValue::Auto),
        GenericSize::LengthPercentage(value) => Ok(SizeValue::LengthPercentage(
            translate_sizing_length_percentage(node, property, &value.0)?,
        )),
        other => Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::Sizing(property, format!("{other:?}")),
        }),
    }
}

fn translate_max_size(
    node: NodeId,
    property: &'static str,
    value: MaxSize,
) -> Result<MaxSizeValue, StyleAdapterError> {
    match value {
        GenericMaxSize::None => Ok(MaxSizeValue::None),
        GenericMaxSize::LengthPercentage(value) => Ok(MaxSizeValue::LengthPercentage(
            translate_sizing_length_percentage(node, property, &value.0)?,
        )),
        other => Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::Sizing(property, format!("{other:?}")),
        }),
    }
}

fn translate_sizing_length_percentage(
    node: NodeId,
    property: &'static str,
    value: &LengthPercentage,
) -> Result<LayoutLengthPercentage, StyleAdapterError> {
    let (length, percentage) = translate_length_percentage(node, property, value)?;
    Ok(LayoutLengthPercentage { length, percentage })
}

fn translate_display(node: NodeId, value: StyloDisplay) -> Result<Display, StyleAdapterError> {
    if value == StyloDisplay::None {
        Ok(Display::None)
    } else if value == StyloDisplay::Block {
        Ok(Display::Block)
    } else if value == StyloDisplay::Inline {
        Ok(Display::Inline)
    } else if value == StyloDisplay::InlineBlock {
        Ok(Display::InlineBlock)
    } else if value == StyloDisplay::Flex {
        Ok(Display::Flex)
    } else {
        Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::Display(format!("{value:?}")),
        })
    }
}

fn translate_flex(
    node: NodeId,
    values: &ComputedValues,
    is_flex_container: bool,
) -> Result<FlexStyle, StyleAdapterError> {
    let mut translated = FlexStyle {
        basis: translate_flex_basis(node, values.clone_flex_basis())?,
        grow: translate_flex_factor(node, "flex-grow", values.clone_flex_grow())?,
        shrink: translate_flex_factor(node, "flex-shrink", values.clone_flex_shrink())?,
        align_self: translate_align_self(node, values.clone_align_self().0)?,
        order: values.clone_order(),
        ..FlexStyle::default()
    };
    if !is_flex_container {
        return Ok(translated);
    }

    validate_align_content(node, values.clone_align_content())?;
    translated.direction = match values.clone_flex_direction() {
        flex_direction::computed_value::T::Row => FlexDirection::Row,
        flex_direction::computed_value::T::Column => FlexDirection::Column,
        other => return unsupported_flex(node, "flex-direction", other),
    };
    translated.wrap = match values.clone_flex_wrap() {
        flex_wrap::computed_value::T::Nowrap => FlexWrap::NoWrap,
        flex_wrap::computed_value::T::Wrap => FlexWrap::Wrap,
        other @ flex_wrap::computed_value::T::WrapReverse => {
            return unsupported_flex(node, "flex-wrap", other);
        }
    };
    translated.justify_content = translate_justify_content(node, values.clone_justify_content())?;
    translated.align_items = translate_align_items(node, values.clone_align_items().0)?;
    translated.row_gap = translate_gap(node, "row-gap", values.clone_row_gap())?;
    translated.column_gap = translate_gap(node, "column-gap", values.clone_column_gap())?;
    Ok(translated)
}

fn validate_align_content(
    node: NodeId,
    value: style::values::computed::ContentDistribution,
) -> Result<(), StyleAdapterError> {
    let primary = value.primary();
    if primary.flags().is_empty() && primary.value() == AlignFlags::NORMAL {
        Ok(())
    } else {
        unsupported_flex(node, "align-content", value)
    }
}

fn translate_flex_basis(
    node: NodeId,
    value: StyloFlexBasis,
) -> Result<FlexBasis, StyleAdapterError> {
    match value {
        GenericFlexBasis::Content => Ok(FlexBasis::Content),
        GenericFlexBasis::Size(GenericSize::Auto) => Ok(FlexBasis::Auto),
        GenericFlexBasis::Size(GenericSize::LengthPercentage(value)) => {
            Ok(FlexBasis::LengthPercentage(
                translate_flex_length_percentage(node, "flex-basis", &value.0)?,
            ))
        }
        other @ GenericFlexBasis::Size(_) => unsupported_flex(node, "flex-basis", other),
    }
}

fn translate_flex_factor(
    node: NodeId,
    property: &'static str,
    value: NonNegativeNumber,
) -> Result<FlexFactor, StyleAdapterError> {
    let factor = value.0;
    let fixed = f64::from(factor) * f64::from(1_000_000_u32);
    if !factor.is_finite() || fixed < 0.0 || fixed > f64::from(u32::MAX) {
        return unsupported_flex(node, property, factor);
    }
    let fixed =
        fixed
            .round()
            .to_u32()
            .ok_or_else(|| StyleAdapterError::UnsupportedComputedValue {
                node,
                value: UnsupportedComputedValue::Flex(property, format!("{factor:?}")),
            })?;
    Ok(FlexFactor::from_millionths(fixed))
}

fn translate_gap(
    node: NodeId,
    property: &'static str,
    value: NonNegativeLengthPercentageOrNormal,
) -> Result<LayoutLengthPercentage, StyleAdapterError> {
    match value {
        GenericLengthPercentageOrNormal::Normal => Ok(LayoutLengthPercentage::default()),
        GenericLengthPercentageOrNormal::LengthPercentage(value) => {
            translate_flex_length_percentage(node, property, &value.0)
        }
    }
}

fn translate_flex_length_percentage(
    node: NodeId,
    property: &'static str,
    value: &LengthPercentage,
) -> Result<LayoutLengthPercentage, StyleAdapterError> {
    translate_sizing_length_percentage(node, property, value).map_err(|_| {
        StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::Flex(property, format!("{value:?}")),
        }
    })
}

fn translate_justify_content(
    node: NodeId,
    value: style::values::computed::ContentDistribution,
) -> Result<JustifyContent, StyleAdapterError> {
    let flags = value.primary();
    if !flags.flags().is_empty() {
        return unsupported_flex(node, "justify-content", value);
    }
    match flags.value() {
        AlignFlags::NORMAL | AlignFlags::START | AlignFlags::FLEX_START => {
            Ok(JustifyContent::Start)
        }
        AlignFlags::END | AlignFlags::FLEX_END => Ok(JustifyContent::End),
        AlignFlags::CENTER => Ok(JustifyContent::Center),
        AlignFlags::SPACE_BETWEEN => Ok(JustifyContent::SpaceBetween),
        AlignFlags::SPACE_AROUND => Ok(JustifyContent::SpaceAround),
        AlignFlags::SPACE_EVENLY => Ok(JustifyContent::SpaceEvenly),
        _ => unsupported_flex(node, "justify-content", value),
    }
}

fn translate_align_items(node: NodeId, flags: AlignFlags) -> Result<AlignItems, StyleAdapterError> {
    if !flags.flags().is_empty() {
        return unsupported_flex(node, "align-items", flags);
    }
    match flags.value() {
        AlignFlags::NORMAL | AlignFlags::STRETCH => Ok(AlignItems::Stretch),
        AlignFlags::START | AlignFlags::FLEX_START => Ok(AlignItems::Start),
        AlignFlags::END | AlignFlags::FLEX_END => Ok(AlignItems::End),
        AlignFlags::CENTER => Ok(AlignItems::Center),
        _ => unsupported_flex(node, "align-items", flags),
    }
}

fn translate_align_self(node: NodeId, flags: AlignFlags) -> Result<AlignSelf, StyleAdapterError> {
    if !flags.flags().is_empty() {
        return unsupported_flex(node, "align-self", flags);
    }
    match flags.value() {
        AlignFlags::AUTO => Ok(AlignSelf::Auto),
        AlignFlags::NORMAL | AlignFlags::STRETCH => Ok(AlignSelf::Stretch),
        AlignFlags::START | AlignFlags::FLEX_START => Ok(AlignSelf::Start),
        AlignFlags::END | AlignFlags::FLEX_END => Ok(AlignSelf::End),
        AlignFlags::CENTER => Ok(AlignSelf::Center),
        _ => unsupported_flex(node, "align-self", flags),
    }
}

fn unsupported_flex<T: std::fmt::Debug, R>(
    node: NodeId,
    property: &'static str,
    value: T,
) -> Result<R, StyleAdapterError> {
    Err(StyleAdapterError::UnsupportedComputedValue {
        node,
        value: UnsupportedComputedValue::Flex(property, format!("{value:?}")),
    })
}

fn translate_margin(
    node: NodeId,
    property: &'static str,
    value: Margin,
) -> Result<TranslatedMargin, StyleAdapterError> {
    match value {
        GenericMargin::LengthPercentage(value) => {
            let (absolute, percentage) = translate_length_percentage(node, property, &value)?;
            Ok(TranslatedMargin {
                absolute,
                percentage,
                automatic: false,
            })
        }
        GenericMargin::Auto => Ok(TranslatedMargin {
            absolute: Au::ZERO,
            percentage: 0,
            automatic: true,
        }),
        GenericMargin::AnchorSizeFunction(_) | GenericMargin::AnchorContainingCalcFunction(_) => {
            Err(StyleAdapterError::UnsupportedComputedValue {
                node,
                value: UnsupportedComputedValue::LengthPercentage(property),
            })
        }
    }
}

fn translate_length_percentage(
    node: NodeId,
    property: &'static str,
    value: &LengthPercentage,
) -> Result<(Au, i32), StyleAdapterError> {
    match value.unpack() {
        Unpacked::Length(length) => Ok((translate_px(node, property, length)?, 0)),
        Unpacked::Percentage(percentage) => Ok((
            Au::ZERO,
            translate_percentage(node, property, percentage.0)?,
        )),
        // Non-linear min/max/clamp calculations cannot be represented by the
        // current `(length, percentage)` pair without inventing used values.
        Unpacked::Calc(_) => Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        }),
    }
}

fn translate_px(
    node: NodeId,
    property: &'static str,
    value: Length,
) -> Result<Au, StyleAdapterError> {
    let px = value.px();
    let raw = f64::from(px) * f64::from(Au::PER_CSS_PX);
    if !px.is_finite() || raw < f64::from(i32::MIN) || raw > f64::from(i32::MAX) {
        return Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        });
    }
    let raw = raw
        .round()
        .to_i32()
        .ok_or(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        })?;
    Ok(Au::from_raw(raw))
}

fn translate_percentage(
    node: NodeId,
    property: &'static str,
    value: f32,
) -> Result<i32, StyleAdapterError> {
    let fixed = f64::from(value) * f64::from(PercentageEdges::ONE_HUNDRED_PERCENT);
    if !value.is_finite() || fixed < f64::from(i32::MIN) || fixed > f64::from(i32::MAX) {
        return Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        });
    }
    fixed
        .round()
        .to_i32()
        .ok_or(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        })
}

fn translate_scaled_au(
    node: NodeId,
    property: &'static str,
    basis: Au,
    factor: f32,
) -> Result<Au, StyleAdapterError> {
    let raw = f64::from(basis.raw()) * f64::from(factor);
    if !factor.is_finite() || raw < f64::from(i32::MIN) || raw > f64::from(i32::MAX) {
        return Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        });
    }
    let raw = raw
        .round()
        .to_i32()
        .ok_or(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::LengthPercentage(property),
        })?;
    Ok(Au::from_raw(raw))
}

fn translate_border(style: BorderStyle, width: app_units::Au) -> Au {
    if style.none_or_hidden() {
        Au::ZERO
    } else {
        Au::from_raw(width.0)
    }
}

fn translate_color(
    node: NodeId,
    property: &'static str,
    value: style::color::AbsoluteColor,
) -> Result<Color, StyleAdapterError> {
    let components = value.raw_components();
    if !components.iter().all(|component| component.is_finite()) {
        return Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::Color(property),
        });
    }
    let [red, green, blue, alpha] = value.to_nscolor().to_le_bytes();
    Ok(Color {
        red,
        green,
        blue,
        alpha,
    })
}

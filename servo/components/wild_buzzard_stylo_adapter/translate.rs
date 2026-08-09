/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Loss-checked projection from Stylo computed values into wave-two layout values.

use num_traits::ToPrimitive;
use style::properties::longhands::{box_sizing, text_wrap_mode, white_space_collapse};
use style::properties::ComputedValues;
use style::values::computed::length_percentage::Unpacked;
use style::values::computed::{
    BorderStyle, Display as StyloDisplay, Length, LengthPercentage, LineHeight, Margin, MaxSize,
    Size, WritingModeProperty,
};
use style::values::generics::length::{GenericMargin, GenericMaxSize, GenericSize};
use wild_buzzard_dom::NodeId;
use wild_buzzard_layout::{
    Au, BoxSizing, Color, ComputedStyle, Display, Edges,
    LengthPercentage as LayoutLengthPercentage, MaxSizeValue, PercentageEdges, SizeValue,
    WhiteSpace, WritingMode,
};

use crate::error::{StyleAdapterError, UnsupportedComputedValue};

pub(crate) fn translate_computed_style(
    node: NodeId,
    values: &ComputedValues,
) -> Result<ComputedStyle, StyleAdapterError> {
    let display = translate_display(node, values.clone_display())?;
    let margin = translate_margin_edges(node, values)?;
    let padding = translate_padding_edges(node, values)?;
    let sizing = translate_sizing(node, values)?;
    let text = translate_text(node, values)?;
    let colors = translate_colors(node, values)?;

    Ok(ComputedStyle {
        display,
        margin: margin.absolute,
        margin_percentage: margin.percentage,
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
        font_size: text.font_size,
        line_height: text.line_height,
        color: colors.foreground,
        background_color: colors.background,
        white_space: text.white_space,
    })
}

struct TranslatedEdges {
    absolute: Edges,
    percentage: PercentageEdges,
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
) -> Result<TranslatedEdges, StyleAdapterError> {
    let top = translate_margin(node, "margin-top", values.clone_margin_top())?;
    let right = translate_margin(node, "margin-right", values.clone_margin_right())?;
    let bottom = translate_margin(node, "margin-bottom", values.clone_margin_bottom())?;
    let left = translate_margin(node, "margin-left", values.clone_margin_left())?;
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
    } else {
        Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::Display(format!("{value:?}")),
        })
    }
}

fn translate_margin(
    node: NodeId,
    property: &'static str,
    value: Margin,
) -> Result<(Au, i32), StyleAdapterError> {
    match value {
        GenericMargin::LengthPercentage(value) => {
            translate_length_percentage(node, property, &value)
        }
        GenericMargin::Auto => Err(StyleAdapterError::UnsupportedComputedValue {
            node,
            value: UnsupportedComputedValue::AutomaticMargin(property),
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

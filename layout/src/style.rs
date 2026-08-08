use wild_buzzard_dom::{ElementData, NodeId, SnapshotNode};

use crate::geometry::{Au, Edges};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Display {
    None,
    Block,
    Inline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WhiteSpace {
    Normal,
    Pre,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 255,
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub margin: Edges,
    pub border: Edges,
    pub padding: Edges,
    pub font_size: Au,
    pub line_height: Au,
    pub color: Color,
    pub background_color: Color,
    pub white_space: WhiteSpace,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        let font_size = Au::from_px(16);
        Self {
            display: Display::Inline,
            margin: Edges::default(),
            border: Edges::default(),
            padding: Edges::default(),
            font_size,
            line_height: font_size.scale(6, 5),
            color: Color::BLACK,
            background_color: Color::TRANSPARENT,
            white_space: WhiteSpace::Normal,
        }
    }
}

impl ComputedStyle {
    pub fn inherit_from(parent: Option<&Self>) -> Self {
        let Some(parent) = parent else {
            return Self::default();
        };
        Self {
            font_size: parent.font_size,
            line_height: parent.line_height,
            color: parent.color,
            white_space: parent.white_space,
            ..Self::default()
        }
    }
}

/// Immutable input presented to a style-system adapter.
pub struct StyleInput<'a> {
    pub node_id: NodeId,
    pub node: &'a SnapshotNode,
    pub element: &'a ElementData,
    pub parent_style: Option<&'a ComputedStyle>,
}

/// Boundary that Stylo can implement without receiving mutable DOM ownership.
pub trait StyleResolver: Send + Sync {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle;
}

/// Small deterministic UA-style baseline used before Stylo is adapted.
#[derive(Clone, Copy, Debug, Default)]
pub struct InitialStyleResolver;

impl StyleResolver for InitialStyleResolver {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let mut style = ComputedStyle::inherit_from(input.parent_style);
        let name = input.element.name.local_name.as_str();
        style.display = if input.element.html_attribute("hidden").is_some()
            || matches!(
                name,
                "head"
                    | "base"
                    | "basefont"
                    | "bgsound"
                    | "link"
                    | "meta"
                    | "title"
                    | "style"
                    | "script"
                    | "template"
            ) {
            Display::None
        } else if is_ua_block(name) {
            Display::Block
        } else {
            Display::Inline
        };

        match name {
            "body" => style.margin = Edges::all(Au::from_px(8)),
            "p" => {
                style.margin.top = Au::from_px(16);
                style.margin.bottom = Au::from_px(16);
            }
            "blockquote" => {
                style.margin = Edges {
                    top: Au::from_px(16),
                    right: Au::from_px(40),
                    bottom: Au::from_px(16),
                    left: Au::from_px(40),
                };
            }
            "pre" => {
                style.margin.top = Au::from_px(16);
                style.margin.bottom = Au::from_px(16);
                style.white_space = WhiteSpace::Pre;
            }
            "h1" => set_heading(&mut style, 2, 1, 11, 16),
            "h2" => set_heading(&mut style, 3, 2, 10, 12),
            "h3" => set_heading(&mut style, 6, 5, 8, 8),
            "h4" => set_heading(&mut style, 1, 1, 8, 8),
            "h5" => set_heading(&mut style, 5, 6, 11, 11),
            "h6" => set_heading(&mut style, 2, 3, 12, 12),
            _ => {}
        }
        style
    }
}

fn set_heading(
    style: &mut ComputedStyle,
    numerator: i32,
    denominator: i32,
    margin_top_px: i32,
    margin_bottom_px: i32,
) {
    style.font_size = Au::from_px(16).scale(numerator, denominator);
    style.line_height = style.font_size.scale(6, 5);
    style.margin.top = Au::from_px(margin_top_px);
    style.margin.bottom = Au::from_px(margin_bottom_px);
}

fn is_ua_block(name: &str) -> bool {
    matches!(
        name,
        "html"
            | "body"
            | "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "dd"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "ul"
            | "li"
    )
}

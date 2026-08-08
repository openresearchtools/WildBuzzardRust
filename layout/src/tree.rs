use std::fmt;

use wild_buzzard_dom::{DocumentSnapshot, NodeId, NodeKind};

use crate::geometry::{Au, Rect, Size, Viewport};
use crate::style::{ComputedStyle, Display, StyleInput, StyleResolver, WhiteSpace};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BoxId(u32);

impl BoxId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoxKind {
    Block,
    Inline,
    Text,
    LineBreak,
    AnonymousBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fragment {
    pub rect: Rect,
    pub baseline: Option<Au>,
    /// Present only for shaped text fragments. Graphics can replace this with
    /// glyph runs once a font backend implements `TextMeasurer`.
    pub text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutBox {
    pub id: BoxId,
    pub node_id: Option<NodeId>,
    pub kind: BoxKind,
    pub style: ComputedStyle,
    pub fragments: Vec<Fragment>,
    pub children: Vec<BoxId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutWarningCode {
    BlockInsideInlineTreatedAsInline,
    InlineEdgesNotApplied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutWarning {
    pub node_id: Option<NodeId>,
    pub code: LayoutWarningCode,
}

/// Recursion bounds for untrusted or script-created document snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutLimits {
    /// Maximum logical depth accepted during box construction and layout.
    pub max_tree_depth: usize,
}

impl Default for LayoutLimits {
    fn default() -> Self {
        Self {
            max_tree_depth: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutPhase {
    BoxConstruction,
    BlockLayout,
    InlineLayout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    InvalidViewport,
    MissingSnapshotNode(NodeId),
    BoxCapacityExceeded,
    TreeDepthLimitExceeded {
        limit: usize,
        node_id: Option<NodeId>,
        phase: LayoutPhase,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidViewport => formatter.write_str("viewport dimensions must be positive"),
            Self::MissingSnapshotNode(node) => {
                write!(formatter, "snapshot is missing node slot {}", node.slot())
            }
            Self::BoxCapacityExceeded => formatter.write_str("layout box capacity exceeded"),
            Self::TreeDepthLimitExceeded {
                limit,
                node_id,
                phase,
            } => write!(
                formatter,
                "layout tree depth exceeded limit {limit} during {phase:?} at node {node_id:?}"
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextMetrics {
    pub advance: Au,
    pub ascent: Au,
    pub descent: Au,
}

impl TextMetrics {
    pub fn height(self) -> Au {
        self.ascent + self.descent
    }
}

/// Font-system boundary. The layout crate never opens fonts or calls native APIs.
pub trait TextMeasurer: Send + Sync {
    fn measure(&self, text: &str, style: &ComputedStyle) -> TextMetrics;
}

/// Deterministic metrics for tests and early headless integration.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonospaceTextMeasurer;

impl TextMeasurer for MonospaceTextMeasurer {
    fn measure(&self, text: &str, style: &ComputedStyle) -> TextMetrics {
        let mut advance = Au::ZERO;
        for character in text.chars() {
            let character_advance = if character == '\t' {
                style.font_size.scale(2, 1)
            } else if character == '\n' || character == '\r' {
                Au::ZERO
            } else if character.is_ascii() {
                style.font_size.scale(1, 2)
            } else {
                style.font_size
            };
            advance += character_advance;
        }
        TextMetrics {
            advance,
            ascent: style.line_height.scale(4, 5),
            descent: style.line_height - style.line_height.scale(4, 5),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutOutput {
    pub document_revision: u64,
    pub viewport: Viewport,
    pub root: Option<BoxId>,
    pub boxes: Vec<LayoutBox>,
    pub content_size: Size,
    pub warnings: Vec<LayoutWarning>,
}

impl LayoutOutput {
    pub fn box_by_id(&self, id: BoxId) -> Option<&LayoutBox> {
        self.boxes.get(id.index())
    }

    pub fn boxes_for_node(&self, node: NodeId) -> impl Iterator<Item = &LayoutBox> {
        self.boxes
            .iter()
            .filter(move |layout_box| layout_box.node_id == Some(node))
    }
}

pub fn layout_document(
    snapshot: &DocumentSnapshot,
    viewport: Viewport,
    styles: &dyn StyleResolver,
    text: &dyn TextMeasurer,
) -> Result<LayoutOutput, LayoutError> {
    layout_document_with_limits(snapshot, viewport, styles, text, LayoutLimits::default())
}

pub fn layout_document_with_limits(
    snapshot: &DocumentSnapshot,
    viewport: Viewport,
    styles: &dyn StyleResolver,
    text: &dyn TextMeasurer,
    limits: LayoutLimits,
) -> Result<LayoutOutput, LayoutError> {
    if viewport.size.width <= Au::ZERO || viewport.size.height <= Au::ZERO {
        return Err(LayoutError::InvalidViewport);
    }
    let mut engine = LayoutEngine {
        snapshot,
        boxes: Vec::new(),
        warnings: Vec::new(),
        styles,
        text,
        limits,
    };
    let root = snapshot
        .document_element()
        .map(|node| engine.build_node(node, None, 1))
        .transpose()?
        .flatten();
    let laid_out_height = if let Some(root) = root {
        engine.layout_block(root, Au::ZERO, Au::ZERO, viewport.size.width, 1)?
    } else {
        Au::ZERO
    };
    Ok(LayoutOutput {
        document_revision: snapshot.revision(),
        viewport,
        root,
        boxes: engine.boxes,
        content_size: Size {
            width: viewport.size.width,
            height: viewport.size.height.max(laid_out_height),
        },
        warnings: engine.warnings,
    })
}

struct LayoutEngine<'a> {
    snapshot: &'a DocumentSnapshot,
    boxes: Vec<LayoutBox>,
    warnings: Vec<LayoutWarning>,
    styles: &'a dyn StyleResolver,
    text: &'a dyn TextMeasurer,
    limits: LayoutLimits,
}

impl LayoutEngine<'_> {
    fn build_node(
        &mut self,
        node_id: NodeId,
        parent_style: Option<&ComputedStyle>,
        depth: usize,
    ) -> Result<Option<BoxId>, LayoutError> {
        self.check_node_depth(node_id, depth, LayoutPhase::BoxConstruction)?;
        let node = self
            .snapshot
            .node(node_id)
            .ok_or(LayoutError::MissingSnapshotNode(node_id))?;
        match &node.kind {
            NodeKind::Element(element) => {
                let style = self.styles.resolve(StyleInput {
                    node_id,
                    node,
                    element,
                    parent_style,
                });
                if style.display == Display::None {
                    return Ok(None);
                }
                let kind = if element.name.local_name == "br" {
                    BoxKind::LineBreak
                } else {
                    match style.display {
                        Display::Block => BoxKind::Block,
                        Display::Inline => BoxKind::Inline,
                        Display::None => unreachable!(),
                    }
                };
                let id = self.allocate(Some(node_id), kind, style.clone())?;
                let mut children = Vec::new();
                for child in &node.children {
                    if let Some(child_box) =
                        self.build_node(*child, Some(&style), depth.saturating_add(1))?
                    {
                        children.push(child_box);
                    }
                }
                self.boxes[id.index()].children = children;
                if kind == BoxKind::Block {
                    self.wrap_inline_runs(id)?;
                }
                Ok(Some(id))
            }
            NodeKind::Text(data) if !data.is_empty() => {
                let style = ComputedStyle::inherit_from(parent_style);
                Ok(Some(self.allocate(Some(node_id), BoxKind::Text, style)?))
            }
            NodeKind::Document
            | NodeKind::DocumentType(_)
            | NodeKind::Comment(_)
            | NodeKind::Text(_) => Ok(None),
        }
    }

    fn allocate(
        &mut self,
        node_id: Option<NodeId>,
        kind: BoxKind,
        style: ComputedStyle,
    ) -> Result<BoxId, LayoutError> {
        let slot = u32::try_from(self.boxes.len()).map_err(|_| LayoutError::BoxCapacityExceeded)?;
        let id = BoxId(slot);
        self.boxes.push(LayoutBox {
            id,
            node_id,
            kind,
            style,
            fragments: Vec::new(),
            children: Vec::new(),
        });
        Ok(id)
    }

    fn check_node_depth(
        &self,
        node_id: NodeId,
        depth: usize,
        phase: LayoutPhase,
    ) -> Result<(), LayoutError> {
        if depth > self.limits.max_tree_depth {
            return Err(LayoutError::TreeDepthLimitExceeded {
                limit: self.limits.max_tree_depth,
                node_id: Some(node_id),
                phase,
            });
        }
        Ok(())
    }

    fn check_box_depth(
        &self,
        id: BoxId,
        depth: usize,
        phase: LayoutPhase,
    ) -> Result<(), LayoutError> {
        if depth > self.limits.max_tree_depth {
            return Err(LayoutError::TreeDepthLimitExceeded {
                limit: self.limits.max_tree_depth,
                node_id: self.boxes[id.index()].node_id,
                phase,
            });
        }
        Ok(())
    }

    fn wrap_inline_runs(&mut self, block: BoxId) -> Result<(), LayoutError> {
        let original = std::mem::take(&mut self.boxes[block.index()].children);
        let parent_style = self.boxes[block.index()].style.clone();
        let mut result = Vec::new();
        let mut run = Vec::new();
        for child in original {
            if self.boxes[child.index()].kind == BoxKind::Block {
                self.flush_inline_run(&mut run, &mut result, &parent_style)?;
                result.push(child);
            } else {
                run.push(child);
            }
        }
        self.flush_inline_run(&mut run, &mut result, &parent_style)?;
        self.boxes[block.index()].children = result;
        Ok(())
    }

    fn flush_inline_run(
        &mut self,
        run: &mut Vec<BoxId>,
        output: &mut Vec<BoxId>,
        parent_style: &ComputedStyle,
    ) -> Result<(), LayoutError> {
        if run.is_empty() {
            return Ok(());
        }
        let mut style = parent_style.clone();
        style.display = Display::Block;
        style.margin = Default::default();
        style.border = Default::default();
        style.padding = Default::default();
        let anonymous = self.allocate(None, BoxKind::AnonymousBlock, style)?;
        self.boxes[anonymous.index()].children = std::mem::take(run);
        output.push(anonymous);
        Ok(())
    }

    /// Returns the occupied outer block height, including margins.
    fn layout_block(
        &mut self,
        id: BoxId,
        containing_x: Au,
        containing_y: Au,
        available_width: Au,
        depth: usize,
    ) -> Result<Au, LayoutError> {
        self.check_box_depth(id, depth, LayoutPhase::BlockLayout)?;
        let style = self.boxes[id.index()].style.clone();
        let margin_width = style.margin.horizontal();
        let border_box_width = (available_width - margin_width).non_negative();
        let content_width =
            (border_box_width - style.border.horizontal() - style.padding.horizontal())
                .non_negative();
        let border_x = containing_x + style.margin.left;
        let border_y = containing_y + style.margin.top;
        let content_x = border_x + style.border.left + style.padding.left;
        let mut cursor_y = border_y + style.border.top + style.padding.top;
        let children = self.boxes[id.index()].children.clone();
        for child in children {
            let height = match self.boxes[child.index()].kind {
                BoxKind::Block => self.layout_block(
                    child,
                    content_x,
                    cursor_y,
                    content_width,
                    depth.saturating_add(1),
                )?,
                BoxKind::AnonymousBlock => self.layout_inline_context(
                    child,
                    content_x,
                    cursor_y,
                    content_width,
                    depth.saturating_add(1),
                )?,
                _ => self.layout_inline_context(
                    child,
                    content_x,
                    cursor_y,
                    content_width,
                    depth.saturating_add(1),
                )?,
            };
            cursor_y += height;
        }
        let content_height = cursor_y - (border_y + style.border.top + style.padding.top);
        let border_height = style.border.top
            + style.padding.top
            + content_height
            + style.padding.bottom
            + style.border.bottom;
        self.boxes[id.index()].fragments.push(Fragment {
            rect: Rect::new(border_x, border_y, border_box_width, border_height),
            baseline: None,
            text: None,
        });
        Ok(style.margin.top + border_height + style.margin.bottom)
    }

    fn layout_inline_context(
        &mut self,
        id: BoxId,
        x: Au,
        y: Au,
        available_width: Au,
        depth: usize,
    ) -> Result<Au, LayoutError> {
        self.check_box_depth(id, depth, LayoutPhase::InlineLayout)?;
        let default_line_height = self.boxes[id.index()].style.line_height;
        let mut cursor = InlineCursor::new(x, y, available_width, default_line_height);
        let (children, child_depth) = if self.boxes[id.index()].kind == BoxKind::AnonymousBlock {
            (
                self.boxes[id.index()].children.clone(),
                depth.saturating_add(1),
            )
        } else {
            (vec![id], depth)
        };
        for child in children {
            self.layout_inline_box(child, &mut cursor, &[], child_depth)?;
        }
        let height = cursor.finish_height();
        if self.boxes[id.index()].kind == BoxKind::AnonymousBlock {
            self.boxes[id.index()].fragments.push(Fragment {
                rect: Rect::new(x, y, available_width, height),
                baseline: None,
                text: None,
            });
        }
        Ok(height)
    }

    fn layout_inline_box(
        &mut self,
        id: BoxId,
        cursor: &mut InlineCursor,
        ancestors: &[BoxId],
        depth: usize,
    ) -> Result<(), LayoutError> {
        self.check_box_depth(id, depth, LayoutPhase::InlineLayout)?;
        let kind = self.boxes[id.index()].kind;
        match kind {
            BoxKind::Text => {
                let node_id = self.boxes[id.index()]
                    .node_id
                    .ok_or(LayoutError::BoxCapacityExceeded)?;
                let node = self
                    .snapshot
                    .node(node_id)
                    .ok_or(LayoutError::MissingSnapshotNode(node_id))?;
                let NodeKind::Text(data) = &node.kind else {
                    return Err(LayoutError::MissingSnapshotNode(node_id));
                };
                let style = self.boxes[id.index()].style.clone();
                self.layout_text(id, data, &style, cursor)?;
            }
            BoxKind::LineBreak => {
                self.boxes[id.index()].fragments.push(Fragment {
                    rect: Rect::new(cursor.x, cursor.y, Au::ZERO, cursor.default_line_height),
                    baseline: Some(cursor.default_line_height.scale(4, 5)),
                    text: None,
                });
                cursor.force_new_line();
            }
            BoxKind::Inline => {
                let style = self.boxes[id.index()].style.clone();
                if style.border != Default::default() || style.padding != Default::default() {
                    self.warnings.push(LayoutWarning {
                        node_id: self.boxes[id.index()].node_id,
                        code: LayoutWarningCode::InlineEdgesNotApplied,
                    });
                }
                let children = self.boxes[id.index()].children.clone();
                let mut nested_ancestors = ancestors.to_vec();
                nested_ancestors.push(id);
                for child in children {
                    self.layout_inline_box(
                        child,
                        cursor,
                        &nested_ancestors,
                        depth.saturating_add(1),
                    )?;
                }
                self.set_inline_fragments_from_children(id);
            }
            BoxKind::Block | BoxKind::AnonymousBlock => {
                self.warnings.push(LayoutWarning {
                    node_id: self.boxes[id.index()].node_id,
                    code: LayoutWarningCode::BlockInsideInlineTreatedAsInline,
                });
                let children = self.boxes[id.index()].children.clone();
                for child in children {
                    self.layout_inline_box(child, cursor, ancestors, depth.saturating_add(1))?;
                }
            }
        }
        Ok(())
    }

    fn layout_text(
        &mut self,
        id: BoxId,
        data: &str,
        style: &ComputedStyle,
        cursor: &mut InlineCursor,
    ) -> Result<(), LayoutError> {
        match style.white_space {
            WhiteSpace::Normal => {
                let mut word = String::new();
                for character in data.chars() {
                    if is_css_collapsible_whitespace(character) {
                        if !word.is_empty() {
                            self.place_wrappable_run(id, &word, style, cursor)?;
                            word.clear();
                        }
                        cursor.pending_space = cursor.line_has_content;
                    } else {
                        word.push(character);
                    }
                }
                if !word.is_empty() {
                    self.place_wrappable_run(id, &word, style, cursor)?;
                }
            }
            WhiteSpace::Pre => {
                let mut run = String::new();
                for character in data.chars() {
                    if character == '\n' {
                        if !run.is_empty() {
                            self.place_unbroken_run(id, &run, style, cursor);
                            run.clear();
                        }
                        cursor.force_new_line();
                    } else {
                        run.push(character);
                    }
                }
                if !run.is_empty() {
                    self.place_unbroken_run(id, &run, style, cursor);
                }
            }
        }
        Ok(())
    }

    fn place_wrappable_run(
        &mut self,
        id: BoxId,
        word: &str,
        style: &ComputedStyle,
        cursor: &mut InlineCursor,
    ) -> Result<(), LayoutError> {
        let prefix = if cursor.pending_space && cursor.line_has_content {
            " "
        } else {
            ""
        };
        let candidate = format!("{prefix}{word}");
        let metrics = self.text.measure(&candidate, style);
        if cursor.line_has_content && cursor.remaining_width() < metrics.advance {
            cursor.new_line();
            cursor.pending_space = false;
        }
        let candidate = if cursor.pending_space && cursor.line_has_content {
            format!(" {word}")
        } else {
            word.to_owned()
        };
        cursor.pending_space = false;

        if self.text.measure(&candidate, style).advance <= cursor.available_width
            || candidate.chars().count() <= 1
        {
            self.place_unbroken_run(id, &candidate, style, cursor);
            return Ok(());
        }

        let mut piece = String::new();
        for character in candidate.chars() {
            let mut next = piece.clone();
            next.push(character);
            let next_width = self.text.measure(&next, style).advance;
            if !piece.is_empty() && next_width > cursor.remaining_width() {
                self.place_unbroken_run(id, &piece, style, cursor);
                cursor.new_line();
                piece.clear();
            }
            piece.push(character);
        }
        if !piece.is_empty() {
            self.place_unbroken_run(id, &piece, style, cursor);
        }
        Ok(())
    }

    fn place_unbroken_run(
        &mut self,
        id: BoxId,
        run: &str,
        style: &ComputedStyle,
        cursor: &mut InlineCursor,
    ) {
        let metrics = self.text.measure(run, style);
        let fragment = Fragment {
            rect: Rect::new(cursor.x, cursor.y, metrics.advance, style.line_height),
            baseline: Some(metrics.ascent),
            text: Some(run.to_owned()),
        };
        self.boxes[id.index()].fragments.push(fragment);
        cursor.x += metrics.advance;
        cursor.line_height = cursor
            .line_height
            .max(style.line_height.max(metrics.height()));
        cursor.line_has_content = true;
        cursor.had_content = true;
    }

    fn set_inline_fragments_from_children(&mut self, id: BoxId) {
        let children = self.boxes[id.index()].children.clone();
        let mut fragments: Vec<Fragment> = Vec::new();
        for child in children {
            for child_fragment in self.boxes[child.index()].fragments.clone() {
                if child_fragment.text.is_none() && child_fragment.rect.size.width == Au::ZERO {
                    continue;
                }
                if let Some(existing) = fragments
                    .iter_mut()
                    .find(|fragment| fragment.rect.origin.y == child_fragment.rect.origin.y)
                {
                    existing.rect = existing.rect.union(child_fragment.rect);
                } else {
                    fragments.push(Fragment {
                        rect: child_fragment.rect,
                        baseline: child_fragment.baseline,
                        text: None,
                    });
                }
            }
        }
        self.boxes[id.index()].fragments = fragments;
    }
}

struct InlineCursor {
    start_x: Au,
    start_y: Au,
    x: Au,
    y: Au,
    available_width: Au,
    default_line_height: Au,
    line_height: Au,
    line_has_content: bool,
    had_content: bool,
    pending_space: bool,
}

/// Wave-one CSS white-space collapsing set: space, tab, and segment breaks.
/// Non-breaking and other Unicode spaces remain text and reach shaping.
fn is_css_collapsible_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{000c}' | '\r' | ' ')
}

impl InlineCursor {
    fn new(x: Au, y: Au, available_width: Au, default_line_height: Au) -> Self {
        Self {
            start_x: x,
            start_y: y,
            x,
            y,
            available_width,
            default_line_height,
            line_height: default_line_height,
            line_has_content: false,
            had_content: false,
            pending_space: false,
        }
    }

    fn remaining_width(&self) -> Au {
        (self.start_x + self.available_width - self.x).non_negative()
    }

    fn new_line(&mut self) {
        self.y += self.line_height;
        self.x = self.start_x;
        self.line_height = self.default_line_height;
        self.line_has_content = false;
        self.pending_space = false;
    }

    fn force_new_line(&mut self) {
        self.had_content = true;
        self.new_line();
    }

    fn finish_height(&self) -> Au {
        if !self.had_content {
            Au::ZERO
        } else if self.line_has_content {
            self.y - self.start_y + self.line_height
        } else {
            self.y - self.start_y
        }
    }
}

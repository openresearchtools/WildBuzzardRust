use std::fmt;

use wild_buzzard_dom::{DocumentId, DocumentSnapshot, DocumentVersion, NodeId, NodeKind};

use crate::flex::{FlexConstraints, FlexError, FlexItemInput, FlexWorkBudget, plan_flex_layout};
use crate::geometry::{Au, Edges, Rect, Size, Viewport};
use crate::style::{
    AlignItems, AlignSelf, BoxSizing, ComputedStyle, ComputedStyleSnapshot, Display, FlexBasis,
    FlexDirection, LengthPercentage, MaxSizeValue, SizeValue, StyleInput, StyleResolver,
    WhiteSpace, WritingMode,
};

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
    Flex,
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
    /// Maximum number of items admitted by any one flex container.
    pub max_flex_items: usize,
    /// Maximum number of lines admitted by any one flex container.
    pub max_flex_lines: usize,
    /// Aggregate item/pass work admitted across all flex containers.
    pub max_flex_work: usize,
}

impl Default for LayoutLimits {
    fn default() -> Self {
        Self {
            max_tree_depth: 256,
            max_flex_items: 4_096,
            max_flex_lines: 1_024,
            max_flex_work: 1_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutPhase {
    BoxConstruction,
    BlockLayout,
    InlineLayout,
    FlexLayout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    InvalidViewport,
    StyleDocumentMismatch {
        document: DocumentId,
        styles: DocumentId,
    },
    StyleRevisionMismatch {
        document_revision: u64,
        style_revision: u64,
    },
    MissingComputedStyle(NodeId),
    UnsupportedWritingMode {
        node: NodeId,
        writing_mode: WritingMode,
    },
    MissingSnapshotNode(NodeId),
    BoxCapacityExceeded,
    FlexItemLimitExceeded {
        limit: usize,
        actual: usize,
    },
    FlexLineLimitExceeded {
        limit: usize,
    },
    FlexWorkLimitExceeded {
        limit: usize,
    },
    FlexAllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    FlexArithmeticOverflow,
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
            Self::StyleDocumentMismatch { document, styles } => write!(
                formatter,
                "computed styles belong to document {}, but layout received document {}",
                styles.get(),
                document.get()
            ),
            Self::StyleRevisionMismatch {
                document_revision,
                style_revision,
            } => write!(
                formatter,
                "computed-style revision {style_revision} does not match document revision {document_revision}"
            ),
            Self::MissingComputedStyle(node) => {
                write!(
                    formatter,
                    "computed style is missing for node slot {}",
                    node.slot()
                )
            }
            Self::UnsupportedWritingMode { node, writing_mode } => write!(
                formatter,
                "layout does not yet support {writing_mode:?} for node slot {}",
                node.slot()
            ),
            Self::MissingSnapshotNode(node) => {
                write!(formatter, "snapshot is missing node slot {}", node.slot())
            }
            Self::BoxCapacityExceeded => formatter.write_str("layout box capacity exceeded"),
            Self::FlexItemLimitExceeded { limit, actual } => write!(
                formatter,
                "flex container has {actual} items; limit is {limit}"
            ),
            Self::FlexLineLimitExceeded { limit } => {
                write!(formatter, "flex line limit {limit} exceeded")
            }
            Self::FlexWorkLimitExceeded { limit } => {
                write!(formatter, "flex work limit {limit} exceeded")
            }
            Self::FlexAllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} entries for {resource}"
            ),
            Self::FlexArithmeticOverflow => {
                formatter.write_str("flex layout arithmetic overflowed")
            }
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

impl From<FlexError> for LayoutError {
    fn from(error: FlexError) -> Self {
        match error {
            FlexError::WorkLimitExceeded { limit } => Self::FlexWorkLimitExceeded { limit },
            FlexError::LineLimitExceeded { limit } => Self::FlexLineLimitExceeded { limit },
            FlexError::AllocationFailed {
                resource,
                requested,
            } => Self::FlexAllocationFailed {
                resource,
                requested,
            },
            FlexError::ArithmeticOverflow => Self::FlexArithmeticOverflow,
        }
    }
}

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
    pub document_version: DocumentVersion,
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
        styles: StyleSource::Resolver(styles),
        text,
        limits,
        flex_work: FlexWorkBudget::new(limits.max_flex_work),
    };
    let root = snapshot
        .document_element()
        .map(|node| engine.build_node(node, None, 1))
        .transpose()?
        .flatten();
    let laid_out_height = if let Some(root) = root {
        engine.layout_block(
            root,
            Au::ZERO,
            Au::ZERO,
            viewport.size.width,
            Some(viewport.size.height),
            1,
        )?
    } else {
        Au::ZERO
    };
    Ok(LayoutOutput {
        document_version: snapshot.version(),
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

/// Lays out one DOM revision using an immutable, revision-matched style publication.
pub fn layout_document_with_style_snapshot(
    snapshot: &DocumentSnapshot,
    viewport: Viewport,
    styles: &ComputedStyleSnapshot,
    text: &dyn TextMeasurer,
) -> Result<LayoutOutput, LayoutError> {
    layout_document_with_style_snapshot_and_limits(
        snapshot,
        viewport,
        styles,
        text,
        LayoutLimits::default(),
    )
}

pub fn layout_document_with_style_snapshot_and_limits(
    snapshot: &DocumentSnapshot,
    viewport: Viewport,
    styles: &ComputedStyleSnapshot,
    text: &dyn TextMeasurer,
    limits: LayoutLimits,
) -> Result<LayoutOutput, LayoutError> {
    if snapshot.document_id() != styles.document_id() {
        return Err(LayoutError::StyleDocumentMismatch {
            document: snapshot.document_id(),
            styles: styles.document_id(),
        });
    }
    if snapshot.version() != styles.document_version() {
        return Err(LayoutError::StyleRevisionMismatch {
            document_revision: snapshot.revision(),
            style_revision: styles.document_revision(),
        });
    }
    if viewport.size.width <= Au::ZERO || viewport.size.height <= Au::ZERO {
        return Err(LayoutError::InvalidViewport);
    }
    let mut engine = LayoutEngine {
        snapshot,
        boxes: Vec::new(),
        warnings: Vec::new(),
        styles: StyleSource::Snapshot(styles),
        text,
        limits,
        flex_work: FlexWorkBudget::new(limits.max_flex_work),
    };
    let root = snapshot
        .document_element()
        .map(|node| engine.build_node(node, None, 1))
        .transpose()?
        .flatten();
    let laid_out_height = if let Some(root) = root {
        engine.layout_block(
            root,
            Au::ZERO,
            Au::ZERO,
            viewport.size.width,
            Some(viewport.size.height),
            1,
        )?
    } else {
        Au::ZERO
    };
    Ok(LayoutOutput {
        document_version: snapshot.version(),
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

enum StyleSource<'a> {
    Resolver(&'a dyn StyleResolver),
    Snapshot(&'a ComputedStyleSnapshot),
}

struct LayoutEngine<'a> {
    snapshot: &'a DocumentSnapshot,
    boxes: Vec<LayoutBox>,
    warnings: Vec<LayoutWarning>,
    styles: StyleSource<'a>,
    text: &'a dyn TextMeasurer,
    limits: LayoutLimits,
    flex_work: FlexWorkBudget,
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
                let style = match self.styles {
                    StyleSource::Resolver(styles) => styles.resolve(StyleInput {
                        node_id,
                        node,
                        element,
                        parent_style,
                    }),
                    StyleSource::Snapshot(styles) => styles
                        .get(node_id)
                        .cloned()
                        .ok_or(LayoutError::MissingComputedStyle(node_id))?,
                };
                if style.display == Display::None {
                    return Ok(None);
                }
                if style.writing_mode != WritingMode::HorizontalTb {
                    return Err(LayoutError::UnsupportedWritingMode {
                        node: node_id,
                        writing_mode: style.writing_mode,
                    });
                }
                let kind = if element.name.local_name == "br" {
                    BoxKind::LineBreak
                } else {
                    match style.display {
                        Display::Block => BoxKind::Block,
                        Display::Flex => BoxKind::Flex,
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
                } else if kind == BoxKind::Flex {
                    self.prepare_flex_items(id)?;
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
            if matches!(
                self.boxes[child.index()].kind,
                BoxKind::Block | BoxKind::Flex
            ) {
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

    fn prepare_flex_items(&mut self, flex: BoxId) -> Result<(), LayoutError> {
        let original_len = self.boxes[flex.index()].children.len();
        self.flex_work.charge(original_len)?;
        let original = std::mem::take(&mut self.boxes[flex.index()].children);
        let parent_style = self.boxes[flex.index()].style.clone();
        let mut result = Vec::new();
        result.try_reserve_exact(original.len()).map_err(|_| {
            LayoutError::FlexAllocationFailed {
                resource: "flex child boxes",
                requested: original.len(),
            }
        })?;
        let mut text_run = Vec::new();
        text_run.try_reserve_exact(original.len()).map_err(|_| {
            LayoutError::FlexAllocationFailed {
                resource: "flex anonymous text run",
                requested: original.len(),
            }
        })?;
        for child in original {
            match self.boxes[child.index()].kind {
                BoxKind::Text | BoxKind::LineBreak => text_run.push(child),
                BoxKind::Inline => {
                    self.flush_flex_text_run(&mut text_run, &mut result, &parent_style)?;
                    // Flex items are blockified without changing their DOM or
                    // accessibility order.
                    self.boxes[child.index()].kind = BoxKind::Block;
                    self.boxes[child.index()].style.display = Display::Block;
                    self.wrap_inline_runs(child)?;
                    result.push(child);
                }
                BoxKind::Block | BoxKind::Flex | BoxKind::AnonymousBlock => {
                    self.flush_flex_text_run(&mut text_run, &mut result, &parent_style)?;
                    result.push(child);
                }
            }
        }
        self.flush_flex_text_run(&mut text_run, &mut result, &parent_style)?;
        if result.len() > self.limits.max_flex_items {
            return Err(LayoutError::FlexItemLimitExceeded {
                limit: self.limits.max_flex_items,
                actual: result.len(),
            });
        }
        self.boxes[flex.index()].children = result;
        Ok(())
    }

    fn flush_flex_text_run(
        &mut self,
        run: &mut Vec<BoxId>,
        output: &mut Vec<BoxId>,
        parent_style: &ComputedStyle,
    ) -> Result<(), LayoutError> {
        if run.is_empty() {
            return Ok(());
        }
        self.flex_work.charge(run.len())?;
        let renderable = run.iter().try_fold(false, |renderable, child| {
            let layout_box = &self.boxes[child.index()];
            if layout_box.kind == BoxKind::LineBreak {
                return Ok(true);
            }
            let node = layout_box
                .node_id
                .and_then(|node| self.snapshot.node(node))
                .ok_or_else(|| {
                    layout_box.node_id.map_or(
                        LayoutError::BoxCapacityExceeded,
                        LayoutError::MissingSnapshotNode,
                    )
                })?;
            let NodeKind::Text(data) = &node.kind else {
                return Err(LayoutError::MissingSnapshotNode(node.id));
            };
            Ok::<_, LayoutError>(
                renderable
                    || data
                        .chars()
                        .any(|character| !is_css_collapsible_whitespace(character)),
            )
        })?;
        if !renderable {
            run.clear();
            return Ok(());
        }
        self.flush_inline_run(run, output, parent_style)
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
        let mut style = ComputedStyle::inherit_from(Some(parent_style));
        style.display = Display::Block;
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
        containing_height: Option<Au>,
        depth: usize,
    ) -> Result<Au, LayoutError> {
        self.layout_block_sized(
            id,
            containing_x,
            containing_y,
            available_width,
            containing_height,
            None,
            None,
            depth,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_block_sized(
        &mut self,
        id: BoxId,
        containing_x: Au,
        containing_y: Au,
        available_width: Au,
        containing_height: Option<Au>,
        forced_content_width: Option<Au>,
        forced_content_height: Option<Au>,
        depth: usize,
    ) -> Result<Au, LayoutError> {
        self.check_box_depth(id, depth, LayoutPhase::BlockLayout)?;
        let style = self.boxes[id.index()].style.clone();
        let mut margin = style.margin;
        let resolved_margin_percentage = style.margin_percentage.resolve(available_width);
        margin.top += resolved_margin_percentage.top;
        margin.right += resolved_margin_percentage.right;
        margin.bottom += resolved_margin_percentage.bottom;
        margin.left += resolved_margin_percentage.left;
        let mut padding = style.padding;
        let resolved_padding_percentage = style.padding_percentage.resolve(available_width);
        padding.top += resolved_padding_percentage.top;
        padding.right += resolved_padding_percentage.right;
        padding.bottom += resolved_padding_percentage.bottom;
        padding.left += resolved_padding_percentage.left;
        let margin_width = margin.horizontal();
        let border_and_padding_width = style.border.horizontal() + padding.horizontal();
        let available_content_width =
            (available_width - margin_width - border_and_padding_width).non_negative();
        let preferred_width = resolve_content_box_preferred_size(
            style.width,
            Some(available_width),
            style.box_sizing,
            border_and_padding_width,
        );
        let content_width = forced_content_width.unwrap_or_else(|| {
            constrain_content_box_size(
                preferred_width.unwrap_or(available_content_width),
                style.min_width,
                style.max_width,
                Some(available_width),
                style.box_sizing,
                border_and_padding_width,
            )
        });
        let border_box_width = content_width + border_and_padding_width;
        let border_and_padding_height = style.border.vertical() + padding.vertical();
        let definite_content_height = forced_content_height.or_else(|| {
            resolve_content_box_preferred_size(
                style.height,
                containing_height,
                style.box_sizing,
                border_and_padding_height,
            )
            .map(|height| {
                constrain_content_box_size(
                    height,
                    style.min_height,
                    style.max_height,
                    containing_height,
                    style.box_sizing,
                    border_and_padding_height,
                )
            })
        });
        let border_x = containing_x + margin.left;
        let border_y = containing_y + margin.top;
        let content_x = border_x + style.border.left + padding.left;
        let content_y = border_y + style.border.top + padding.top;
        let natural_content_height = if self.boxes[id.index()].kind == BoxKind::Flex {
            self.layout_flex_children(
                id,
                content_x,
                content_y,
                content_width,
                definite_content_height,
                depth.saturating_add(1),
            )?
        } else {
            let mut cursor_y = content_y;
            let children = self.boxes[id.index()].children.clone();
            for child in children {
                let height = match self.boxes[child.index()].kind {
                    BoxKind::Block | BoxKind::Flex => self.layout_block(
                        child,
                        content_x,
                        cursor_y,
                        content_width,
                        definite_content_height,
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
            cursor_y - content_y
        };
        let content_height = definite_content_height.unwrap_or_else(|| {
            constrain_content_box_size(
                natural_content_height,
                style.min_height,
                style.max_height,
                containing_height,
                style.box_sizing,
                border_and_padding_height,
            )
        });
        let border_height =
            style.border.top + padding.top + content_height + padding.bottom + style.border.bottom;
        self.boxes[id.index()].fragments.push(Fragment {
            rect: Rect::new(border_x, border_y, border_box_width, border_height),
            baseline: None,
            text: None,
        });
        Ok(margin.top + border_height + margin.bottom)
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_flex_children(
        &mut self,
        container: BoxId,
        content_x: Au,
        content_y: Au,
        content_width: Au,
        content_height: Option<Au>,
        depth: usize,
    ) -> Result<Au, LayoutError> {
        self.check_box_depth(container, depth, LayoutPhase::FlexLayout)?;
        let container_style = self.boxes[container.index()].style.clone();
        let child_count = self.boxes[container.index()].children.len();
        if child_count > self.limits.max_flex_items {
            return Err(LayoutError::FlexItemLimitExceeded {
                limit: self.limits.max_flex_items,
                actual: child_count,
            });
        }
        self.flex_work.charge(child_count)?;
        let children = self.cloned_flex_children(container, "flex container children")?;

        self.flex_work.charge(children.len())?;
        let mut inputs = Vec::new();
        inputs.try_reserve_exact(children.len()).map_err(|_| {
            LayoutError::FlexAllocationFailed {
                resource: "flex layout inputs",
                requested: children.len(),
            }
        })?;
        for (source_index, child) in children.iter().copied().enumerate() {
            inputs.push(self.flex_item_input(
                child,
                source_index,
                &container_style,
                content_width,
                content_height,
                depth.saturating_add(1),
            )?);
        }

        let direction = container_style.flex.direction;
        let (main_size, cross_size, main_gap, cross_gap) = match direction {
            FlexDirection::Row => (
                Some(content_width),
                content_height,
                resolve_flex_length_percentage(
                    container_style.flex.column_gap,
                    Some(content_width),
                )?
                .unwrap_or(Au::ZERO),
                resolve_indefinite_gap(container_style.flex.row_gap, content_height)?,
            ),
            FlexDirection::Column => (
                content_height,
                Some(content_width),
                resolve_indefinite_gap(container_style.flex.row_gap, content_height)?,
                resolve_flex_length_percentage(
                    container_style.flex.column_gap,
                    Some(content_width),
                )?
                .unwrap_or(Au::ZERO),
            ),
        };
        let constraints = FlexConstraints {
            main_size,
            cross_size,
            wrap: container_style.flex.wrap,
            main_gap,
            cross_gap,
            justify_content: container_style.flex.justify_content,
            max_lines: self.limits.max_flex_lines,
        };
        let mut plan = plan_flex_layout(&inputs, constraints, &mut self.flex_work)?;

        if direction == FlexDirection::Row {
            // The resolved flexed width can change an auto-height item's line
            // count. Charge admission for the complete item pass before the
            // first planner-input update; each intrinsic walk separately
            // charges its work. The duplicate planner then uses its ordinary
            // budgeted entry point.
            self.flex_work.charge(plan.placements.len())?;
            let mut remeasured_cross_size = false;
            for placement in &plan.placements {
                let source_index = placement.source_index;
                let cross_auto = inputs
                    .get(source_index)
                    .ok_or(LayoutError::FlexArithmeticOverflow)?
                    .cross_auto;
                if !cross_auto {
                    continue;
                }
                let child = *children
                    .get(source_index)
                    .ok_or(LayoutError::FlexArithmeticOverflow)?;
                let measured = self.estimate_content_size(
                    child,
                    placement.target_main,
                    depth.saturating_add(1),
                )?;
                inputs
                    .get_mut(source_index)
                    .ok_or(LayoutError::FlexArithmeticOverflow)?
                    .base_cross = measured.height;
                remeasured_cross_size = true;
            }
            if remeasured_cross_size {
                plan = plan_flex_layout(&inputs, constraints, &mut self.flex_work)?;
            }
        }

        // Charge the entire fragment-producing item pass before laying out its
        // first child, so exhaustion cannot publish a partial flex result.
        self.flex_work.charge(plan.placements.len())?;
        for placement in &plan.placements {
            let child = children[placement.source_index];
            let style = self.boxes[child.index()].style.clone();
            let (margin, padding) = resolve_physical_edges_checked(&style, content_width)?;
            let (outer_x, outer_y, forced_width, forced_height) = match direction {
                FlexDirection::Row => (
                    checked_au_sum(&[content_x, placement.outer_main_offset])?,
                    checked_au_sum(&[content_y, placement.outer_cross_offset])?,
                    placement.target_main,
                    placement.target_cross,
                ),
                FlexDirection::Column => (
                    checked_au_sum(&[content_x, placement.outer_cross_offset])?,
                    checked_au_sum(&[content_y, placement.outer_main_offset])?,
                    placement.target_cross,
                    placement.target_main,
                ),
            };
            let border_and_padding_width = checked_au_sum(&[
                style.border.left,
                style.border.right,
                padding.left,
                padding.right,
            ])?;
            let border_and_padding_height = checked_au_sum(&[
                style.border.top,
                style.border.bottom,
                padding.top,
                padding.bottom,
            ])?;
            checked_au_sum(&[forced_width, border_and_padding_width])?;
            checked_au_sum(&[forced_height, border_and_padding_height])?;
            let border_x = checked_au_sum(&[outer_x, margin.left])?;
            let border_y = checked_au_sum(&[outer_y, margin.top])?;
            checked_au_sum(&[border_x, style.border.left, padding.left])?;
            checked_au_sum(&[border_y, style.border.top, padding.top])?;
            checked_au_sum(&[
                margin.left,
                forced_width,
                border_and_padding_width,
                margin.right,
            ])?;
            checked_au_sum(&[
                margin.top,
                forced_height,
                border_and_padding_height,
                margin.bottom,
            ])?;
            match self.boxes[child.index()].kind {
                BoxKind::Block | BoxKind::Flex => {
                    self.layout_block_sized(
                        child,
                        outer_x,
                        outer_y,
                        content_width,
                        content_height,
                        Some(forced_width),
                        Some(forced_height),
                        depth.saturating_add(1),
                    )?;
                }
                BoxKind::AnonymousBlock => {
                    self.layout_inline_context(
                        child,
                        border_x,
                        border_y,
                        forced_width,
                        depth.saturating_add(1),
                    )?;
                    if let Some(fragment) = self.boxes[child.index()].fragments.last_mut() {
                        fragment.rect.size.width = forced_width;
                        fragment.rect.size.height = forced_height;
                    }
                }
                BoxKind::Inline | BoxKind::Text | BoxKind::LineBreak => {
                    return Err(LayoutError::FlexArithmeticOverflow);
                }
            }
        }

        Ok(match direction {
            FlexDirection::Row => plan.cross_extent,
            FlexDirection::Column => plan.main_extent,
        })
    }

    fn flex_item_input(
        &mut self,
        item: BoxId,
        source_index: usize,
        container_style: &ComputedStyle,
        containing_width: Au,
        containing_height: Option<Au>,
        depth: usize,
    ) -> Result<FlexItemInput, LayoutError> {
        self.check_box_depth(item, depth, LayoutPhase::FlexLayout)?;
        let style = self.boxes[item.index()].style.clone();
        let (margin, padding) = resolve_physical_edges_checked(&style, containing_width)?;
        let border_and_padding_width = checked_au_sum(&[
            style.border.left,
            style.border.right,
            padding.left,
            padding.right,
        ])?;
        let border_and_padding_height = checked_au_sum(&[
            style.border.top,
            style.border.bottom,
            padding.top,
            padding.bottom,
        ])?;
        let estimated = self.estimate_content_size(item, containing_width, depth)?;
        let preferred_width = resolve_content_box_preferred_size_checked(
            style.width,
            Some(containing_width),
            style.box_sizing,
            border_and_padding_width,
        )?;
        let preferred_height = resolve_content_box_preferred_size_checked(
            style.height,
            containing_height,
            style.box_sizing,
            border_and_padding_height,
        )?;
        let width = preferred_width.unwrap_or(estimated.width);
        let height = preferred_height.unwrap_or(estimated.height);

        let (axis_preferred, axis_estimated, axis_basis, axis_edges) =
            match container_style.flex.direction {
                FlexDirection::Row => (
                    preferred_width,
                    estimated.width,
                    Some(containing_width),
                    border_and_padding_width,
                ),
                FlexDirection::Column => (
                    preferred_height,
                    estimated.height,
                    containing_height,
                    border_and_padding_height,
                ),
            };
        let base_main = match style.flex.basis {
            FlexBasis::Auto => axis_preferred.unwrap_or(axis_estimated),
            FlexBasis::Content => axis_estimated,
            FlexBasis::LengthPercentage(value) => {
                if let Some(specified) = resolve_flex_length_percentage(value, axis_basis)? {
                    specified_to_content_box(specified, style.box_sizing, axis_edges)
                } else {
                    // A percentage flex basis with an indefinite main-size
                    // basis is `content`, not the item's preferred main size.
                    axis_estimated
                }
            }
        };

        let (
            min_main,
            max_main,
            base_cross,
            min_cross,
            max_cross,
            outer_main,
            outer_cross,
            cross_auto,
        ) = match container_style.flex.direction {
            FlexDirection::Row => (
                resolve_minimum(
                    style.min_width,
                    Some(containing_width),
                    style.box_sizing,
                    border_and_padding_width,
                )?,
                resolve_maximum(
                    style.max_width,
                    Some(containing_width),
                    style.box_sizing,
                    border_and_padding_width,
                )?,
                height,
                resolve_minimum(
                    style.min_height,
                    containing_height,
                    style.box_sizing,
                    border_and_padding_height,
                )?,
                resolve_maximum(
                    style.max_height,
                    containing_height,
                    style.box_sizing,
                    border_and_padding_height,
                )?,
                checked_au_sum(&[margin.left, margin.right, border_and_padding_width])?,
                checked_au_sum(&[margin.top, margin.bottom, border_and_padding_height])?,
                style.height == SizeValue::Auto,
            ),
            FlexDirection::Column => (
                resolve_minimum(
                    style.min_height,
                    containing_height,
                    style.box_sizing,
                    border_and_padding_height,
                )?,
                resolve_maximum(
                    style.max_height,
                    containing_height,
                    style.box_sizing,
                    border_and_padding_height,
                )?,
                width,
                resolve_minimum(
                    style.min_width,
                    Some(containing_width),
                    style.box_sizing,
                    border_and_padding_width,
                )?,
                resolve_maximum(
                    style.max_width,
                    Some(containing_width),
                    style.box_sizing,
                    border_and_padding_width,
                )?,
                checked_au_sum(&[margin.top, margin.bottom, border_and_padding_height])?,
                checked_au_sum(&[margin.left, margin.right, border_and_padding_width])?,
                style.width == SizeValue::Auto,
            ),
        };
        let align = match style.flex.align_self {
            AlignSelf::Auto => container_style.flex.align_items,
            AlignSelf::Stretch => AlignItems::Stretch,
            AlignSelf::Start => AlignItems::Start,
            AlignSelf::End => AlignItems::End,
            AlignSelf::Center => AlignItems::Center,
        };
        Ok(FlexItemInput {
            source_index,
            order: style.flex.order,
            base_main,
            min_main,
            max_main,
            grow: style.flex.grow,
            shrink: style.flex.shrink,
            outer_main,
            base_cross,
            min_cross,
            max_cross,
            outer_cross,
            cross_auto,
            align,
        })
    }

    fn estimate_content_size(
        &mut self,
        id: BoxId,
        available_width: Au,
        depth: usize,
    ) -> Result<Size, LayoutError> {
        self.check_box_depth(id, depth, LayoutPhase::FlexLayout)?;
        self.flex_work.charge(1)?;
        let layout_box = &self.boxes[id.index()];
        match layout_box.kind {
            BoxKind::Text => {
                let node_id = layout_box.node_id.ok_or(LayoutError::BoxCapacityExceeded)?;
                let node = self
                    .snapshot
                    .node(node_id)
                    .ok_or(LayoutError::MissingSnapshotNode(node_id))?;
                let NodeKind::Text(data) = &node.kind else {
                    return Err(LayoutError::MissingSnapshotNode(node_id));
                };
                let metrics = self.text.measure(data, &layout_box.style);
                Ok(Size {
                    width: metrics.advance,
                    height: if data.is_empty() {
                        Au::ZERO
                    } else {
                        layout_box.style.line_height.max(metrics.height())
                    },
                })
            }
            BoxKind::LineBreak => Ok(Size {
                width: Au::ZERO,
                height: layout_box.style.line_height,
            }),
            BoxKind::Inline | BoxKind::AnonymousBlock => {
                let child_count = layout_box.children.len();
                self.flex_work.charge(child_count)?;
                let children = self.cloned_flex_children(id, "flex intrinsic inline children")?;
                let mut width = Au::ZERO;
                let mut line_height = Au::ZERO;
                for child in children {
                    let size =
                        self.estimate_outer_size(child, available_width, depth.saturating_add(1))?;
                    width = checked_au_sum(&[width, size.width])?;
                    line_height = line_height.max(size.height);
                }
                let lines = if width > Au::ZERO && available_width > Au::ZERO {
                    i64::from(width.raw())
                        .checked_add(i64::from(available_width.raw()) - 1)
                        .ok_or(LayoutError::FlexArithmeticOverflow)?
                        / i64::from(available_width.raw())
                } else {
                    1
                };
                Ok(Size {
                    width,
                    height: checked_au_mul(line_height, lines)?,
                })
            }
            BoxKind::Block | BoxKind::Flex => {
                let child_count = layout_box.children.len();
                self.flex_work.charge(child_count)?;
                let children = self.cloned_flex_children(id, "flex intrinsic block children")?;
                let direction = layout_box.style.flex.direction;
                let is_row_flex =
                    layout_box.kind == BoxKind::Flex && direction == FlexDirection::Row;
                let mut width = Au::ZERO;
                let mut height = Au::ZERO;
                for child in children {
                    let size =
                        self.estimate_outer_size(child, available_width, depth.saturating_add(1))?;
                    if is_row_flex {
                        width = checked_au_sum(&[width, size.width])?;
                        height = height.max(size.height);
                    } else {
                        width = width.max(size.width);
                        height = checked_au_sum(&[height, size.height])?;
                    }
                }
                Ok(Size { width, height })
            }
        }
    }

    fn estimate_outer_size(
        &mut self,
        id: BoxId,
        available_width: Au,
        depth: usize,
    ) -> Result<Size, LayoutError> {
        let style = self.boxes[id.index()].style.clone();
        let (margin, padding) = resolve_physical_edges_checked(&style, available_width)?;
        let border_and_padding_width = checked_au_sum(&[
            style.border.left,
            style.border.right,
            padding.left,
            padding.right,
        ])?;
        let border_and_padding_height = checked_au_sum(&[
            style.border.top,
            style.border.bottom,
            padding.top,
            padding.bottom,
        ])?;
        let intrinsic = self.estimate_content_size(id, available_width, depth)?;
        let width = resolve_content_box_preferred_size_checked(
            style.width,
            Some(available_width),
            style.box_sizing,
            border_and_padding_width,
        )?
        .unwrap_or(intrinsic.width);
        let height = resolve_content_box_preferred_size_checked(
            style.height,
            None,
            style.box_sizing,
            border_and_padding_height,
        )?
        .unwrap_or(intrinsic.height);
        Ok(Size {
            width: checked_au_sum(&[width, border_and_padding_width, margin.left, margin.right])?
                .non_negative(),
            height: checked_au_sum(&[
                height,
                border_and_padding_height,
                margin.top,
                margin.bottom,
            ])?
            .non_negative(),
        })
    }

    fn cloned_flex_children(
        &self,
        id: BoxId,
        resource: &'static str,
    ) -> Result<Vec<BoxId>, LayoutError> {
        let source = &self.boxes[id.index()].children;
        let mut children = Vec::new();
        children.try_reserve_exact(source.len()).map_err(|_| {
            LayoutError::FlexAllocationFailed {
                resource,
                requested: source.len(),
            }
        })?;
        children.extend_from_slice(source);
        Ok(children)
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
                if style.margin != Default::default()
                    || style.margin_percentage != Default::default()
                    || style.border != Default::default()
                    || style.padding != Default::default()
                    || style.padding_percentage != Default::default()
                {
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
            BoxKind::Block | BoxKind::Flex | BoxKind::AnonymousBlock => {
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

fn resolve_content_box_preferred_size(
    value: SizeValue,
    percentage_basis: Option<Au>,
    box_sizing: BoxSizing,
    border_and_padding: Au,
) -> Option<Au> {
    value
        .resolve_optional(percentage_basis)
        .map(|value| specified_to_content_box(value, box_sizing, border_and_padding))
}

fn constrain_content_box_size(
    tentative: Au,
    minimum: SizeValue,
    maximum: MaxSizeValue,
    percentage_basis: Option<Au>,
    box_sizing: BoxSizing,
    border_and_padding: Au,
) -> Au {
    let minimum = resolve_content_box_preferred_size(
        minimum,
        percentage_basis,
        box_sizing,
        border_and_padding,
    );
    let maximum = maximum
        .resolve_optional(percentage_basis)
        .map(|value| specified_to_content_box(value, box_sizing, border_and_padding));
    let mut used = tentative.non_negative();
    if let Some(maximum) = maximum {
        used = used.min(maximum);
    }
    if let Some(minimum) = minimum {
        // CSS sizing gives the minimum precedence when min > max.
        used = used.max(minimum);
    }
    used
}

fn specified_to_content_box(specified: Au, box_sizing: BoxSizing, border_and_padding: Au) -> Au {
    match box_sizing {
        BoxSizing::ContentBox => specified.non_negative(),
        BoxSizing::BorderBox => (specified - border_and_padding).non_negative(),
    }
}

fn resolve_minimum(
    value: SizeValue,
    percentage_basis: Option<Au>,
    box_sizing: BoxSizing,
    border_and_padding: Au,
) -> Result<Au, LayoutError> {
    Ok(resolve_content_box_preferred_size_checked(
        value,
        percentage_basis,
        box_sizing,
        border_and_padding,
    )?
    .unwrap_or(Au::ZERO))
}

fn resolve_maximum(
    value: MaxSizeValue,
    percentage_basis: Option<Au>,
    box_sizing: BoxSizing,
    border_and_padding: Au,
) -> Result<Option<Au>, LayoutError> {
    Ok(match value {
        MaxSizeValue::None => None,
        MaxSizeValue::LengthPercentage(value) => {
            resolve_flex_length_percentage(value, percentage_basis)?
                .map(|value| specified_to_content_box(value, box_sizing, border_and_padding))
        }
    })
}

fn resolve_indefinite_gap(value: LengthPercentage, basis: Option<Au>) -> Result<Au, LayoutError> {
    Ok(resolve_flex_length_percentage(value, basis)?.unwrap_or(value.length.non_negative()))
}

fn resolve_physical_edges_checked(
    style: &ComputedStyle,
    containing_width: Au,
) -> Result<(Edges, Edges), LayoutError> {
    let resolve = |absolute: Au, percentage: i32| {
        checked_au_sum(&[absolute, checked_percentage(containing_width, percentage)?])
    };
    Ok((
        Edges {
            top: resolve(style.margin.top, style.margin_percentage.top)?,
            right: resolve(style.margin.right, style.margin_percentage.right)?,
            bottom: resolve(style.margin.bottom, style.margin_percentage.bottom)?,
            left: resolve(style.margin.left, style.margin_percentage.left)?,
        },
        Edges {
            top: resolve(style.padding.top, style.padding_percentage.top)?,
            right: resolve(style.padding.right, style.padding_percentage.right)?,
            bottom: resolve(style.padding.bottom, style.padding_percentage.bottom)?,
            left: resolve(style.padding.left, style.padding_percentage.left)?,
        },
    ))
}

fn resolve_content_box_preferred_size_checked(
    value: SizeValue,
    percentage_basis: Option<Au>,
    box_sizing: BoxSizing,
    border_and_padding: Au,
) -> Result<Option<Au>, LayoutError> {
    Ok(resolve_size_value_checked(value, percentage_basis)?
        .map(|value| specified_to_content_box(value, box_sizing, border_and_padding)))
}

fn resolve_size_value_checked(
    value: SizeValue,
    percentage_basis: Option<Au>,
) -> Result<Option<Au>, LayoutError> {
    match value {
        SizeValue::Auto => Ok(None),
        SizeValue::LengthPercentage(value) => {
            resolve_flex_length_percentage(value, percentage_basis)
        }
    }
}

fn resolve_flex_length_percentage(
    value: LengthPercentage,
    basis: Option<Au>,
) -> Result<Option<Au>, LayoutError> {
    if value.percentage == 0 {
        return Ok(Some(value.length.non_negative()));
    }
    let Some(basis) = basis else {
        return Ok(None);
    };
    Ok(Some(
        checked_au_sum(&[value.length, checked_percentage(basis, value.percentage)?])?
            .non_negative(),
    ))
}

fn checked_percentage(basis: Au, millionths: i32) -> Result<Au, LayoutError> {
    let scaled = i64::from(basis.raw())
        .checked_mul(i64::from(millionths))
        .ok_or(LayoutError::FlexArithmeticOverflow)?
        / i64::from(crate::style::PercentageEdges::ONE_HUNDRED_PERCENT);
    i32::try_from(scaled)
        .map(Au::from_raw)
        .map_err(|_| LayoutError::FlexArithmeticOverflow)
}

fn checked_au_sum(values: &[Au]) -> Result<Au, LayoutError> {
    let sum = values.iter().try_fold(0_i64, |sum, value| {
        sum.checked_add(i64::from(value.raw()))
            .ok_or(LayoutError::FlexArithmeticOverflow)
    })?;
    i32::try_from(sum)
        .map(Au::from_raw)
        .map_err(|_| LayoutError::FlexArithmeticOverflow)
}

fn checked_au_mul(value: Au, multiplier: i64) -> Result<Au, LayoutError> {
    let product = i64::from(value.raw())
        .checked_mul(multiplier)
        .ok_or(LayoutError::FlexArithmeticOverflow)?;
    i32::try_from(product)
        .map(Au::from_raw)
        .map_err(|_| LayoutError::FlexArithmeticOverflow)
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

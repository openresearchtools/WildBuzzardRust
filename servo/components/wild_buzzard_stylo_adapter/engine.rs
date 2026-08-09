/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;

use cssparser::{Parser, ParserInput, Token};
use euclid::{Scale, Size2D};
use num_traits::ToPrimitive;
use selectors::Element as _;
use servo_arc::Arc;
use style::animation::DocumentAnimationSet;
use style::context::{
    QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext,
    StyleContext, StyleSystemOptions, ThreadLocalStyleContext,
};
use style::custom_properties::AttrTaint;
use style::device::servo::FontMetricsProvider;
use style::device::Device;
use style::dom::TElement as _;
use style::error_reporting::{ContextualParseError, ParseErrorReporter};
use style::font_metrics::FontMetrics;
use style::matching::MatchMethods;
use style::media_queries::{MediaList, MediaType};
use style::parser::ParserContext;
use style::properties::style_structs::Font;
use style::properties::ComputedValues;
use style::queries::values::PrefersColorScheme;
use style::selector_parser::SnapshotMap;
use style::shared_lock::StylesheetGuards;
use style::style_resolver::{PseudoElementResolution, StyleResolverForElement};
use style::stylesheets::{
    AllowImportRules, DocumentStyleSheet, Namespaces, Origin, Stylesheet, UrlExtraData,
};
use style::stylist::{RuleInclusion, Stylist};
use style::traversal_flags::TraversalFlags;
use style::values::computed::font::GenericFontFamily;
use style::values::computed::{CSSPixelLength, Length};
use style::Atom;
use style_traits::{CSSPixel, DevicePixel, ParsingMode};
use url::Url;
use wild_buzzard_dom::{DocumentSnapshot, NodeId, NodeKind};
use wild_buzzard_layout::{ComputedStyleSnapshot, ComputedStyleSnapshotLimits};

use crate::embedding::AdapterSnapshot;
use crate::error::StyleAdapterError;
use crate::state::SelectorStateSnapshot;
use crate::translate::translate_computed_style;

const TEMPORARY_UA_CSS: &str = r"
html, body, address, article, aside, blockquote, div, footer, header, main,
nav, section, figure, figcaption, p, pre, h1, h2, h3, h4, h5, h6, ul, ol,
li, dl, dt, dd, form, fieldset { display: block; }
head, base, basefont, bgsound, link, meta, title, style, script, template,
[hidden] { display: none !important; }
body { margin: 8px; }
p, blockquote, pre { margin-block: 1em; }
pre { white-space: pre; }
";

/// Hard resource bounds applied before computed styles are published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleLimits {
    /// Maximum nodes in the immutable snapshot.
    pub max_nodes: usize,
    /// Maximum element side-table/style entries.
    pub max_style_entries: usize,
    /// Maximum DOM depth accepted by selector traversal.
    pub max_tree_depth: usize,
    /// Maximum attributes copied across the whole snapshot.
    pub max_attributes: usize,
    /// Maximum attributes copied for any one element.
    pub max_attributes_per_element: usize,
    /// Maximum UTF-8 bytes in one element or attribute name/namespace component.
    pub max_name_bytes: usize,
    /// Maximum UTF-8 bytes in one attribute value.
    pub max_attribute_value_bytes: usize,
    /// Maximum class tokens atomized across the whole snapshot.
    pub max_class_tokens: usize,
    /// Maximum aggregate UTF-8 bytes in class tokens.
    pub max_class_bytes: usize,
    /// Maximum UTF-8 bytes in one id or class token.
    pub max_identifier_bytes: usize,
    /// Maximum atoms constructed for names, namespaces, ids, and classes.
    pub max_atoms: usize,
    /// Maximum aggregate UTF-8 bytes examined or copied from the DOM snapshot.
    pub max_snapshot_string_bytes: usize,
    /// Maximum HTML author `<style>` elements.
    pub max_stylesheets: usize,
    /// Maximum aggregate UTF-8 bytes in author `<style>` elements.
    pub max_stylesheet_bytes: usize,
    /// Maximum aggregate UTF-8 bytes in inline style attributes.
    pub max_inline_style_bytes: usize,
    /// Maximum selectors admitted after Stylo parses all sheets.
    pub max_selectors: usize,
    /// Maximum declarations admitted after Stylo parses sheets and inline styles.
    pub max_declarations: usize,
    /// Maximum conservative node-squared selector work estimate.
    pub max_selector_work: usize,
    /// Maximum retained CSS parse diagnostics.
    pub max_diagnostics: usize,
    /// Maximum retained UTF-8 bytes per diagnostic message.
    pub max_diagnostic_bytes: usize,
}

impl Default for StyleLimits {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_style_entries: 10_000,
            max_tree_depth: 256,
            max_attributes: 65_536,
            max_attributes_per_element: 1_024,
            max_name_bytes: 4_096,
            max_attribute_value_bytes: 1_048_576,
            max_class_tokens: 65_536,
            max_class_bytes: 1_048_576,
            max_identifier_bytes: 65_536,
            max_atoms: 262_144,
            max_snapshot_string_bytes: 16_777_216,
            max_stylesheets: 64,
            max_stylesheet_bytes: 1_048_576,
            max_inline_style_bytes: 1_048_576,
            max_selectors: 8_192,
            max_declarations: 65_536,
            max_selector_work: 50_000_000,
            max_diagnostics: 256,
            max_diagnostic_bytes: 1_024,
        }
    }
}

/// Deterministic options for the static style preparation fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticStyleOptions {
    /// CSS-pixel viewport width supplied to Stylo media/value computation.
    pub viewport_width: u32,
    /// CSS-pixel viewport height supplied to Stylo media/value computation.
    pub viewport_height: u32,
    /// Resource bounds for this preparation.
    pub limits: StyleLimits,
}

impl Default for StaticStyleOptions {
    fn default() -> Self {
        Self {
            viewport_width: 800,
            viewport_height: 600,
            limits: StyleLimits::default(),
        }
    }
}

/// Bounded parse diagnostic produced by Stylo's standards error recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyleDiagnostic {
    /// Style element or inline-style owner, when available.
    pub node: Option<NodeId>,
    /// One-based parser line, or zero for an adapter policy diagnostic.
    pub line: u32,
    /// One-based parser column, or zero for an adapter policy diagnostic.
    pub column: u32,
    /// Bounded human-readable description.
    pub message: String,
}

#[derive(Debug)]
struct DiagnosticState {
    current_node: Option<NodeId>,
    diagnostics: Vec<StyleDiagnostic>,
    dropped: usize,
}

/// Thread-safe reporter used by the imported parser. Preparation itself is sequential.
#[derive(Debug)]
pub(crate) struct DiagnosticCollector {
    state: Mutex<DiagnosticState>,
    max_diagnostics: usize,
    max_message_bytes: usize,
}

#[derive(Debug)]
struct DeterministicFontMetrics;

impl FontMetricsProvider for DeterministicFontMetrics {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        _font: &Font,
        _base_size: CSSPixelLength,
        _flags: style::values::specified::font::QueryFontMetricsFlags,
    ) -> FontMetrics {
        FontMetrics::default()
    }

    fn base_size_for_generic(&self, _generic: GenericFontFamily) -> Length {
        Length::new(16.0)
    }
}

#[derive(Debug)]
struct NoRegisteredPainters;

impl RegisteredSpeculativePainters for NoRegisteredPainters {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

impl DiagnosticCollector {
    pub(crate) fn new(limits: StyleLimits) -> Self {
        Self {
            state: Mutex::new(DiagnosticState {
                current_node: None,
                diagnostics: Vec::new(),
                dropped: 0,
            }),
            max_diagnostics: limits.max_diagnostics,
            max_message_bytes: limits.max_diagnostic_bytes,
        }
    }

    pub(crate) fn set_current_node(&self, node: Option<NodeId>) {
        self.lock().current_node = node;
    }

    pub(crate) fn note(&self, node: NodeId, message: &str) {
        let mut state = self.lock();
        if state.diagnostics.len() >= self.max_diagnostics {
            state.dropped = state.dropped.saturating_add(1);
            return;
        }
        let mut message = message.to_owned();
        if message.len() > self.max_message_bytes {
            let mut boundary = self.max_message_bytes.min(message.len());
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message.truncate(boundary);
        }
        state.diagnostics.push(StyleDiagnostic {
            node: Some(node),
            line: 0,
            column: 0,
            message,
        });
    }

    pub(crate) fn finish(self) -> (Vec<StyleDiagnostic>, usize) {
        let state = self
            .state
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.diagnostics, state.dropped)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DiagnosticState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ParseErrorReporter for DiagnosticCollector {
    fn report_error(
        &self,
        _url: &UrlExtraData,
        location: style::values::SourceLocation,
        error: ContextualParseError,
    ) {
        let mut state = self.lock();
        if state.diagnostics.len() >= self.max_diagnostics {
            state.dropped = state.dropped.saturating_add(1);
            return;
        }
        let mut message = error.to_string();
        if message.len() > self.max_message_bytes {
            let mut boundary = self.max_message_bytes.min(message.len());
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message.truncate(boundary);
        }
        let node = state.current_node;
        state.diagnostics.push(StyleDiagnostic {
            node,
            line: location.line.saturating_add(1),
            column: location.column,
            message,
        });
    }
}

/// Owned Stylo and layout-facing results for one exact document revision.
pub struct ComputedStyloSnapshot {
    pub(crate) layout: ComputedStyleSnapshot,
    pub(crate) stylo_styles: HashMap<NodeId, Arc<ComputedValues>>,
    pub(crate) diagnostics: Vec<StyleDiagnostic>,
    pub(crate) dropped_diagnostics: usize,
}

impl ComputedStyloSnapshot {
    /// Immutable styles consumed by the current layout boundary.
    #[must_use]
    pub fn layout_styles(&self) -> &ComputedStyleSnapshot {
        &self.layout
    }

    /// Bounded recoverable CSS syntax diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[StyleDiagnostic] {
        &self.diagnostics
    }

    /// Parse or policy diagnostics omitted because the configured cap was reached.
    #[must_use]
    pub const fn dropped_diagnostic_count(&self) -> usize {
        self.dropped_diagnostics
    }

    /// Number of retained native Stylo `ComputedValues` entries.
    #[must_use]
    pub fn stylo_style_count(&self) -> usize {
        self.stylo_styles.len()
    }
}

/// Computes styles through the imported Stylo parser, matcher, and cascade.
///
/// # Errors
///
/// Returns [`StyleAdapterError`] when the immutable input violates a resource
/// bound or relationship invariant, imported-style preparation cannot be
/// represented safely, or the layout projection would lose required meaning.
pub fn prepare_computed_styles(
    snapshot: DocumentSnapshot,
    options: StaticStyleOptions,
) -> Result<ComputedStyloSnapshot, StyleAdapterError> {
    prepare_computed_styles_inner(snapshot, options, None)
}

/// Computes styles with selector-visible state supplied by the exact DOM
/// revision's event/form-state owner.
///
/// # Errors
///
/// Returns [`StyleAdapterError`] when state belongs to a different document or
/// revision, or when ordinary style preparation fails.
pub fn prepare_computed_styles_with_states(
    snapshot: DocumentSnapshot,
    options: StaticStyleOptions,
    states: &SelectorStateSnapshot,
) -> Result<ComputedStyloSnapshot, StyleAdapterError> {
    states.validate_for(&snapshot)?;
    prepare_computed_styles_inner(snapshot, options, Some(states))
}

fn prepare_computed_styles_inner(
    snapshot: DocumentSnapshot,
    options: StaticStyleOptions,
    states: Option<&SelectorStateSnapshot>,
) -> Result<ComputedStyloSnapshot, StyleAdapterError> {
    let diagnostics = DiagnosticCollector::new(options.limits);
    let url_data = UrlExtraData::from(
        Url::parse("https://wild-buzzard.invalid/static-document.css")
            .map_err(|_| StyleAdapterError::SnapshotInvariant("invalid built-in stylesheet URL"))?,
    );
    let shared_lock = style::shared_lock::SharedRwLock::new();
    let adapter = AdapterSnapshot::new(
        snapshot,
        shared_lock.clone(),
        &url_data,
        &diagnostics,
        options.limits,
        states,
    )?;
    let stylist = build_stylist(&adapter, &shared_lock, &url_data, &diagnostics, options)?;
    let native_styles = resolve_native_styles(&adapter, &stylist)?;
    let layout = project_layout_styles(&adapter, &native_styles, options.limits)?;
    let (diagnostics, dropped_diagnostics) = diagnostics.finish();
    Ok(ComputedStyloSnapshot {
        layout,
        stylo_styles: native_styles,
        diagnostics,
        dropped_diagnostics,
    })
}

fn build_stylist(
    adapter: &AdapterSnapshot,
    shared_lock: &style::shared_lock::SharedRwLock,
    url_data: &UrlExtraData,
    diagnostics: &DiagnosticCollector,
    options: StaticStyleOptions,
) -> Result<Stylist, StyleAdapterError> {
    let defaults = ComputedValues::initial_values_with_font_override(Font::initial_values());
    let device = Device::new(
        MediaType::screen(),
        QuirksMode::NoQuirks,
        Size2D::<f32, CSSPixel>::new(
            viewport_dimension(options.viewport_width, "viewport width")?,
            viewport_dimension(options.viewport_height, "viewport height")?,
        ),
        Scale::<f32, CSSPixel, DevicePixel>::new(1.0),
        Box::new(DeterministicFontMetrics),
        defaults,
        PrefersColorScheme::Light,
    );
    let mut stylist = Stylist::new(device, QuirksMode::NoQuirks);
    let ua_media = Arc::new(shared_lock.wrap(MediaList::empty()));
    let ua_sheet = Arc::new(Stylesheet::from_str(
        TEMPORARY_UA_CSS,
        url_data.clone(),
        Origin::UserAgent,
        ua_media,
        shared_lock.clone(),
        None,
        Some(diagnostics),
        QuirksMode::NoQuirks,
        AllowImportRules::No,
    ));
    let sheet_capacity = options.limits.max_stylesheets.saturating_add(1);
    let mut sheets = Vec::new();
    sheets
        .try_reserve_exact(sheet_capacity)
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "stylesheet list",
            requested: sheet_capacity,
        })?;
    sheets.push(DocumentStyleSheet(ua_sheet));
    collect_author_stylesheets(
        adapter,
        shared_lock,
        url_data,
        diagnostics,
        options.limits,
        &mut sheets,
    )?;
    let guard = shared_lock.read();
    for sheet in sheets {
        stylist.append_stylesheet(sheet, &guard);
    }
    stylist.flush(&StylesheetGuards::same(&guard));
    validate_style_work(adapter, &stylist, options.limits)?;
    Ok(stylist)
}

fn viewport_dimension(value: u32, resource: &'static str) -> Result<f32, StyleAdapterError> {
    const MAX_EXACT_CSS_PIXELS: u32 = 1 << f32::MANTISSA_DIGITS;
    if value > MAX_EXACT_CSS_PIXELS {
        let limit = MAX_EXACT_CSS_PIXELS
            .to_usize()
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        let actual = value
            .to_usize()
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        return Err(StyleAdapterError::SnapshotResourceLimitExceeded {
            resource,
            limit,
            actual,
        });
    }
    value
        .to_f32()
        .ok_or(StyleAdapterError::ResourceBoundOverflow)
}

fn validate_style_work(
    adapter: &AdapterSnapshot,
    stylist: &Stylist,
    limits: StyleLimits,
) -> Result<(), StyleAdapterError> {
    let selectors = stylist.num_selectors();
    if selectors > limits.max_selectors {
        return Err(StyleAdapterError::SelectorLimitExceeded {
            limit: limits.max_selectors,
            actual: selectors,
        });
    }
    let declarations = stylist
        .num_declarations()
        .checked_add(adapter.inline_declaration_count())
        .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
    if declarations > limits.max_declarations {
        return Err(StyleAdapterError::DeclarationLimitExceeded {
            limit: limits.max_declarations,
            actual: declarations,
        });
    }
    let selector_work = adapter
        .element_count()
        .checked_mul(adapter.element_count())
        .and_then(|work| work.checked_mul(selectors.max(1)))
        .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
    if selector_work > limits.max_selector_work {
        return Err(StyleAdapterError::SelectorWorkLimitExceeded {
            limit: limits.max_selector_work,
            estimated: selector_work,
        });
    }
    Ok(())
}

fn resolve_native_styles(
    adapter: &AdapterSnapshot,
    stylist: &Stylist,
) -> Result<HashMap<NodeId, Arc<ComputedValues>>, StyleAdapterError> {
    let _thread_role = LayoutThreadRole::acquire()?;
    let guard = adapter.shared_lock().read();
    let snapshot_map = SnapshotMap::new();
    let painters = NoRegisteredPainters;
    let shared_context = SharedStyleContext {
        stylist,
        visited_styles_enabled: false,
        options: StyleSystemOptions {
            disable_style_sharing_cache: true,
            dump_style_statistics: false,
            style_statistics_threshold: usize::MAX,
        },
        guards: StylesheetGuards::same(&guard),
        current_time_for_animations: 0.0,
        traversal_flags: TraversalFlags::empty(),
        snapshot_map: &snapshot_map,
        animations: DocumentAnimationSet::default(),
        registered_speculative_painters: &painters,
    };
    let mut thread_context = ThreadLocalStyleContext::new();
    let mut native_styles = HashMap::new();
    native_styles
        .try_reserve(adapter.element_count())
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "native computed-style map",
            requested: adapter.element_count(),
        })?;
    for element in adapter.elements() {
        thread_context
            .bloom_filter
            .insert_parents_recovering(element, element.depth());
        let parent_id = element
            .parent_element()
            .map(crate::embedding::StyleElement::source_id);
        let parent_style = parent_id.and_then(|parent| native_styles.get(&parent));
        let mut layout_parent = element.parent_element();
        while layout_parent.is_some_and(|parent| {
            native_styles
                .get(&parent.source_id())
                .is_some_and(|style: &Arc<ComputedValues>| style.is_display_contents())
        }) {
            layout_parent = layout_parent.and_then(|parent| parent.parent_element());
        }
        let layout_parent_style = layout_parent
            .and_then(|parent| native_styles.get(&parent.source_id()))
            .map(Arc::as_ref);
        let mut context = StyleContext {
            shared: &shared_context,
            thread_local: &mut thread_context,
        };
        let resolved = StyleResolverForElement::new(
            element,
            &mut context,
            RuleInclusion::All,
            PseudoElementResolution::IfApplicable,
        )
        .resolve_style(parent_style.map(Arc::as_ref), layout_parent_style);
        let primary = {
            let mut data = element
                .mutate_data()
                .ok_or(StyleAdapterError::SnapshotInvariant(
                    "Stylo element data disappeared during style resolution",
                ))?;
            element.finish_restyle(&mut context, &mut data, resolved, true);
            data.styles.primary().clone()
        };
        native_styles.insert(element.source_id(), primary);
    }
    Ok(native_styles)
}

struct LayoutThreadRole {
    added: style::thread_state::ThreadState,
}

impl LayoutThreadRole {
    fn acquire() -> Result<Self, StyleAdapterError> {
        let current = style::thread_state::get();
        if current.is_empty() {
            let added = style::thread_state::ThreadState::LAYOUT
                | style::thread_state::ThreadState::IN_WORKER;
            style::thread_state::enter(added);
            return Ok(Self { added });
        }
        if current.is_layout() {
            return Ok(Self {
                added: style::thread_state::ThreadState::empty(),
            });
        }
        Err(StyleAdapterError::IncompatibleThreadState {
            current: current.bits(),
        })
    }
}

impl Drop for LayoutThreadRole {
    fn drop(&mut self) {
        if !self.added.is_empty() {
            style::thread_state::exit(self.added);
        }
    }
}

fn project_layout_styles(
    adapter: &AdapterSnapshot,
    native_styles: &HashMap<NodeId, Arc<ComputedValues>>,
    limits: StyleLimits,
) -> Result<ComputedStyleSnapshot, StyleAdapterError> {
    let mut layout_entries = Vec::new();
    layout_entries
        .try_reserve_exact(adapter.element_count())
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "layout computed-style entries",
            requested: adapter.element_count(),
        })?;
    for element in adapter.elements() {
        let native =
            native_styles
                .get(&element.source_id())
                .ok_or(StyleAdapterError::SnapshotInvariant(
                    "Stylo omitted an element computed style",
                ))?;
        layout_entries.push((
            element.source_id(),
            translate_computed_style(element.source_id(), native)?,
        ));
    }
    ComputedStyleSnapshot::try_new(
        adapter.source(),
        layout_entries,
        ComputedStyleSnapshotLimits {
            max_entries: limits.max_style_entries,
        },
    )
    .map_err(StyleAdapterError::from)
}

fn collect_author_stylesheets(
    adapter: &AdapterSnapshot,
    shared_lock: &style::shared_lock::SharedRwLock,
    url_data: &UrlExtraData,
    diagnostics: &DiagnosticCollector,
    limits: StyleLimits,
    sheets: &mut Vec<DocumentStyleSheet>,
) -> Result<(), StyleAdapterError> {
    let mut stylesheet_count = 0_usize;
    let mut stylesheet_bytes = 0_usize;
    for node in adapter.source().nodes_in_document_order() {
        let NodeKind::Element(element) = &node.kind else {
            continue;
        };
        if element.name.local_name != "style" {
            continue;
        }
        if element.name.namespace.as_uri() != wild_buzzard_dom::Namespace::HTML_URI {
            diagnostics.note(node.id, "ignored a non-HTML style element");
            continue;
        }
        if element
            .html_attribute("type")
            .is_some_and(|kind| !kind.is_empty() && !kind.eq_ignore_ascii_case("text/css"))
        {
            diagnostics.note(node.id, "ignored a style element with a non-CSS type");
            continue;
        }
        stylesheet_count = stylesheet_count
            .checked_add(1)
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        if stylesheet_count > limits.max_stylesheets {
            return Err(StyleAdapterError::StylesheetLimitExceeded {
                limit: limits.max_stylesheets,
            });
        }

        let remaining_stylesheet_bytes = limits
            .max_stylesheet_bytes
            .checked_sub(stylesheet_bytes)
            .ok_or(StyleAdapterError::StylesheetByteLimitExceeded {
                limit: limits.max_stylesheet_bytes,
            })?;
        let css = collect_descendant_text(adapter, node.id, remaining_stylesheet_bytes, limits)?;
        stylesheet_bytes = stylesheet_bytes
            .checked_add(css.len())
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        if contains_top_level_import(&css) {
            return Err(StyleAdapterError::ImportRuleProhibited { node: node.id });
        }

        diagnostics.set_current_node(Some(node.id));
        let media = parse_media_list(
            element.html_attribute("media").unwrap_or(""),
            url_data,
            diagnostics,
        );
        let sheet = Stylesheet::from_str(
            &css,
            url_data.clone(),
            Origin::Author,
            Arc::new(shared_lock.wrap(media)),
            shared_lock.clone(),
            None,
            Some(diagnostics),
            QuirksMode::NoQuirks,
            AllowImportRules::No,
        );
        diagnostics.set_current_node(None);
        sheets.push(DocumentStyleSheet(Arc::new(sheet)));
    }
    Ok(())
}

fn collect_descendant_text(
    adapter: &AdapterSnapshot,
    root: NodeId,
    remaining_bytes: usize,
    limits: StyleLimits,
) -> Result<String, StyleAdapterError> {
    let root_node = adapter
        .source()
        .node(root)
        .ok_or(StyleAdapterError::SnapshotInvariant(
            "style element is absent from its source snapshot",
        ))?;
    let node_capacity = adapter.source().nodes_in_document_order().len();
    let mut stack = Vec::new();
    stack
        .try_reserve_exact(node_capacity)
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "stylesheet descendant traversal",
            requested: node_capacity,
        })?;
    stack.extend(root_node.children.iter().rev().copied());
    let mut text_nodes = Vec::new();
    text_nodes.try_reserve_exact(node_capacity).map_err(|_| {
        StyleAdapterError::AllocationFailed {
            resource: "stylesheet text-node list",
            requested: node_capacity,
        }
    })?;
    let mut text_bytes = 0_usize;
    while let Some(node_id) = stack.pop() {
        let node = adapter
            .source()
            .node(node_id)
            .ok_or(StyleAdapterError::MissingRelation {
                node: root,
                relation: "stylesheet descendant",
                target: node_id,
            })?;
        if let NodeKind::Text(text) = &node.kind {
            text_bytes = text_bytes
                .checked_add(text.len())
                .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
            if text_bytes > remaining_bytes {
                return Err(StyleAdapterError::StylesheetByteLimitExceeded {
                    limit: limits.max_stylesheet_bytes,
                });
            }
            text_nodes.push(node_id);
        }
        stack.extend(node.children.iter().rev().copied());
    }
    let mut css = String::new();
    css.try_reserve_exact(text_bytes)
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "author stylesheet text",
            requested: text_bytes,
        })?;
    for node in text_nodes {
        if let Some(NodeKind::Text(text)) = adapter.source().node(node).map(|node| &node.kind) {
            css.push_str(text);
        }
    }
    Ok(css)
}

fn parse_media_list(
    input: &str,
    url_data: &UrlExtraData,
    diagnostics: &DiagnosticCollector,
) -> MediaList {
    let mut parser_input = ParserInput::new(input);
    let mut parser = Parser::new(&mut parser_input);
    let mut context = ParserContext::new(
        Origin::Author,
        url_data,
        None,
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        Some(diagnostics),
        None,
        AttrTaint::default(),
    );
    MediaList::parse(&mut context, &mut parser)
}

fn contains_top_level_import(css: &str) -> bool {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    while let Ok(token) = parser.next_including_whitespace_and_comments() {
        if matches!(token, Token::AtKeyword(keyword) if keyword.eq_ignore_ascii_case("import")) {
            return true;
        }
    }
    false
}

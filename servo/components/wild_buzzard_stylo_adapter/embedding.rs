/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stylo DOM-trait views over an immutable Wild Buzzard snapshot.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

use euclid::default::Size2D;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::{BloomFilter, BLOOM_HASH_MASK};
use selectors::matching::{ElementSelectorFlags, MatchingContext, QuirksMode, VisitedHandlingMode};
use selectors::sink::Push;
use selectors::Element as SelectorsElement;
use selectors::OpaqueElement;
use servo_arc::{Arc, ArcBorrow};
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::context::{SharedStyleContext, TreeCountingCaches};
use style::data::{ElementData, ElementDataMut, ElementDataRef, ElementDataWrapper};
use style::dom::{
    ElementContext, LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot,
};
use style::properties::PropertyDeclarationBlock;
use style::selector_parser::{
    extended_filtering, AttrValue, Lang, NonTSPseudoClass, PseudoElement, SelectorImpl,
};
use style::shared_lock::{Locked, SharedRwLock};
use style::values::computed::{Display, TreeCountingResult};
use style::values::{AtomIdent, AtomString};
use style::{Atom, LocalName, Namespace};
use wild_buzzard_dom::{
    DocumentSnapshot, ElementData as DomElementData, Namespace as DomNamespace, NodeId, NodeKind,
    SnapshotNode,
};
use wild_buzzard_style_platform::ElementState;

use crate::engine::{DiagnosticCollector, StyleLimits};
use crate::error::StyleAdapterError;
use crate::state::SelectorStateSnapshot;

#[derive(Debug)]
struct AdapterAttribute {
    namespace: Namespace,
    local_name: LocalName,
    value: String,
}

#[derive(Debug)]
struct AdapterElement {
    local_name: LocalName,
    namespace: Namespace,
    attributes: Box<[AdapterAttribute]>,
    id: Option<Atom>,
    classes: Box<[Atom]>,
    inline_style: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    selector_state: ElementState,
    style_data: ElementDataWrapper,
    data_present: AtomicBool,
    selector_flags: AtomicUsize,
    dirty_descendants: AtomicBool,
    handled_snapshot: AtomicBool,
    children_to_process: AtomicIsize,
}

#[derive(Debug)]
enum AdapterNodeKind {
    Document,
    Element(AdapterElement),
    Text { is_empty: bool },
    Other,
}

#[derive(Clone, Copy, Debug, Default)]
struct Relations {
    parent: Option<u32>,
    first_child: Option<u32>,
    last_child: Option<u32>,
    previous_sibling: Option<u32>,
    next_sibling: Option<u32>,
    depth: usize,
}

#[derive(Debug)]
struct AdapterNode {
    source_id: NodeId,
    relations: Relations,
    kind: AdapterNodeKind,
}

/// Owned immutable tree plus adapter-owned Stylo bookkeeping.
pub(crate) struct AdapterSnapshot {
    source: Arc<DocumentSnapshot>,
    nodes: Box<[AdapterNode]>,
    document_index: u32,
    document_element_index: Option<u32>,
    shared_lock: SharedRwLock,
    element_count: usize,
    inline_declaration_count: usize,
}

impl fmt::Debug for AdapterSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterSnapshot")
            .field("document_id", &self.source.document_id())
            .field("revision", &self.source.revision())
            .field("nodes", &self.nodes.len())
            .field("elements", &self.element_count)
            .finish_non_exhaustive()
    }
}

impl AdapterSnapshot {
    pub(crate) fn new(
        source: DocumentSnapshot,
        shared_lock: SharedRwLock,
        url_data: &style::stylesheets::UrlExtraData,
        diagnostics: &DiagnosticCollector,
        limits: StyleLimits,
        selector_states: Option<&SelectorStateSnapshot>,
    ) -> Result<Self, StyleAdapterError> {
        preflight_snapshot_strings(&source, limits)?;
        let source_nodes = source.nodes_in_document_order();
        if source_nodes.len() > limits.max_nodes {
            return Err(StyleAdapterError::NodeLimitExceeded {
                limit: limits.max_nodes,
                actual: source_nodes.len(),
            });
        }
        let source_to_index = build_source_index(source_nodes)?;
        let relations = build_relations(source_nodes, &source_to_index, limits)?;
        let (nodes, element_count, inline_declaration_count) = build_adapter_nodes(
            source_nodes,
            &relations,
            &shared_lock,
            url_data,
            diagnostics,
            limits,
            selector_states,
        )?;
        let document_index = *source_to_index.get(&source.document_node()).ok_or(
            StyleAdapterError::SnapshotInvariant("snapshot document node is missing"),
        )?;
        let document_element_index = source
            .document_element()
            .map(|element| {
                relation_index(
                    &source_to_index,
                    source.document_node(),
                    "document element",
                    element,
                )
            })
            .transpose()?;
        Ok(Self {
            source: Arc::new(source),
            nodes: nodes.into_boxed_slice(),
            document_index,
            document_element_index,
            shared_lock,
            element_count,
            inline_declaration_count,
        })
    }

    pub(crate) fn source(&self) -> &DocumentSnapshot {
        &self.source
    }

    pub(crate) fn element_count(&self) -> usize {
        self.element_count
    }

    pub(crate) fn inline_declaration_count(&self) -> usize {
        self.inline_declaration_count
    }

    pub(crate) fn elements(&self) -> impl Iterator<Item = StyleElement<'_>> {
        self.nodes.iter().enumerate().filter_map(|(index, node)| {
            matches!(node.kind, AdapterNodeKind::Element(_)).then_some(StyleElement {
                tree: self,
                index: u32::try_from(index)
                    .expect("adapter construction rejects node counts wider than u32"),
            })
        })
    }

    pub(crate) fn shared_lock(&self) -> &SharedRwLock {
        &self.shared_lock
    }

    fn record(&self, index: u32) -> &AdapterNode {
        &self.nodes[index as usize]
    }
}

fn build_source_index(
    source_nodes: &[SnapshotNode],
) -> Result<HashMap<NodeId, u32>, StyleAdapterError> {
    let mut source_to_index = HashMap::new();
    source_to_index
        .try_reserve(source_nodes.len())
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "node-index map",
            requested: source_nodes.len(),
        })?;
    for (index, node) in source_nodes.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| StyleAdapterError::ResourceBoundOverflow)?;
        if source_to_index.insert(node.id, index).is_some() {
            return Err(StyleAdapterError::SnapshotInvariant(
                "snapshot contains a duplicate node identity",
            ));
        }
    }
    Ok(source_to_index)
}

fn relation_index(
    source_to_index: &HashMap<NodeId, u32>,
    node: NodeId,
    relation: &'static str,
    target: NodeId,
) -> Result<u32, StyleAdapterError> {
    source_to_index
        .get(&target)
        .copied()
        .ok_or(StyleAdapterError::MissingRelation {
            node,
            relation,
            target,
        })
}

fn build_relations(
    source_nodes: &[SnapshotNode],
    source_to_index: &HashMap<NodeId, u32>,
    limits: StyleLimits,
) -> Result<Vec<Relations>, StyleAdapterError> {
    let mut relations = Vec::new();
    relations
        .try_reserve_exact(source_nodes.len())
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "node-relations table",
            requested: source_nodes.len(),
        })?;
    relations.resize(source_nodes.len(), Relations::default());
    for (index, node) in source_nodes.iter().enumerate() {
        if let Some(parent) = node.parent {
            let parent_index = relation_index(source_to_index, node.id, "parent", parent)?;
            let parent_position = usize::try_from(parent_index)
                .map_err(|_| StyleAdapterError::ResourceBoundOverflow)?;
            if parent_position >= index {
                return Err(StyleAdapterError::SnapshotInvariant(
                    "snapshot is not in parent-before-child document order",
                ));
            }
            relations[index].parent = Some(parent_index);
            relations[index].depth = relations[parent_position]
                .depth
                .checked_add(1)
                .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
            if relations[index].depth > limits.max_tree_depth {
                return Err(StyleAdapterError::TreeDepthLimitExceeded {
                    limit: limits.max_tree_depth,
                    node: node.id,
                });
            }
        }
        relations[index].first_child = node
            .children
            .first()
            .map(|child| relation_index(source_to_index, node.id, "first child", *child))
            .transpose()?;
        relations[index].last_child = node
            .children
            .last()
            .map(|child| relation_index(source_to_index, node.id, "last child", *child))
            .transpose()?;
        set_sibling_relations(node, source_to_index, &mut relations)?;
    }
    Ok(relations)
}

fn set_sibling_relations(
    node: &SnapshotNode,
    source_to_index: &HashMap<NodeId, u32>,
    relations: &mut [Relations],
) -> Result<(), StyleAdapterError> {
    for (child_position, child) in node.children.iter().enumerate() {
        let child_index = relation_index(source_to_index, node.id, "child", *child)?;
        let child_index =
            usize::try_from(child_index).map_err(|_| StyleAdapterError::ResourceBoundOverflow)?;
        relations[child_index].previous_sibling = child_position
            .checked_sub(1)
            .map(|previous| {
                relation_index(
                    source_to_index,
                    node.id,
                    "previous sibling",
                    node.children[previous],
                )
            })
            .transpose()?;
        relations[child_index].next_sibling = node
            .children
            .get(child_position + 1)
            .map(|next| relation_index(source_to_index, node.id, "next sibling", *next))
            .transpose()?;
    }
    Ok(())
}

#[derive(Default)]
struct AdapterBuildCounts {
    inline_style_bytes: usize,
    inline_declarations: usize,
    elements: usize,
}

struct AdapterBuildContext<'a> {
    shared_lock: &'a SharedRwLock,
    url_data: &'a style::stylesheets::UrlExtraData,
    diagnostics: &'a DiagnosticCollector,
    limits: StyleLimits,
    selector_states: Option<&'a SelectorStateSnapshot>,
}

fn build_adapter_nodes(
    source_nodes: &[SnapshotNode],
    relations: &[Relations],
    shared_lock: &SharedRwLock,
    url_data: &style::stylesheets::UrlExtraData,
    diagnostics: &DiagnosticCollector,
    limits: StyleLimits,
    selector_states: Option<&SelectorStateSnapshot>,
) -> Result<(Vec<AdapterNode>, usize, usize), StyleAdapterError> {
    let context = AdapterBuildContext {
        shared_lock,
        url_data,
        diagnostics,
        limits,
        selector_states,
    };
    let mut counts = AdapterBuildCounts::default();
    let mut nodes = Vec::new();
    nodes.try_reserve_exact(source_nodes.len()).map_err(|_| {
        StyleAdapterError::AllocationFailed {
            resource: "adapter node table",
            requested: source_nodes.len(),
        }
    })?;
    for (index, node) in source_nodes.iter().enumerate() {
        let kind = match &node.kind {
            NodeKind::Document => AdapterNodeKind::Document,
            NodeKind::Element(element) => AdapterNodeKind::Element(build_adapter_element(
                node.id,
                element,
                &context,
                &mut counts,
            )?),
            NodeKind::Text(text) => AdapterNodeKind::Text {
                is_empty: text.is_empty(),
            },
            NodeKind::DocumentType(_) | NodeKind::Comment(_) => AdapterNodeKind::Other,
        };
        nodes.push(AdapterNode {
            source_id: node.id,
            relations: relations[index],
            kind,
        });
    }
    Ok((nodes, counts.elements, counts.inline_declarations))
}

fn build_adapter_element(
    node: NodeId,
    element: &DomElementData,
    context: &AdapterBuildContext<'_>,
    counts: &mut AdapterBuildCounts,
) -> Result<AdapterElement, StyleAdapterError> {
    counts.elements = counts
        .elements
        .checked_add(1)
        .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
    if counts.elements > context.limits.max_style_entries {
        return Err(StyleAdapterError::ElementLimitExceeded {
            limit: context.limits.max_style_entries,
            actual: counts.elements,
        });
    }
    let mut attributes = Vec::new();
    attributes
        .try_reserve_exact(element.attributes.len())
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "attribute table",
            requested: element.attributes.len(),
        })?;
    attributes.extend(element.attributes.iter().map(|attribute| AdapterAttribute {
        namespace: Namespace::from(attribute.name.namespace.as_deref().unwrap_or("")),
        local_name: LocalName::from(attribute.name.local_name.as_str()),
        value: attribute.value.clone(),
    }));
    let classes = atomize_classes(element)?;
    let inline_style = parse_inline_style(
        node,
        element,
        context.shared_lock,
        context.url_data,
        context.diagnostics,
        context.limits,
        counts,
    )?;
    Ok(AdapterElement {
        local_name: LocalName::from(element.name.local_name.as_str()),
        namespace: Namespace::from(element.name.namespace.as_uri()),
        attributes: attributes.into_boxed_slice(),
        id: element.html_attribute("id").map(Atom::from),
        classes,
        inline_style,
        selector_state: context
            .selector_states
            .map_or_else(ElementState::empty, |states| states.get(node)),
        style_data: ElementDataWrapper::default(),
        data_present: AtomicBool::new(true),
        selector_flags: AtomicUsize::new(0),
        dirty_descendants: AtomicBool::new(false),
        handled_snapshot: AtomicBool::new(true),
        children_to_process: AtomicIsize::new(0),
    })
}

fn atomize_classes(element: &DomElementData) -> Result<Box<[Atom]>, StyleAdapterError> {
    let class_count = element
        .html_attribute("class")
        .into_iter()
        .flat_map(str::split_ascii_whitespace)
        .count();
    let mut classes = Vec::new();
    classes
        .try_reserve_exact(class_count)
        .map_err(|_| StyleAdapterError::AllocationFailed {
            resource: "class atom table",
            requested: class_count,
        })?;
    classes.extend(
        element
            .html_attribute("class")
            .into_iter()
            .flat_map(str::split_ascii_whitespace)
            .map(Atom::from),
    );
    Ok(classes.into_boxed_slice())
}

fn parse_inline_style(
    node: NodeId,
    element: &DomElementData,
    shared_lock: &SharedRwLock,
    url_data: &style::stylesheets::UrlExtraData,
    diagnostics: &DiagnosticCollector,
    limits: StyleLimits,
    counts: &mut AdapterBuildCounts,
) -> Result<Option<Arc<Locked<PropertyDeclarationBlock>>>, StyleAdapterError> {
    let Some(css) = element.html_attribute("style") else {
        return Ok(None);
    };
    counts.inline_style_bytes = counts
        .inline_style_bytes
        .checked_add(css.len())
        .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
    if counts.inline_style_bytes > limits.max_inline_style_bytes {
        return Err(StyleAdapterError::InlineStyleByteLimitExceeded {
            limit: limits.max_inline_style_bytes,
        });
    }
    diagnostics.set_current_node(Some(node));
    let block = style::properties::parse_style_attribute(
        css,
        url_data,
        Some(diagnostics),
        QuirksMode::NoQuirks,
        style::stylesheets::CssRuleType::Style,
    );
    diagnostics.set_current_node(None);
    counts.inline_declarations = counts
        .inline_declarations
        .checked_add(block.len())
        .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
    if counts.inline_declarations > limits.max_declarations {
        return Err(StyleAdapterError::DeclarationLimitExceeded {
            limit: limits.max_declarations,
            actual: counts.inline_declarations,
        });
    }
    Ok(Some(Arc::new(shared_lock.wrap(block))))
}

fn preflight_snapshot_strings(
    source: &DocumentSnapshot,
    limits: StyleLimits,
) -> Result<(), StyleAdapterError> {
    let mut budget = SnapshotBudget::default();
    for node in source.nodes_in_document_order() {
        match &node.kind {
            NodeKind::Element(element) => budget.inspect_element(element, limits)?,
            NodeKind::Text(text) | NodeKind::Comment(text) => budget.add_string(text, limits)?,
            NodeKind::DocumentType(doctype) => {
                budget.add_string(&doctype.name, limits)?;
                budget.add_string(&doctype.public_id, limits)?;
                budget.add_string(&doctype.system_id, limits)?;
            }
            NodeKind::Document => {}
        }
    }
    Ok(())
}

#[derive(Default)]
struct SnapshotBudget {
    attributes: usize,
    class_tokens: usize,
    class_bytes: usize,
    atoms: usize,
    string_bytes: usize,
}

impl SnapshotBudget {
    fn inspect_element(
        &mut self,
        element: &DomElementData,
        limits: StyleLimits,
    ) -> Result<(), StyleAdapterError> {
        check_resource(
            "element local-name bytes",
            element.name.local_name.len(),
            limits.max_name_bytes,
        )?;
        check_resource(
            "element namespace bytes",
            element.name.namespace.as_uri().len(),
            limits.max_name_bytes,
        )?;
        self.add_string(&element.name.local_name, limits)?;
        self.add_string(element.name.namespace.as_uri(), limits)?;
        self.add_atoms(2, limits)?;
        if let Some(prefix) = &element.name.prefix {
            check_resource("element prefix bytes", prefix.len(), limits.max_name_bytes)?;
            self.add_string(prefix, limits)?;
        }
        check_resource(
            "attributes per element",
            element.attributes.len(),
            limits.max_attributes_per_element,
        )?;
        self.attributes = self
            .attributes
            .checked_add(element.attributes.len())
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        check_resource("attribute count", self.attributes, limits.max_attributes)?;
        self.add_atoms(
            element
                .attributes
                .len()
                .checked_mul(2)
                .ok_or(StyleAdapterError::ResourceBoundOverflow)?,
            limits,
        )?;
        for attribute in &element.attributes {
            self.inspect_attribute(attribute, limits)?;
        }
        if let Some(id) = element.html_attribute("id") {
            check_resource("id bytes", id.len(), limits.max_identifier_bytes)?;
            self.add_atoms(1, limits)?;
        }
        self.inspect_classes(element, limits)
    }

    fn inspect_attribute(
        &mut self,
        attribute: &wild_buzzard_dom::Attribute,
        limits: StyleLimits,
    ) -> Result<(), StyleAdapterError> {
        let namespace = attribute.name.namespace.as_deref().unwrap_or("");
        check_resource(
            "attribute local-name bytes",
            attribute.name.local_name.len(),
            limits.max_name_bytes,
        )?;
        check_resource(
            "attribute namespace bytes",
            namespace.len(),
            limits.max_name_bytes,
        )?;
        check_resource(
            "attribute value bytes",
            attribute.value.len(),
            limits.max_attribute_value_bytes,
        )?;
        self.add_string(&attribute.name.local_name, limits)?;
        self.add_string(namespace, limits)?;
        self.add_string(&attribute.value, limits)?;
        if let Some(prefix) = &attribute.name.prefix {
            check_resource(
                "attribute prefix bytes",
                prefix.len(),
                limits.max_name_bytes,
            )?;
            self.add_string(prefix, limits)?;
        }
        Ok(())
    }

    fn inspect_classes(
        &mut self,
        element: &DomElementData,
        limits: StyleLimits,
    ) -> Result<(), StyleAdapterError> {
        for class in element
            .html_attribute("class")
            .into_iter()
            .flat_map(str::split_ascii_whitespace)
        {
            check_resource(
                "class token bytes",
                class.len(),
                limits.max_identifier_bytes,
            )?;
            self.class_tokens = self
                .class_tokens
                .checked_add(1)
                .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
            self.class_bytes = self
                .class_bytes
                .checked_add(class.len())
                .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
            self.add_atoms(1, limits)?;
            check_resource("class tokens", self.class_tokens, limits.max_class_tokens)?;
            check_resource(
                "class token bytes",
                self.class_bytes,
                limits.max_class_bytes,
            )?;
        }
        Ok(())
    }

    fn add_atoms(&mut self, count: usize, limits: StyleLimits) -> Result<(), StyleAdapterError> {
        self.atoms = self
            .atoms
            .checked_add(count)
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        check_resource("atoms", self.atoms, limits.max_atoms)
    }

    fn add_string(&mut self, value: &str, limits: StyleLimits) -> Result<(), StyleAdapterError> {
        self.string_bytes = self
            .string_bytes
            .checked_add(value.len())
            .ok_or(StyleAdapterError::ResourceBoundOverflow)?;
        check_resource(
            "string bytes",
            self.string_bytes,
            limits.max_snapshot_string_bytes,
        )
    }
}

fn check_resource(
    resource: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), StyleAdapterError> {
    if actual > limit {
        Err(StyleAdapterError::SnapshotResourceLimitExceeded {
            resource,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StyleNode<'tree> {
    tree: &'tree AdapterSnapshot,
    index: u32,
}

impl fmt::Debug for StyleNode<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleNode")
            .field("node_id", &self.record().source_id)
            .finish()
    }
}

impl PartialEq for StyleNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.tree, other.tree) && self.index == other.index
    }
}

impl<'tree> StyleNode<'tree> {
    fn record(self) -> &'tree AdapterNode {
        self.tree.record(self.index)
    }

    fn related(self, index: Option<u32>) -> Option<Self> {
        index.map(|index| Self {
            tree: self.tree,
            index,
        })
    }
}

impl NodeInfo for StyleNode<'_> {
    fn is_element(&self) -> bool {
        matches!(self.record().kind, AdapterNodeKind::Element(_))
    }

    fn is_text_node(&self) -> bool {
        matches!(self.record().kind, AdapterNodeKind::Text { .. })
    }
}

impl<'tree> TNode for StyleNode<'tree> {
    type ConcreteDocument = StyleDocument<'tree>;
    type ConcreteElement = StyleElement<'tree>;
    type ConcreteShadowRoot = NoShadowRoot<'tree>;

    fn parent_node(&self) -> Option<Self> {
        self.related(self.record().relations.parent)
    }

    fn first_child(&self) -> Option<Self> {
        self.related(self.record().relations.first_child)
    }

    fn last_child(&self) -> Option<Self> {
        self.related(self.record().relations.last_child)
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.related(self.record().relations.previous_sibling)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.related(self.record().relations.next_sibling)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        StyleDocument { tree: self.tree }
    }

    fn is_in_document(&self) -> bool {
        true
    }

    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        self.parent_node().and_then(|parent| parent.as_element())
    }

    fn opaque(&self) -> OpaqueNode {
        // Stylo treats this address strictly as an equality/hash token. The
        // record stays boxed for the `AdapterSnapshot` lifetime, and no opaque
        // identity escapes the synchronous preparation call.
        OpaqueNode(std::ptr::from_ref(self.record()).addr())
    }

    fn debug_id(self) -> usize {
        std::ptr::from_ref(self.record()).addr()
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        self.is_element().then_some(StyleElement {
            tree: self.tree,
            index: self.index,
        })
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        (self.index == self.tree.document_index).then_some(StyleDocument { tree: self.tree })
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        None
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StyleDocument<'tree> {
    tree: &'tree AdapterSnapshot,
}

impl<'tree> TDocument for StyleDocument<'tree> {
    type ConcreteNode = StyleNode<'tree>;

    fn as_node(&self) -> Self::ConcreteNode {
        StyleNode {
            tree: self.tree,
            index: self.tree.document_index,
        }
    }

    fn is_html_document(&self) -> bool {
        self.tree.document_element_index.is_some_and(|index| {
            let AdapterNodeKind::Element(element) = &self.tree.record(index).kind else {
                return false;
            };
            element.namespace == Namespace::from(DomNamespace::HTML_URI)
        })
    }

    fn quirks_mode(&self) -> QuirksMode {
        QuirksMode::NoQuirks
    }

    fn elements_with_id<'a>(
        &self,
        _id: &AtomIdent,
    ) -> Result<&'a [<Self::ConcreteNode as TNode>::ConcreteElement], ()>
    where
        Self: 'a,
    {
        // The owned snapshot intentionally does not contain self-referential
        // slices of handles. Returning Err disables only this lookup shortcut;
        // ordinary id selector matching remains active.
        Err(())
    }

    fn shared_lock(&self) -> &SharedRwLock {
        self.tree.shared_lock()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StyleElement<'tree> {
    tree: &'tree AdapterSnapshot,
    index: u32,
}

impl<'tree> StyleElement<'tree> {
    pub(crate) fn source_id(self) -> NodeId {
        self.node_record().source_id
    }

    pub(crate) fn depth(self) -> usize {
        let mut depth = 0;
        let mut cursor = self.parent_element();
        while let Some(parent) = cursor {
            depth += 1;
            cursor = parent.parent_element();
        }
        depth
    }

    fn node_record(self) -> &'tree AdapterNode {
        self.tree.record(self.index)
    }

    fn element_record(self) -> &'tree AdapterElement {
        let AdapterNodeKind::Element(element) = &self.node_record().kind else {
            unreachable!("StyleElement was constructed only for an element record")
        };
        element
    }

    fn attribute(self, namespace: &Namespace, local_name: &LocalName) -> Option<&'tree str> {
        self.element_record()
            .attributes
            .iter()
            .find(|attribute| {
                &attribute.namespace == namespace && &attribute.local_name == local_name
            })
            .map(|attribute| attribute.value.as_str())
    }

    fn attributes_with_name(
        self,
        local_name: &LocalName,
    ) -> impl Iterator<Item = &'tree AdapterAttribute> {
        let local_name = local_name.clone();
        self.element_record()
            .attributes
            .iter()
            .filter(move |attribute| attribute.local_name == local_name)
    }

    fn dynamic_state(self) -> ElementState {
        let mut state = self.element_record().selector_state;
        if self.is_link() {
            state.insert(ElementState::UNVISITED);
        }
        state
    }
}

impl fmt::Debug for StyleElement<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleElement")
            .field("node_id", &self.node_record().source_id)
            .field("local_name", &self.element_record().local_name)
            .finish()
    }
}

impl PartialEq for StyleElement<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.tree, other.tree) && self.index == other.index
    }
}

impl Eq for StyleElement<'_> {}

impl Hash for StyleElement<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::from_ref(self.tree).addr().hash(state);
        self.index.hash(state);
    }
}

pub(crate) struct StyleChildren<'tree> {
    next: Option<StyleNode<'tree>>,
}

impl<'tree> Iterator for StyleChildren<'tree> {
    type Item = StyleNode<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.next.take()?;
        self.next = next.next_sibling();
        Some(next)
    }
}

impl<'tree> TElement for StyleElement<'tree> {
    type ConcreteNode = StyleNode<'tree>;
    type TraversalChildrenIterator = StyleChildren<'tree>;

    fn as_node(&self) -> Self::ConcreteNode {
        StyleNode {
            tree: self.tree,
            index: self.index,
        }
    }

    fn traversal_children(&self) -> LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(StyleChildren {
            next: self.as_node().first_child(),
        })
    }

    fn is_html_element(&self) -> bool {
        self.element_record().namespace == Namespace::from(DomNamespace::HTML_URI)
    }

    fn is_mathml_element(&self) -> bool {
        self.element_record().namespace == Namespace::from(DomNamespace::MATHML_URI)
    }

    fn is_svg_element(&self) -> bool {
        self.element_record().namespace == Namespace::from(DomNamespace::SVG_URI)
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.element_record()
            .inline_style
            .as_ref()
            .map(Arc::borrow_arc)
    }

    fn animation_rule(
        &self,
        _context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn transition_rule(
        &self,
        _context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn state(&self) -> ElementState {
        self.dynamic_state()
    }

    fn has_part_attr(&self) -> bool {
        false
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&Atom> {
        self.element_record().id.as_ref()
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for class in &self.element_record().classes {
            callback(AtomIdent::cast(class));
        }
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&LocalName),
    {
        for attribute in &self.element_record().attributes {
            callback(&attribute.local_name);
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        self.element_record()
            .dirty_descendants
            .load(Ordering::Acquire)
    }

    fn has_snapshot(&self) -> bool {
        false
    }

    fn handled_snapshot(&self) -> bool {
        self.element_record()
            .handled_snapshot
            .load(Ordering::Acquire)
    }

    /// The static adapter never publishes mutation snapshots. This method is
    /// nevertheless implemented because Stylo's generic trait requires it.
    #[allow(unsafe_code)]
    unsafe fn set_handled_snapshot(&self) {
        self.element_record()
            .handled_snapshot
            .store(true, Ordering::Release);
    }

    /// Callers must hold exclusive access to this adapter's sequential style
    /// preparation. The adapter never runs parallel traversal in this wave.
    #[allow(unsafe_code)]
    unsafe fn set_dirty_descendants(&self) {
        self.element_record()
            .dirty_descendants
            .store(true, Ordering::Release);
    }

    /// Callers must hold exclusive access to this adapter's sequential style
    /// preparation. The adapter never runs parallel traversal in this wave.
    #[allow(unsafe_code)]
    unsafe fn unset_dirty_descendants(&self) {
        self.element_record()
            .dirty_descendants
            .store(false, Ordering::Release);
    }

    fn store_children_to_process(&self, count: isize) {
        self.element_record()
            .children_to_process
            .store(count, Ordering::Release);
    }

    fn did_process_child(&self) -> isize {
        self.element_record()
            .children_to_process
            .fetch_sub(1, Ordering::AcqRel)
            - 1
    }

    /// The side-table entry is allocated before wrapper publication. Exclusive
    /// sequential preparation prevents release-mode aliasing in Stylo's
    /// `ElementDataWrapper`; debug builds additionally enforce borrows.
    #[allow(unsafe_code)]
    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        self.element_record()
            .data_present
            .store(true, Ordering::Release);
        self.element_record().style_data.borrow_mut()
    }

    /// The same exclusive sequential-preparation invariant as `ensure_data`
    /// applies. Storage remains allocated, but becomes observably absent.
    #[allow(unsafe_code)]
    unsafe fn clear_data(&self) {
        *self.element_record().style_data.borrow_mut() = ElementData::default();
        self.element_record()
            .data_present
            .store(false, Ordering::Release);
    }

    fn has_data(&self) -> bool {
        self.element_record().data_present.load(Ordering::Acquire)
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        self.has_data()
            .then(|| self.element_record().style_data.borrow())
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        self.has_data()
            .then(|| self.element_record().style_data.borrow_mut())
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn has_animations(&self, _context: &SharedStyleContext) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        _context: &SharedStyleContext,
        _pseudo_element: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        _context: &SharedStyleContext,
        _pseudo_element: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn lang_attr(&self) -> Option<AttrValue> {
        let empty = Namespace::from("");
        let xml = Namespace::from("http://www.w3.org/XML/1998/namespace");
        let lang = LocalName::from("lang");
        self.attribute(&xml, &lang)
            .or_else(|| self.attribute(&empty, &lang))
            .map(AtomString::from)
    }

    fn match_element_lang(&self, override_lang: Option<Option<AttrValue>>, value: &Lang) -> bool {
        let owned;
        let language = match override_lang {
            Some(Some(language)) => {
                owned = language;
                owned.as_ref()
            }
            Some(None) => "",
            None => {
                let mut cursor = Some(*self);
                let mut found = None;
                while let Some(element) = cursor {
                    if let Some(language) = element.lang_attr() {
                        found = Some(language);
                        break;
                    }
                    cursor = element.parent_element();
                }
                let Some(language) = found else {
                    return false;
                };
                owned = language;
                owned.as_ref()
            }
        };
        extended_filtering(language, value)
    }

    fn is_html_document_body_element(&self) -> bool {
        self.is_html_element()
            && self.element_record().local_name == LocalName::from("body")
            && self.parent_element().is_some_and(|parent| {
                parent.is_html_element()
                    && parent.element_record().local_name == LocalName::from("html")
            })
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        _hints: &mut V,
    ) where
        V: Push<ApplicableDeclarationBlock>,
    {
        // Legacy presentational hints are a recorded gap. Returning no hints
        // does not bypass selector matching or author cascade.
    }

    fn local_name(&self) -> &<SelectorImpl as selectors::parser::SelectorImpl>::BorrowedLocalName {
        &self.element_record().local_name.0
    }

    fn namespace(
        &self,
    ) -> &<SelectorImpl as selectors::parser::SelectorImpl>::BorrowedNamespaceUrl {
        &self.element_record().namespace.0
    }

    fn query_container_size(&self, _display: &Display) -> Size2D<Option<app_units::Au>> {
        Size2D::new(None, None)
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        let present = ElementSelectorFlags::from_bits_retain(
            self.element_record().selector_flags.load(Ordering::Acquire),
        );
        present.contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        ElementSelectorFlags::from_bits_retain(
            self.element_record().selector_flags.load(Ordering::Acquire),
        ) & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
    }
}

impl ElementContext for StyleElement<'_> {
    fn opaque_element(&self) -> Option<OpaqueElement> {
        Some(self.opaque())
    }

    fn opaque_parent(&self) -> Option<OpaqueNode> {
        self.as_node().parent_node().map(|node| node.opaque())
    }

    fn get_attr(&self, attr: &LocalName, namespace: &Namespace) -> Option<String> {
        self.attribute(namespace, attr).map(str::to_owned)
    }

    fn get_tree_counting_result(&self, caches: &mut TreeCountingCaches) -> TreeCountingResult {
        let Some(parent) = self.as_node().parent_node() else {
            return TreeCountingResult::default();
        };
        let mut index = 0_u32;
        let mut count = 0_u32;
        for sibling in parent.dom_children().filter_map(|node| node.as_element()) {
            count = count.saturating_add(1);
            if sibling == *self {
                index = count;
            }
            caches.sibling_index.insert(sibling.opaque(), count);
        }
        caches.sibling_count.insert(parent.opaque(), count);
        TreeCountingResult::new(index, count)
    }
}

impl SelectorsElement for StyleElement<'_> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        // This matcher identity is transient. The boxed `AdapterNode` remains
        // at a stable address throughout the borrowed adapter lifetime.
        OpaqueElement::new(self.node_record())
    }

    fn parent_element(&self) -> Option<Self> {
        self.as_node()
            .parent_node()
            .and_then(|node| node.as_element())
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let mut node = self.as_node();
        while let Some(sibling) = node.prev_sibling() {
            if let Some(element) = sibling.as_element() {
                return Some(element);
            }
            node = sibling;
        }
        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut node = self.as_node();
        while let Some(sibling) = node.next_sibling() {
            if let Some(element) = sibling.as_element() {
                return Some(element);
            }
            node = sibling;
        }
        None
    }

    fn first_element_child(&self) -> Option<Self> {
        self.as_node()
            .dom_children()
            .find_map(|node| node.as_element())
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.is_html_element() && self.as_node().owner_doc().is_html_document()
    }

    fn has_local_name(
        &self,
        local_name: &<Self::Impl as selectors::parser::SelectorImpl>::BorrowedLocalName,
    ) -> bool {
        &self.element_record().local_name.0 == local_name
    }

    fn has_namespace(
        &self,
        namespace: &<Self::Impl as selectors::parser::SelectorImpl>::BorrowedNamespaceUrl,
    ) -> bool {
        &self.element_record().namespace.0 == namespace
    }

    fn is_same_type(&self, other: &Self) -> bool {
        self.element_record().local_name == other.element_record().local_name
            && self.element_record().namespace == other.element_record().namespace
    }

    fn attr_matches(
        &self,
        namespace: &NamespaceConstraint<&Namespace>,
        local_name: &LocalName,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        match namespace {
            NamespaceConstraint::Specific(namespace) => self
                .attribute(namespace, local_name)
                .is_some_and(|value| operation.eval_str(value)),
            NamespaceConstraint::Any => self
                .attributes_with_name(local_name)
                .any(|attribute| operation.eval_str(&attribute.value)),
        }
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &NonTSPseudoClass,
        _context: &mut MatchingContext<SelectorImpl>,
    ) -> bool {
        match pseudo_class {
            NonTSPseudoClass::CustomState(_) | NonTSPseudoClass::Visited => false,
            NonTSPseudoClass::Lang(language) => self.match_element_lang(None, language),
            NonTSPseudoClass::ServoNonZeroBorder => self
                .attribute(&Namespace::from(""), &LocalName::from("border"))
                .is_some_and(|value| value.parse::<u32>().is_ok_and(|value| value != 0)),
            NonTSPseudoClass::Link | NonTSPseudoClass::AnyLink => self.is_link(),
            _ => {
                let required = pseudo_class.state_flag();
                !required.is_empty() && self.dynamic_state().contains(required)
            }
        }
    }

    fn match_pseudo_element(
        &self,
        _pseudo_element: &PseudoElement,
        _context: &mut MatchingContext<SelectorImpl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        let self_flags = flags.for_self();
        if !self_flags.is_empty() {
            self.element_record()
                .selector_flags
                .fetch_or(self_flags.bits(), Ordering::AcqRel);
        }
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty() {
            if let Some(parent) = self.parent_element() {
                parent
                    .element_record()
                    .selector_flags
                    .fetch_or(parent_flags.bits(), Ordering::AcqRel);
            }
        }
    }

    fn is_link(&self) -> bool {
        let empty = Namespace::from("");
        let href = LocalName::from("href");
        if self.is_html_element()
            && matches!(
                self.element_record().local_name.as_ref(),
                "a" | "area" | "link"
            )
        {
            return self.attribute(&empty, &href).is_some();
        }
        if self.is_svg_element() && self.element_record().local_name.as_ref() == "a" {
            let xlink = Namespace::from("http://www.w3.org/1999/xlink");
            return self.attribute(&empty, &href).is_some()
                || self.attribute(&xlink, &href).is_some();
        }
        false
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(&self, id: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element_record()
            .id
            .as_ref()
            .is_some_and(|actual| case_sensitivity.eq(actual.as_bytes(), id.as_bytes()))
    }

    fn has_class(&self, name: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element_record()
            .classes
            .iter()
            .any(|class| case_sensitivity.eq(class.as_bytes(), name.as_bytes()))
    }

    fn has_custom_state(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn imported_part(&self, _name: &AtomIdent) -> Option<AtomIdent> {
        None
    }

    fn is_part(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.as_node()
            .dom_children()
            .all(|node| match node.record().kind {
                AdapterNodeKind::Element(_) => false,
                AdapterNodeKind::Text { is_empty } => is_empty,
                AdapterNodeKind::Document | AdapterNodeKind::Other => true,
            })
    }

    fn is_root(&self) -> bool {
        self.tree.document_element_index == Some(self.index)
    }

    fn add_element_unique_hashes(&self, filter: &mut BloomFilter) -> bool {
        style::bloom::each_relevant_element_hash(*self, |hash| {
            filter.insert_hash(hash & BLOOM_HASH_MASK);
        });
        true
    }
}

/// Uninhabited type proving that this static adapter has no shadow roots.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NoShadowRoot<'tree> {
    never: Infallible,
    marker: PhantomData<&'tree ()>,
}

impl<'tree> TShadowRoot for NoShadowRoot<'tree> {
    type ConcreteNode = StyleNode<'tree>;

    fn as_node(&self) -> Self::ConcreteNode {
        match self.never {}
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        match self.never {}
    }

    fn style_data<'a>(&self) -> Option<&'a style::stylist::CascadeData>
    where
        Self: 'a,
    {
        match self.never {}
    }
}

#[cfg(test)]
mod tests {
    use std::mem::{align_of, size_of};

    use style::context::ThreadLocalStyleContext;

    use super::StyleElement;

    #[test]
    fn two_word_safe_element_handle_fits_wild_buzzard_sharing_cache() {
        assert_eq!(size_of::<StyleElement<'static>>(), size_of::<usize>() * 2);
        assert_eq!(align_of::<StyleElement<'static>>(), align_of::<usize>());
        style::thread_state::initialize_layout_worker_thread();
        let context = ThreadLocalStyleContext::<StyleElement<'static>>::new();
        drop(context);
    }
}

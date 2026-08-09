//! Rust-native DOM ownership for Wild Buzzard.
//!
//! Nodes live in a document-owned arena and are addressed by stable `NodeId`
//! values. Mutations validate the DOM hierarchy before changing either the old
//! or new parent, so an error never leaves a half-reparented tree. Layout and
//! other readers consume an owned, immutable `DocumentSnapshot` rather than
//! reaching into the mutable arena.

pub mod bindings;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);

/// Stable identity of a document arena.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentId(NonZeroU64);

impl DocumentId {
    /// Numeric identity, useful for diagnostics and transport contracts.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn fresh() -> Self {
        let value = NEXT_DOCUMENT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("document identity space exhausted");
        let value = NonZeroU64::new(value).expect("document identity space exhausted");
        Self(value)
    }
}

/// Identity of one exact immutable state of a document arena.
///
/// Revisions are local to a [`DocumentId`]. Comparing bare revision numbers
/// across documents is therefore meaningless; cross-subsystem contracts carry
/// this pair as one value so a revision cannot be detached from its owner.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentVersion {
    document_id: DocumentId,
    revision: u64,
}

impl DocumentVersion {
    /// Creates an exact document-version identity.
    #[must_use]
    pub const fn new(document_id: DocumentId, revision: u64) -> Self {
        Self {
            document_id,
            revision,
        }
    }

    /// Returns the owning document arena.
    #[must_use]
    pub const fn document_id(self) -> DocumentId {
        self.document_id
    }

    /// Returns the document-local revision.
    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }
}

/// Stable, document-scoped handle to a DOM node.
///
/// Handles remain valid when a node is detached or reparented. Nodes are not
/// slot-reused in this wave, so a handle can never silently resolve to a
/// different node. The owning document is encoded to reject cross-document
/// access.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId {
    document: DocumentId,
    slot: u32,
}

impl NodeId {
    pub const fn document_id(self) -> DocumentId {
        self.document
    }

    pub const fn slot(self) -> u32 {
        self.slot
    }
}

/// Namespace of an element name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Namespace {
    Html,
    Svg,
    MathMl,
    Other(String),
}

impl Namespace {
    pub const HTML_URI: &'static str = "http://www.w3.org/1999/xhtml";
    pub const SVG_URI: &'static str = "http://www.w3.org/2000/svg";
    pub const MATHML_URI: &'static str = "http://www.w3.org/1998/Math/MathML";

    pub fn as_uri(&self) -> &str {
        match self {
            Self::Html => Self::HTML_URI,
            Self::Svg => Self::SVG_URI,
            Self::MathMl => Self::MATHML_URI,
            Self::Other(uri) => uri,
        }
    }
}

/// Namespace-aware element name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct QualifiedName {
    pub namespace: Namespace,
    pub prefix: Option<String>,
    pub local_name: String,
}

impl QualifiedName {
    pub fn html(local_name: impl Into<String>) -> Self {
        Self {
            namespace: Namespace::Html,
            prefix: None,
            local_name: local_name.into().to_ascii_lowercase(),
        }
    }

    pub fn new(
        namespace: Namespace,
        prefix: Option<String>,
        local_name: impl Into<String>,
    ) -> Result<Self, DomError> {
        let local_name = local_name.into();
        if local_name.is_empty() {
            return Err(DomError::InvalidName);
        }
        Ok(Self {
            namespace,
            prefix,
            local_name,
        })
    }
}

/// Namespace-aware attribute name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AttributeName {
    pub namespace: Option<String>,
    pub prefix: Option<String>,
    pub local_name: String,
}

impl AttributeName {
    pub fn html(local_name: impl Into<String>) -> Self {
        Self {
            namespace: None,
            prefix: None,
            local_name: local_name.into().to_ascii_lowercase(),
        }
    }

    pub fn new(
        namespace: Option<String>,
        prefix: Option<String>,
        local_name: impl Into<String>,
    ) -> Result<Self, DomError> {
        let local_name = local_name.into();
        if local_name.is_empty() {
            return Err(DomError::InvalidName);
        }
        Ok(Self {
            namespace,
            prefix,
            local_name,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attribute {
    pub name: AttributeName,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElementData {
    pub name: QualifiedName,
    /// Attributes retain insertion order. Replacing a value does not move it.
    pub attributes: Vec<Attribute>,
}

impl ElementData {
    pub fn attribute(&self, namespace: Option<&str>, local_name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| {
                attribute.name.namespace.as_deref() == namespace
                    && attribute.name.local_name == local_name
            })
            .map(|attribute| attribute.value.as_str())
    }

    pub fn html_attribute(&self, local_name: &str) -> Option<&str> {
        self.attribute(None, local_name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentTypeData {
    pub name: String,
    pub public_id: String,
    pub system_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Document,
    DocumentType(DocumentTypeData),
    Element(ElementData),
    Text(String),
    Comment(String),
}

impl NodeKind {
    pub const fn is_container(&self) -> bool {
        matches!(self, Self::Document | Self::Element(_))
    }

    pub const fn is_element(&self) -> bool {
        matches!(self, Self::Element(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DomError {
    WrongDocument {
        expected: DocumentId,
        actual: DocumentId,
    },
    UnknownNode(NodeId),
    ArenaCapacityExceeded,
    InvalidName,
    HierarchyRequest,
    Cycle,
    ReferenceIsNotChild,
    NodeIsNotChild,
    NotAnElement,
    NotCharacterData,
    SnapshotInvariant(&'static str),
}

impl fmt::Display for DomError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDocument { expected, actual } => write!(
                formatter,
                "node belongs to document {}, expected {}",
                actual.get(),
                expected.get()
            ),
            Self::UnknownNode(node) => write!(formatter, "unknown node slot {}", node.slot()),
            Self::ArenaCapacityExceeded => formatter.write_str("DOM arena capacity exceeded"),
            Self::InvalidName => formatter.write_str("name must not be empty"),
            Self::HierarchyRequest => formatter.write_str("requested DOM hierarchy is invalid"),
            Self::Cycle => formatter.write_str("mutation would create a DOM cycle"),
            Self::ReferenceIsNotChild => {
                formatter.write_str("reference node is not a child of the parent")
            }
            Self::NodeIsNotChild => formatter.write_str("node is not a child of the parent"),
            Self::NotAnElement => formatter.write_str("node is not an element"),
            Self::NotCharacterData => formatter.write_str("node is not character data"),
            Self::SnapshotInvariant(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DomError {}

#[derive(Clone, Debug)]
struct NodeRecord {
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    kind: NodeKind,
}

/// Mutable, single-owner DOM arena.
#[derive(Debug)]
pub struct Document {
    id: DocumentId,
    document_node: NodeId,
    nodes: Vec<NodeRecord>,
    revision: u64,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        let id = DocumentId::fresh();
        let document_node = NodeId {
            document: id,
            slot: 0,
        };
        Self {
            id,
            document_node,
            nodes: vec![NodeRecord {
                parent: None,
                children: Vec::new(),
                kind: NodeKind::Document,
            }],
            revision: 0,
        }
    }

    pub const fn id(&self) -> DocumentId {
        self.id
    }

    pub const fn document_node(&self) -> NodeId {
        self.document_node
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the identity of the document's current mutable state.
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        DocumentVersion::new(self.id, self.revision)
    }

    pub fn create_element(&mut self, name: QualifiedName) -> Result<NodeId, DomError> {
        if name.local_name.is_empty() {
            return Err(DomError::InvalidName);
        }
        self.allocate(NodeKind::Element(ElementData {
            name,
            attributes: Vec::new(),
        }))
    }

    pub fn create_html_element(&mut self, local_name: &str) -> Result<NodeId, DomError> {
        if local_name.is_empty() {
            return Err(DomError::InvalidName);
        }
        self.create_element(QualifiedName::html(local_name))
    }

    pub fn create_text(&mut self, data: impl Into<String>) -> Result<NodeId, DomError> {
        self.allocate(NodeKind::Text(data.into()))
    }

    pub fn create_comment(&mut self, data: impl Into<String>) -> Result<NodeId, DomError> {
        self.allocate(NodeKind::Comment(data.into()))
    }

    pub fn create_doctype(
        &mut self,
        name: impl Into<String>,
        public_id: impl Into<String>,
        system_id: impl Into<String>,
    ) -> Result<NodeId, DomError> {
        self.allocate(NodeKind::DocumentType(DocumentTypeData {
            name: name.into(),
            public_id: public_id.into(),
            system_id: system_id.into(),
        }))
    }

    fn allocate(&mut self, kind: NodeKind) -> Result<NodeId, DomError> {
        let slot = u32::try_from(self.nodes.len()).map_err(|_| DomError::ArenaCapacityExceeded)?;
        let id = NodeId {
            document: self.id,
            slot,
        };
        self.nodes.push(NodeRecord {
            parent: None,
            children: Vec::new(),
            kind,
        });
        self.bump_revision();
        Ok(id)
    }

    fn bump_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("document revision space exhausted");
    }

    fn checked_index(&self, node: NodeId) -> Result<usize, DomError> {
        if node.document != self.id {
            return Err(DomError::WrongDocument {
                expected: self.id,
                actual: node.document,
            });
        }
        let index = node.slot as usize;
        if index >= self.nodes.len() {
            return Err(DomError::UnknownNode(node));
        }
        Ok(index)
    }

    fn record(&self, node: NodeId) -> Result<&NodeRecord, DomError> {
        let index = self.checked_index(node)?;
        Ok(&self.nodes[index])
    }

    pub fn node_kind(&self, node: NodeId) -> Result<&NodeKind, DomError> {
        Ok(&self.record(node)?.kind)
    }

    pub fn parent(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
        Ok(self.record(node)?.parent)
    }

    pub fn children(&self, node: NodeId) -> Result<&[NodeId], DomError> {
        Ok(&self.record(node)?.children)
    }

    pub fn first_child(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
        Ok(self.children(node)?.first().copied())
    }

    pub fn last_child(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
        Ok(self.children(node)?.last().copied())
    }

    pub fn previous_sibling(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
        let Some(parent) = self.parent(node)? else {
            return Ok(None);
        };
        let children = self.children(parent)?;
        let position = children
            .iter()
            .position(|candidate| *candidate == node)
            .ok_or(DomError::SnapshotInvariant("parent does not contain child"))?;
        Ok(position.checked_sub(1).map(|index| children[index]))
    }

    pub fn next_sibling(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
        let Some(parent) = self.parent(node)? else {
            return Ok(None);
        };
        let children = self.children(parent)?;
        let position = children
            .iter()
            .position(|candidate| *candidate == node)
            .ok_or(DomError::SnapshotInvariant("parent does not contain child"))?;
        Ok(children.get(position + 1).copied())
    }

    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
        self.insert_before(parent, child, None)
    }

    /// Inserts `child` before `reference`, atomically reparenting it if needed.
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        child: NodeId,
        reference: Option<NodeId>,
    ) -> Result<(), DomError> {
        let parent_index = self.checked_index(parent)?;
        let child_index = self.checked_index(child)?;
        if let Some(reference) = reference {
            self.checked_index(reference)?;
            if !self.nodes[parent_index].children.contains(&reference) {
                return Err(DomError::ReferenceIsNotChild);
            }
            if reference == child {
                return Ok(());
            }
        }
        if child == self.document_node {
            return Err(DomError::HierarchyRequest);
        }
        if !self.nodes[parent_index].kind.is_container() {
            return Err(DomError::HierarchyRequest);
        }
        if parent == child || self.is_inclusive_descendant(parent, child)? {
            return Err(DomError::Cycle);
        }

        let old_parent = self.nodes[child_index].parent;
        let mut candidate = self.nodes[parent_index].children.clone();
        candidate.retain(|node| *node != child);
        let insertion_index = match reference {
            Some(reference) => candidate
                .iter()
                .position(|node| *node == reference)
                .ok_or(DomError::ReferenceIsNotChild)?,
            None => candidate.len(),
        };
        candidate.insert(insertion_index, child);
        self.validate_children(parent, &candidate)?;

        if let Some(old_parent) = old_parent {
            let old_parent_index = self.checked_index(old_parent)?;
            self.nodes[old_parent_index]
                .children
                .retain(|node| *node != child);
        }
        self.nodes[parent_index].children = candidate;
        self.nodes[child_index].parent = Some(parent);
        self.bump_revision();
        Ok(())
    }

    /// Replaces `old_child` with `new_child` after validating the final tree.
    pub fn replace_child(
        &mut self,
        parent: NodeId,
        new_child: NodeId,
        old_child: NodeId,
    ) -> Result<(), DomError> {
        let parent_index = self.checked_index(parent)?;
        let new_index = self.checked_index(new_child)?;
        let old_index = self.checked_index(old_child)?;
        if new_child == old_child {
            if self.nodes[old_index].parent == Some(parent) {
                return Ok(());
            }
            return Err(DomError::NodeIsNotChild);
        }
        let old_position = self.nodes[parent_index]
            .children
            .iter()
            .position(|node| *node == old_child)
            .ok_or(DomError::NodeIsNotChild)?;
        if new_child == self.document_node || !self.nodes[parent_index].kind.is_container() {
            return Err(DomError::HierarchyRequest);
        }
        if parent == new_child || self.is_inclusive_descendant(parent, new_child)? {
            return Err(DomError::Cycle);
        }

        let new_old_parent = self.nodes[new_index].parent;
        let new_position = if new_old_parent == Some(parent) {
            self.nodes[parent_index]
                .children
                .iter()
                .position(|node| *node == new_child)
        } else {
            None
        };
        let mut candidate = self.nodes[parent_index].children.clone();
        candidate.retain(|node| *node != new_child && *node != old_child);
        let adjusted_old_position = old_position.saturating_sub(usize::from(
            new_position.is_some_and(|position| position < old_position),
        ));
        let insertion_index = adjusted_old_position.min(candidate.len());
        candidate.insert(insertion_index, new_child);
        self.validate_children(parent, &candidate)?;

        if let Some(new_old_parent) = new_old_parent {
            let index = self.checked_index(new_old_parent)?;
            self.nodes[index].children.retain(|node| *node != new_child);
        }
        self.nodes[parent_index].children = candidate;
        self.nodes[new_index].parent = Some(parent);
        self.nodes[old_index].parent = None;
        self.bump_revision();
        Ok(())
    }

    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
        let parent_index = self.checked_index(parent)?;
        let child_index = self.checked_index(child)?;
        let position = self.nodes[parent_index]
            .children
            .iter()
            .position(|node| *node == child)
            .ok_or(DomError::NodeIsNotChild)?;
        self.nodes[parent_index].children.remove(position);
        self.nodes[child_index].parent = None;
        self.bump_revision();
        Ok(())
    }

    pub fn detach(&mut self, node: NodeId) -> Result<bool, DomError> {
        let Some(parent) = self.parent(node)? else {
            return Ok(false);
        };
        self.remove_child(parent, node)?;
        Ok(true)
    }

    fn validate_children(&self, parent: NodeId, children: &[NodeId]) -> Result<(), DomError> {
        let parent_kind = &self.record(parent)?.kind;
        match parent_kind {
            NodeKind::Document => {
                let mut seen_element = false;
                let mut seen_doctype = false;
                for child in children {
                    match self.node_kind(*child)? {
                        NodeKind::Element(_) if !seen_element => seen_element = true,
                        NodeKind::DocumentType(_) if !seen_doctype && !seen_element => {
                            seen_doctype = true;
                        }
                        NodeKind::Comment(_) => {}
                        _ => return Err(DomError::HierarchyRequest),
                    }
                }
            }
            NodeKind::Element(_) => {
                if children.iter().any(|child| {
                    matches!(
                        self.node_kind(*child),
                        Ok(NodeKind::Document | NodeKind::DocumentType(_))
                    )
                }) {
                    return Err(DomError::HierarchyRequest);
                }
            }
            _ => return Err(DomError::HierarchyRequest),
        }
        Ok(())
    }

    pub fn is_inclusive_descendant(
        &self,
        candidate: NodeId,
        ancestor: NodeId,
    ) -> Result<bool, DomError> {
        self.checked_index(candidate)?;
        self.checked_index(ancestor)?;
        let mut cursor = Some(candidate);
        while let Some(node) = cursor {
            if node == ancestor {
                return Ok(true);
            }
            cursor = self.parent(node)?;
        }
        Ok(false)
    }

    pub fn set_attribute(
        &mut self,
        element: NodeId,
        name: AttributeName,
        value: impl Into<String>,
    ) -> Result<Option<String>, DomError> {
        if name.local_name.is_empty() {
            return Err(DomError::InvalidName);
        }
        let index = self.checked_index(element)?;
        let NodeKind::Element(element_data) = &mut self.nodes[index].kind else {
            return Err(DomError::NotAnElement);
        };
        let value = value.into();
        let previous = if let Some(attribute) =
            element_data.attributes.iter_mut().find(|attribute| {
                attribute.name.namespace == name.namespace
                    && attribute.name.local_name == name.local_name
            }) {
            Some(std::mem::replace(&mut attribute.value, value))
        } else {
            element_data.attributes.push(Attribute { name, value });
            None
        };
        self.bump_revision();
        Ok(previous)
    }

    pub fn set_html_attribute(
        &mut self,
        element: NodeId,
        local_name: &str,
        value: impl Into<String>,
    ) -> Result<Option<String>, DomError> {
        self.set_attribute(element, AttributeName::html(local_name), value)
    }

    pub fn remove_attribute(
        &mut self,
        element: NodeId,
        namespace: Option<&str>,
        local_name: &str,
    ) -> Result<Option<Attribute>, DomError> {
        let index = self.checked_index(element)?;
        let NodeKind::Element(element_data) = &mut self.nodes[index].kind else {
            return Err(DomError::NotAnElement);
        };
        let Some(position) = element_data.attributes.iter().position(|attribute| {
            attribute.name.namespace.as_deref() == namespace
                && attribute.name.local_name == local_name
        }) else {
            return Ok(None);
        };
        let removed = element_data.attributes.remove(position);
        self.bump_revision();
        Ok(Some(removed))
    }

    pub fn attribute(
        &self,
        element: NodeId,
        namespace: Option<&str>,
        local_name: &str,
    ) -> Result<Option<&str>, DomError> {
        let NodeKind::Element(element_data) = self.node_kind(element)? else {
            return Err(DomError::NotAnElement);
        };
        Ok(element_data.attribute(namespace, local_name))
    }

    pub fn append_character_data(&mut self, node: NodeId, suffix: &str) -> Result<(), DomError> {
        let index = self.checked_index(node)?;
        match &mut self.nodes[index].kind {
            NodeKind::Text(data) | NodeKind::Comment(data) => data.push_str(suffix),
            _ => return Err(DomError::NotCharacterData),
        }
        self.bump_revision();
        Ok(())
    }

    pub fn set_character_data(
        &mut self,
        node: NodeId,
        data: impl Into<String>,
    ) -> Result<String, DomError> {
        let index = self.checked_index(node)?;
        let data = data.into();
        let old = match &mut self.nodes[index].kind {
            NodeKind::Text(old) | NodeKind::Comment(old) => std::mem::replace(old, data),
            _ => return Err(DomError::NotCharacterData),
        };
        self.bump_revision();
        Ok(old)
    }

    /// Appends text, coalescing with an adjacent text child as the HTML parser does.
    pub fn append_text(&mut self, parent: NodeId, data: &str) -> Result<NodeId, DomError> {
        self.checked_index(parent)?;
        if let Some(last) = self.last_child(parent)?
            && matches!(self.node_kind(last)?, NodeKind::Text(_))
        {
            self.append_character_data(last, data)?;
            return Ok(last);
        }
        let text = self.create_text(data)?;
        self.append_child(parent, text)?;
        Ok(text)
    }

    pub fn document_element(&self) -> Option<NodeId> {
        self.nodes[0]
            .children
            .iter()
            .copied()
            .find(|node| self.nodes[node.slot as usize].kind.is_element())
    }

    pub fn doctype(&self) -> Option<NodeId> {
        self.nodes[0].children.iter().copied().find(|node| {
            matches!(
                self.nodes[node.slot as usize].kind,
                NodeKind::DocumentType(_)
            )
        })
    }

    pub fn document_order(&self) -> Result<Vec<NodeId>, DomError> {
        let mut result = Vec::new();
        let mut pending = vec![self.document_node];
        while let Some(node) = pending.pop() {
            result.push(node);
            pending.extend(self.children(node)?.iter().rev().copied());
        }
        Ok(result)
    }

    pub fn element_by_id(&self, id: &str) -> Result<Option<NodeId>, DomError> {
        for node in self.document_order()?.into_iter().skip(1) {
            if let NodeKind::Element(element) = self.node_kind(node)?
                && element.html_attribute("id") == Some(id)
            {
                return Ok(Some(node));
            }
        }
        Ok(None)
    }

    pub fn elements_by_tag_name(&self, local_name: &str) -> Result<Vec<NodeId>, DomError> {
        let wildcard = local_name == "*";
        let mut result = Vec::new();
        for node in self.document_order()?.into_iter().skip(1) {
            if let NodeKind::Element(element) = self.node_kind(node)?
                && (wildcard
                    || (element.name.namespace == Namespace::Html
                        && element.name.local_name.eq_ignore_ascii_case(local_name))
                    || element.name.local_name == local_name)
            {
                result.push(node);
            }
        }
        Ok(result)
    }

    /// DOM-like text content. Documents and doctypes return `None`.
    pub fn text_content(&self, node: NodeId) -> Result<Option<String>, DomError> {
        match self.node_kind(node)? {
            NodeKind::Document | NodeKind::DocumentType(_) => Ok(None),
            NodeKind::Text(data) | NodeKind::Comment(data) => Ok(Some(data.clone())),
            NodeKind::Element(_) => {
                let mut content = String::new();
                let mut pending = Vec::new();
                pending.extend(self.children(node)?.iter().rev().copied());
                while let Some(descendant) = pending.pop() {
                    match self.node_kind(descendant)? {
                        NodeKind::Text(data) => content.push_str(data),
                        NodeKind::Element(_) => {
                            pending.extend(self.children(descendant)?.iter().rev().copied())
                        }
                        _ => {}
                    }
                }
                Ok(Some(content))
            }
        }
    }

    /// Checks bidirectional parent/child links, uniqueness, and document shape.
    pub fn validate_invariants(&self) -> Result<(), DomError> {
        if self.nodes[0].parent.is_some() || !matches!(self.nodes[0].kind, NodeKind::Document) {
            return Err(DomError::SnapshotInvariant("invalid document node"));
        }
        self.validate_children(self.document_node, &self.nodes[0].children)?;
        let mut seen_as_child = HashSet::new();
        for (slot, record) in self.nodes.iter().enumerate() {
            let parent = NodeId {
                document: self.id,
                slot: slot as u32,
            };
            if !record.kind.is_container() && !record.children.is_empty() {
                return Err(DomError::SnapshotInvariant("leaf node has children"));
            }
            for child in &record.children {
                let child_index = self.checked_index(*child)?;
                if !seen_as_child.insert(*child) {
                    return Err(DomError::SnapshotInvariant("node has multiple parents"));
                }
                if self.nodes[child_index].parent != Some(parent) {
                    return Err(DomError::SnapshotInvariant("parent/child links disagree"));
                }
            }
        }
        for (slot, record) in self.nodes.iter().enumerate().skip(1) {
            let node = NodeId {
                document: self.id,
                slot: slot as u32,
            };
            if record.parent.is_some() != seen_as_child.contains(&node) {
                return Err(DomError::SnapshotInvariant("orphan parent link"));
            }
            let mut ancestors = HashSet::new();
            let mut cursor = record.parent;
            while let Some(ancestor) = cursor {
                if !ancestors.insert(ancestor) {
                    return Err(DomError::SnapshotInvariant("cycle in parent chain"));
                }
                cursor = self.parent(ancestor)?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<DocumentSnapshot, DomError> {
        self.validate_invariants()?;
        let order = self.document_order()?;
        let mut index = HashMap::with_capacity(order.len());
        let mut nodes = Vec::with_capacity(order.len());
        for id in order {
            let record = self.record(id)?;
            index.insert(id, nodes.len());
            nodes.push(SnapshotNode {
                id,
                parent: record.parent,
                children: record.children.clone(),
                kind: record.kind.clone(),
            });
        }
        Ok(DocumentSnapshot {
            document_id: self.id,
            revision: self.revision,
            document_node: self.document_node,
            nodes,
            index,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub kind: NodeKind,
}

/// Owned immutable view consumed by layout and other read-only subsystems.
#[derive(Clone, Debug)]
pub struct DocumentSnapshot {
    document_id: DocumentId,
    revision: u64,
    document_node: NodeId,
    nodes: Vec<SnapshotNode>,
    index: HashMap<NodeId, usize>,
}

impl DocumentSnapshot {
    pub const fn document_id(&self) -> DocumentId {
        self.document_id
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the exact document identity and local revision in this snapshot.
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        DocumentVersion::new(self.document_id, self.revision)
    }

    pub const fn document_node(&self) -> NodeId {
        self.document_node
    }

    pub fn nodes_in_document_order(&self) -> &[SnapshotNode] {
        &self.nodes
    }

    pub fn node(&self, id: NodeId) -> Option<&SnapshotNode> {
        self.index.get(&id).map(|index| &self.nodes[*index])
    }

    pub fn document_element(&self) -> Option<NodeId> {
        self.node(self.document_node)?
            .children
            .iter()
            .copied()
            .find(|id| {
                self.node(*id)
                    .is_some_and(|node| matches!(node.kind, NodeKind::Element(_)))
            })
    }
}

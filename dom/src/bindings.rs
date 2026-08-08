//! Host-side contract for a future JavaScript binding layer.
//!
//! `NodeId` is a stable lookup key, but it is deliberately **not** a GC root.
//! A JavaScript runtime adapter must produce an owned `RootedNodeHandle` whose
//! lifetime keeps the host document alive and whose tracing implementation
//! reports every live DOM edge. This keeps unrooted engine pointers out of the
//! public boundary without making this crate depend on a particular JS engine.

use crate::{DocumentId, NodeId};

/// An owned, traceable reference maintained by a JS/DOM integration adapter.
///
/// Implementations must keep the referenced document alive until the last
/// clone is dropped. They must not contain an unrooted pointer into either the
/// DOM arena or a moving garbage-collected heap.
pub trait RootedNodeHandle: Clone + Send + Sync + 'static {
    /// The stable identity of the owning document.
    fn document_id(&self) -> DocumentId;

    /// The stable node lookup key within that document.
    fn node_id(&self) -> NodeId;
}

/// Capability supplied by the eventual JS/DOM integration layer.
///
/// No implementation is provided for `NodeId`: turning a lookup key into a
/// root is an ownership operation that only the embedding can perform.
pub trait DomRootProvider {
    type Root: RootedNodeHandle;
    type Error;

    /// Acquires an owned root after checking document identity and liveness.
    fn root_node(&self, node: NodeId) -> Result<Self::Root, Self::Error>;

    /// Returns whether an existing root still denotes a host-visible node.
    fn is_live(&self, root: &Self::Root) -> bool;
}

/// A visitor used by binding objects to expose their rooted host edges.
pub trait DomRootTrace {
    /// Visits every DOM root held by the binding object exactly once per edge.
    fn trace_dom_roots(&self, visitor: &mut dyn FnMut(NodeId));
}

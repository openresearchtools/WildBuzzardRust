//! Typed service identities and thread-safe service registries.
//!
//! Service users receive an [`Arc`] cloned from a registry lookup. No raw
//! pointer, borrowed registry entry, runtime interface query, or process-local
//! address is exposed.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use wild_buzzard_handles::{Arena, Handle, InsertError, InvalidHandle, RawHandle};

/// A stable, non-zero identifier for one service contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceKind(u128);

impl ServiceKind {
    /// Defines a service kind. Values must be globally unique in Wild Buzzard.
    ///
    /// # Panics
    ///
    /// Panics during constant evaluation or runtime if `value` is zero.
    #[must_use]
    pub const fn new(value: u128) -> Self {
        assert!(value != 0, "service kind zero is reserved");
        Self(value)
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// A non-zero namespace separating service registries and processes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceNamespace(NonZeroU64);

impl ServiceNamespace {
    /// Creates a namespace, rejecting the reserved zero value.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Declares the interface and stable kind represented by a service identity.
pub trait ServiceSpec: 'static {
    /// The thread-safe interface stored by the registry.
    type Interface: ?Sized + Send + Sync + 'static;

    /// A project-wide unique contract identifier.
    const KIND: ServiceKind;

    /// A diagnostic name. It is never used for identity comparison.
    const NAME: &'static str;
}

/// A validated, transport-safe representation of a service identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WireServiceIdentity {
    kind: ServiceKind,
    namespace: ServiceNamespace,
    handle: RawHandle,
}

impl WireServiceIdentity {
    /// Validates integer wire fields before constructing an identity.
    pub fn from_parts(
        kind: u128,
        namespace: u64,
        slot: u32,
        generation: u32,
    ) -> Result<Self, IdentityDecodeError> {
        if kind == 0 {
            return Err(IdentityDecodeError::ZeroKind);
        }
        let namespace =
            ServiceNamespace::new(namespace).ok_or(IdentityDecodeError::ZeroNamespace)?;
        let handle = RawHandle::new(slot, generation)
            .map_err(|InvalidHandle::ZeroGeneration| IdentityDecodeError::ZeroGeneration)?;
        Ok(Self {
            kind: ServiceKind(kind),
            namespace,
            handle,
        })
    }

    /// Returns the service contract kind.
    #[must_use]
    pub const fn kind(self) -> ServiceKind {
        self.kind
    }

    /// Returns the registry namespace.
    #[must_use]
    pub const fn namespace(self) -> ServiceNamespace {
        self.namespace
    }

    /// Returns the zero-based registry slot.
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.handle.slot()
    }

    /// Returns the non-zero slot generation.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.handle.generation()
    }
}

/// A typed identity for one registered service instance.
pub struct ServiceId<S: ServiceSpec> {
    namespace: ServiceNamespace,
    handle: Handle<Arc<S::Interface>>,
    marker: PhantomData<fn() -> S>,
}

impl<S: ServiceSpec> ServiceId<S> {
    /// Erases only the Rust marker, retaining the service kind on the wire.
    #[must_use]
    pub fn to_wire(self) -> WireServiceIdentity {
        WireServiceIdentity {
            kind: S::KIND,
            namespace: self.namespace,
            handle: self.handle.into_raw(),
        }
    }

    /// Restores a typed identity after verifying its service kind.
    pub fn try_from_wire(wire: WireServiceIdentity) -> Result<Self, IdentityDecodeError> {
        if wire.kind != S::KIND {
            return Err(IdentityDecodeError::UnexpectedKind {
                expected: S::KIND,
                actual: wire.kind,
            });
        }
        Ok(Self {
            namespace: wire.namespace,
            handle: Handle::from_raw(wire.handle),
            marker: PhantomData,
        })
    }

    /// Returns the registry namespace.
    #[must_use]
    pub const fn namespace(self) -> ServiceNamespace {
        self.namespace
    }

    /// Returns the zero-based registry slot.
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.handle.slot()
    }

    /// Returns the non-zero slot generation.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.handle.generation()
    }
}

impl<S: ServiceSpec> Clone for ServiceId<S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: ServiceSpec> Copy for ServiceId<S> {}

impl<S: ServiceSpec> fmt::Debug for ServiceId<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceId")
            .field("service", &S::NAME)
            .field("kind", &S::KIND)
            .field("namespace", &self.namespace)
            .field("slot", &self.handle.slot())
            .field("generation", &self.handle.generation())
            .finish()
    }
}

impl<S: ServiceSpec> PartialEq for ServiceId<S> {
    fn eq(&self, other: &Self) -> bool {
        self.namespace == other.namespace && self.handle == other.handle
    }
}

impl<S: ServiceSpec> Eq for ServiceId<S> {}

impl<S: ServiceSpec> Hash for ServiceId<S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.namespace.hash(state);
        self.handle.hash(state);
    }
}

/// A rejected service identity received from a typed or wire boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityDecodeError {
    /// Service kind zero is reserved.
    ZeroKind,
    /// Namespace zero is reserved.
    ZeroNamespace,
    /// Generation zero is reserved.
    ZeroGeneration,
    /// The identity names a different typed service contract.
    UnexpectedKind {
        /// Required service kind.
        expected: ServiceKind,
        /// Received service kind.
        actual: ServiceKind,
    },
}

impl fmt::Display for IdentityDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroKind => formatter.write_str("service kind zero is reserved"),
            Self::ZeroNamespace => formatter.write_str("service namespace zero is reserved"),
            Self::ZeroGeneration => formatter.write_str("service generation zero is reserved"),
            Self::UnexpectedKind { expected, actual } => write!(
                formatter,
                "unexpected service kind: expected {}, received {}",
                expected.get(),
                actual.get()
            ),
        }
    }
}

impl Error for IdentityDecodeError {}

/// A duplicate typed service-contract assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DuplicateServiceKind {
    /// Colliding stable kind.
    pub kind: ServiceKind,
    /// Name registered first.
    pub existing_name: &'static str,
    /// Name rejected by this registration.
    pub attempted_name: &'static str,
}

impl fmt::Display for DuplicateServiceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "service kind {} is already assigned to {}; cannot assign it to {}",
            self.kind.get(),
            self.existing_name,
            self.attempted_name
        )
    }
}

impl Error for DuplicateServiceKind {}

/// A caller-owned table validating stable typed service-kind assignments.
///
/// This is deliberately not mutable global state. An integrating process
/// builds it from the orchestrator-owned checked-in contract registry and
/// rejects collisions before accepting service traffic.
#[derive(Debug, Default)]
pub struct ServiceContractRegistry {
    contracts: BTreeMap<ServiceKind, &'static str>,
}

impl ServiceContractRegistry {
    /// Creates an empty caller-owned contract table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            contracts: BTreeMap::new(),
        }
    }

    /// Registers one typed service contract, rejecting a kind collision.
    pub fn register<S: ServiceSpec>(&mut self) -> Result<(), DuplicateServiceKind> {
        if let Some(existing_name) = self.contracts.get(&S::KIND) {
            return Err(DuplicateServiceKind {
                kind: S::KIND,
                existing_name,
                attempted_name: S::NAME,
            });
        }
        self.contracts.insert(S::KIND, S::NAME);
        Ok(())
    }

    /// Returns the diagnostic name for a registered kind.
    #[must_use]
    pub fn name(&self, kind: ServiceKind) -> Option<&'static str> {
        self.contracts.get(&kind).copied()
    }

    /// Returns the number of unique contracts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.contracts.len()
    }

    /// Returns whether no contracts are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.contracts.is_empty()
    }
}

/// A service registration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterError {
    /// The registry lock was poisoned by a panicking owner.
    RegistryPoisoned,
    /// No additional generational slot can be represented.
    CapacityExhausted,
}

impl fmt::Display for RegisterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegistryPoisoned => formatter.write_str("service registry lock is poisoned"),
            Self::CapacityExhausted => formatter.write_str("service registry capacity exhausted"),
        }
    }
}

impl Error for RegisterError {}

/// A service lookup or removal failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupError {
    /// The identity belongs to another registry or process namespace.
    ForeignNamespace {
        /// Namespace of this registry.
        expected: ServiceNamespace,
        /// Namespace carried by the identity.
        actual: ServiceNamespace,
    },
    /// The slot is absent or its generation has been invalidated.
    StaleIdentity,
    /// The registry lock was poisoned by a panicking owner.
    RegistryPoisoned,
}

impl fmt::Display for LookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignNamespace { expected, actual } => write!(
                formatter,
                "foreign service namespace: expected {}, received {}",
                expected.get(),
                actual.get()
            ),
            Self::StaleIdentity => formatter.write_str("service identity is stale"),
            Self::RegistryPoisoned => formatter.write_str("service registry lock is poisoned"),
        }
    }
}

impl Error for LookupError {}

/// A typed, thread-safe owner of services for one namespace and contract.
///
/// `ServiceRegistry<S>` is `Send + Sync` because [`ServiceSpec::Interface`]
/// must be `Send + Sync`. Resolving returns an owned `Arc`, so concurrent
/// unregister cannot invalidate a service already obtained by a caller.
pub struct ServiceRegistry<S: ServiceSpec> {
    namespace: ServiceNamespace,
    entries: RwLock<Arena<Arc<S::Interface>>>,
    marker: PhantomData<fn() -> S>,
}

impl<S: ServiceSpec> ServiceRegistry<S> {
    /// Creates an empty registry in an explicitly assigned namespace.
    #[must_use]
    pub fn new(namespace: ServiceNamespace) -> Self {
        Self {
            namespace,
            entries: RwLock::new(Arena::new()),
            marker: PhantomData,
        }
    }

    /// Returns this registry's namespace.
    #[must_use]
    pub const fn namespace(&self) -> ServiceNamespace {
        self.namespace
    }

    /// Registers a service and issues a typed generational identity.
    pub fn register(&self, service: Arc<S::Interface>) -> Result<ServiceId<S>, RegisterError> {
        let mut entries = self
            .entries
            .write()
            .map_err(|_| RegisterError::RegistryPoisoned)?;
        let handle = entries
            .try_insert(service)
            .map_err(|InsertError::CapacityExhausted| RegisterError::CapacityExhausted)?;
        Ok(ServiceId {
            namespace: self.namespace,
            handle,
            marker: PhantomData,
        })
    }

    /// Resolves a live service to an owned, thread-safe reference.
    pub fn resolve(&self, id: ServiceId<S>) -> Result<Arc<S::Interface>, LookupError> {
        self.check_namespace(id)?;
        let entries = self
            .entries
            .read()
            .map_err(|_| LookupError::RegistryPoisoned)?;
        entries
            .get(id.handle)
            .cloned()
            .ok_or(LookupError::StaleIdentity)
    }

    /// Removes a service and invalidates its identity before slot reuse.
    pub fn unregister(&self, id: ServiceId<S>) -> Result<Arc<S::Interface>, LookupError> {
        self.check_namespace(id)?;
        let mut entries = self
            .entries
            .write()
            .map_err(|_| LookupError::RegistryPoisoned)?;
        entries.remove(id.handle).ok_or(LookupError::StaleIdentity)
    }

    /// Returns the current number of registered instances.
    pub fn len(&self) -> Result<usize, LookupError> {
        let entries = self
            .entries
            .read()
            .map_err(|_| LookupError::RegistryPoisoned)?;
        Ok(entries.len())
    }

    /// Returns whether no service instances are registered.
    pub fn is_empty(&self) -> Result<bool, LookupError> {
        self.len().map(|len| len == 0)
    }

    fn check_namespace(&self, id: ServiceId<S>) -> Result<(), LookupError> {
        if id.namespace == self.namespace {
            Ok(())
        } else {
            Err(LookupError::ForeignNamespace {
                expected: self.namespace,
                actual: id.namespace,
            })
        }
    }
}

impl<S: ServiceSpec> fmt::Debug for ServiceRegistry<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceRegistry")
            .field("service", &S::NAME)
            .field("kind", &S::KIND)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DuplicateServiceKind, IdentityDecodeError, LookupError, ServiceContractRegistry, ServiceId,
        ServiceKind, ServiceNamespace, ServiceRegistry, ServiceSpec, WireServiceIdentity,
    };
    use std::sync::{Arc, Barrier};
    use std::thread;

    trait Greeting: Send + Sync {
        fn greeting(&self) -> &'static str;
    }

    struct Greeter;

    impl Greeting for Greeter {
        fn greeting(&self) -> &'static str {
            "hello"
        }
    }

    struct GreetingService;

    impl ServiceSpec for GreetingService {
        type Interface = dyn Greeting;

        const KIND: ServiceKind = ServiceKind::new(0x1bb2_47e9_d237_4a4f_a6f0_9674_1e28_0101);
        const NAME: &'static str = "test.greeting";
    }

    struct OtherService;

    impl ServiceSpec for OtherService {
        type Interface = dyn Greeting;

        const KIND: ServiceKind = ServiceKind::new(0x1bb2_47e9_d237_4a4f_a6f0_9674_1e28_0102);
        const NAME: &'static str = "test.other";
    }

    struct CollidingService;

    impl ServiceSpec for CollidingService {
        type Interface = dyn Greeting;

        const KIND: ServiceKind = GreetingService::KIND;
        const NAME: &'static str = "test.collision";
    }

    fn namespace(value: u64) -> ServiceNamespace {
        ServiceNamespace::new(value).unwrap()
    }

    #[test]
    fn stale_service_identity_is_rejected_after_reuse() {
        let registry = ServiceRegistry::<GreetingService>::new(namespace(7));
        let first = registry.register(Arc::new(Greeter)).unwrap();

        let removed = registry.unregister(first).unwrap();
        assert_eq!(removed.greeting(), "hello");
        assert!(matches!(
            registry.resolve(first),
            Err(LookupError::StaleIdentity)
        ));

        let second = registry.register(Arc::new(Greeter)).unwrap();
        assert_eq!(first.slot(), second.slot());
        assert_ne!(first.generation(), second.generation());
        assert!(matches!(
            registry.resolve(first),
            Err(LookupError::StaleIdentity)
        ));
        assert_eq!(registry.resolve(second).unwrap().greeting(), "hello");
    }

    #[test]
    fn foreign_namespace_is_rejected() {
        let first_registry = ServiceRegistry::<GreetingService>::new(namespace(11));
        let second_registry = ServiceRegistry::<GreetingService>::new(namespace(12));
        let id = first_registry.register(Arc::new(Greeter)).unwrap();

        match second_registry.resolve(id) {
            Err(LookupError::ForeignNamespace { expected, actual }) => {
                assert_eq!(expected, namespace(12));
                assert_eq!(actual, namespace(11));
            }
            Ok(_) | Err(_) => panic!("expected a foreign-namespace error"),
        }
    }

    #[test]
    fn wire_identity_must_match_typed_contract() {
        let registry = ServiceRegistry::<GreetingService>::new(namespace(20));
        let id = registry.register(Arc::new(Greeter)).unwrap();
        let wire = id.to_wire();

        assert_eq!(ServiceId::<GreetingService>::try_from_wire(wire), Ok(id));
        assert_eq!(
            ServiceId::<OtherService>::try_from_wire(wire),
            Err(IdentityDecodeError::UnexpectedKind {
                expected: OtherService::KIND,
                actual: GreetingService::KIND,
            })
        );
    }

    #[test]
    fn malformed_wire_identity_is_rejected() {
        assert_eq!(
            WireServiceIdentity::from_parts(0, 1, 0, 1),
            Err(IdentityDecodeError::ZeroKind)
        );
        assert_eq!(
            WireServiceIdentity::from_parts(1, 0, 0, 1),
            Err(IdentityDecodeError::ZeroNamespace)
        );
        assert_eq!(
            WireServiceIdentity::from_parts(1, 1, 0, 0),
            Err(IdentityDecodeError::ZeroGeneration)
        );
    }

    #[test]
    fn caller_owned_contract_registry_rejects_kind_collisions() {
        let mut contracts = ServiceContractRegistry::new();
        contracts.register::<GreetingService>().unwrap();
        assert_eq!(
            contracts.register::<CollidingService>(),
            Err(DuplicateServiceKind {
                kind: GreetingService::KIND,
                existing_name: GreetingService::NAME,
                attempted_name: CollidingService::NAME,
            })
        );
        assert_eq!(contracts.name(GreetingService::KIND), Some("test.greeting"));
        assert_eq!(contracts.len(), 1);
    }

    #[test]
    fn resolve_unregister_race_never_returns_a_dangling_reference() {
        let registry = Arc::new(ServiceRegistry::<GreetingService>::new(namespace(30)));
        let id = registry.register(Arc::new(Greeter)).unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let resolver_registry = Arc::clone(&registry);
        let resolver_barrier = Arc::clone(&barrier);
        let resolver = thread::spawn(move || {
            resolver_barrier.wait();
            resolver_registry.resolve(id)
        });

        barrier.wait();
        let removed = registry.unregister(id).unwrap();
        assert_eq!(removed.greeting(), "hello");

        match resolver.join().unwrap() {
            Ok(service) => assert_eq!(service.greeting(), "hello"),
            Err(error) => assert_eq!(error, LookupError::StaleIdentity),
        }
        assert!(matches!(
            registry.resolve(id),
            Err(LookupError::StaleIdentity)
        ));
    }

    #[test]
    fn registry_and_ids_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ServiceRegistry<GreetingService>>();
        assert_send_sync::<ServiceId<GreetingService>>();
    }
}

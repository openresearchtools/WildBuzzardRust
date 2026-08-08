use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::Function;
use crate::runtime::HostFunction;
use crate::string::JsString;

pub(crate) struct Handle<T> {
    index: usize,
    generation: u32,
    marker: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    const fn new(index: usize, generation: u32) -> Self {
        Self {
            index,
            generation,
            marker: PhantomData,
        }
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.generation == other.generation
    }
}

impl<T> Eq for Handle<T> {}

impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
        self.generation.hash(state);
    }
}

impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Handle")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .finish()
    }
}

enum SlotState<T> {
    Occupied { value: T, marked: bool },
    Tombstone { next: Option<usize> },
    Retired,
}

struct Slot<T> {
    generation: u32,
    state: SlotState<T>,
}

struct Arena<T> {
    slots: Vec<Slot<T>>,
    free_head: Option<usize>,
    live: usize,
    free: usize,
    retired: usize,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free_head: None,
            live: 0,
            free: 0,
            retired: 0,
        }
    }
}

impl<T> Arena<T> {
    fn insert(&mut self, value: T) -> Handle<T> {
        if let Some(index) = self.free_head {
            let slot = &mut self.slots[index];
            let SlotState::Tombstone { next } = slot.state else {
                unreachable!("arena free list must contain only tombstones");
            };
            self.free_head = next;
            slot.state = SlotState::Occupied {
                value,
                marked: false,
            };
            self.live += 1;
            self.free -= 1;
            return Handle::new(index, slot.generation);
        }

        let handle = Handle::new(self.slots.len(), 0);
        self.slots.push(Slot {
            generation: 0,
            state: SlotState::Occupied {
                value,
                marked: false,
            },
        });
        self.live += 1;
        handle
    }

    fn get(&self, handle: Handle<T>) -> Option<&T> {
        let slot = self.slots.get(handle.index)?;
        if slot.generation != handle.generation {
            return None;
        }
        match &slot.state {
            SlotState::Occupied { value, .. } => Some(value),
            SlotState::Tombstone { .. } | SlotState::Retired => None,
        }
    }

    fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        let slot = self.slots.get_mut(handle.index)?;
        if slot.generation != handle.generation {
            return None;
        }
        match &mut slot.state {
            SlotState::Occupied { value, .. } => Some(value),
            SlotState::Tombstone { .. } | SlotState::Retired => None,
        }
    }

    fn mark(&mut self, handle: Handle<T>) -> Result<bool, ()> {
        let slot = self.slots.get_mut(handle.index).ok_or(())?;
        if slot.generation != handle.generation {
            return Err(());
        }
        match &mut slot.state {
            SlotState::Occupied { marked, .. } => {
                let was_unmarked = !*marked;
                *marked = true;
                Ok(was_unmarked)
            }
            SlotState::Tombstone { .. } | SlotState::Retired => Err(()),
        }
    }

    fn clear_marks(&mut self) {
        for slot in &mut self.slots {
            if let SlotState::Occupied { marked, .. } = &mut slot.state {
                *marked = false;
            }
        }
    }

    fn sweep(&mut self) -> usize {
        let mut reclaimed = 0;
        for index in 0..self.slots.len() {
            let should_reclaim = match &mut self.slots[index].state {
                SlotState::Occupied { marked, .. } if *marked => {
                    *marked = false;
                    false
                }
                SlotState::Occupied { .. } => true,
                SlotState::Tombstone { .. } | SlotState::Retired => false,
            };
            if !should_reclaim {
                continue;
            }

            let slot = &mut self.slots[index];
            slot.state = if let Some(next_generation) = slot.generation.checked_add(1) {
                slot.generation = next_generation;
                let next = self.free_head;
                self.free_head = Some(index);
                self.free += 1;
                SlotState::Tombstone { next }
            } else {
                self.retired += 1;
                SlotState::Retired
            };
            self.live -= 1;
            reclaimed += 1;
        }
        reclaimed
    }

    const fn live_len(&self) -> usize {
        self.live
    }

    fn statistics(&self) -> ArenaStatistics {
        ArenaStatistics {
            capacity: self.slots.len(),
            live: self.live,
            free: self.free,
            retired: self.retired,
        }
    }
}

pub(crate) type StringId = Handle<StringRecord>;
pub(crate) type ObjectId = Handle<ObjectRecord>;
pub(crate) type FunctionId = Handle<FunctionRecord>;
pub(crate) type EnvironmentId = Handle<EnvironmentRecord>;

#[derive(Clone, Debug)]
pub(crate) struct StringRecord {
    contents: JsString,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RawValue {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(StringId),
    Object(ObjectId),
    Function(FunctionId),
}

impl RawValue {
    pub(crate) const fn type_name(self) -> &'static str {
        match self {
            Self::Undefined => "undefined",
            Self::Null | Self::Object(_) => "object",
            Self::Boolean(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Function(_) => "function",
        }
    }

    pub(crate) const fn as_object_ref(self) -> Option<ObjectRef> {
        match self {
            Self::Object(id) => Some(ObjectRef::Object(id)),
            Self::Function(id) => Some(ObjectRef::Function(id)),
            Self::Undefined | Self::Null | Self::Boolean(_) | Self::Number(_) | Self::String(_) => {
                None
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ObjectRef {
    Object(ObjectId),
    Function(FunctionId),
}

impl ObjectRef {
    pub(crate) const fn as_value(self) -> RawValue {
        match self {
            Self::Object(id) => RawValue::Object(id),
            Self::Function(id) => RawValue::Function(id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PropertyKind {
    Data {
        value: RawValue,
        writable: bool,
    },
    Accessor {
        getter: Option<RawValue>,
        setter: Option<RawValue>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PropertyDescriptor {
    pub kind: PropertyKind,
    pub enumerable: bool,
    pub configurable: bool,
}

impl PropertyDescriptor {
    pub(crate) const fn data(
        value: RawValue,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    ) -> Self {
        Self {
            kind: PropertyKind::Data { value, writable },
            enumerable,
            configurable,
        }
    }

    pub(crate) const fn default_data(value: RawValue) -> Self {
        Self::data(value, true, true, true)
    }

    pub(crate) const fn accessor(
        getter: Option<RawValue>,
        setter: Option<RawValue>,
        enumerable: bool,
        configurable: bool,
    ) -> Self {
        Self {
            kind: PropertyKind::Accessor { getter, setter },
            enumerable,
            configurable,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObjectData {
    pub prototype: Option<ObjectRef>,
    pub extensible: bool,
    pub properties: HashMap<JsString, PropertyDescriptor>,
}

impl ObjectData {
    fn new(
        prototype: Option<ObjectRef>,
        properties: HashMap<JsString, PropertyDescriptor>,
    ) -> Self {
        Self {
            prototype,
            extensible: true,
            properties,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ArrayRecord {
    pub length: u32,
    pub length_writable: bool,
    pub elements: BTreeMap<u32, PropertyDescriptor>,
}

#[derive(Debug)]
pub(crate) enum ObjectKind {
    Ordinary,
    Array(ArrayRecord),
}

#[derive(Debug)]
pub(crate) struct ObjectRecord {
    pub data: ObjectData,
    pub kind: ObjectKind,
}

#[derive(Clone)]
pub(crate) struct ScriptFunction {
    pub function: Function,
    pub closure: EnvironmentId,
    pub source_name: Arc<str>,
}

#[derive(Clone)]
pub(crate) struct HostFunctionRecord {
    pub name: String,
    pub arity: usize,
    pub callback: Rc<dyn HostFunction>,
}

#[derive(Clone)]
pub(crate) enum Callable {
    Script(ScriptFunction),
    Host(HostFunctionRecord),
}

impl Callable {
    pub(crate) fn display_name(&self) -> &str {
        match self {
            Self::Script(script) => script.function.name.as_deref().unwrap_or("<anonymous>"),
            Self::Host(host) => &host.name,
        }
    }

    pub(crate) fn arity(&self) -> usize {
        match self {
            Self::Script(script) => script.function.parameters.len(),
            Self::Host(host) => host.arity,
        }
    }
}

#[derive(Clone)]
pub(crate) struct FunctionRecord {
    pub callable: Callable,
    pub data: ObjectData,
    pub constructible: bool,
}

impl FunctionRecord {
    pub(crate) fn display_name(&self) -> &str {
        self.callable.display_name()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BindingState {
    Uninitialized,
    Initialized(RawValue),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Binding {
    pub mutable: bool,
    pub state: BindingState,
}

#[derive(Debug)]
pub(crate) struct EnvironmentRecord {
    pub outer: Option<EnvironmentId>,
    pub bindings: HashMap<String, Binding>,
}

impl EnvironmentRecord {
    pub(crate) fn new(outer: Option<EnvironmentId>) -> Self {
        Self {
            outer,
            bindings: HashMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ArenaStatistics {
    pub capacity: usize,
    pub live: usize,
    pub free: usize,
    pub retired: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HeapArenaStatistics {
    pub strings: ArenaStatistics,
    pub objects: ArenaStatistics,
    pub functions: ArenaStatistics,
    pub environments: ArenaStatistics,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReclaimedCounts {
    pub strings: usize,
    pub objects: usize,
    pub functions: usize,
    pub environments: usize,
}

impl ReclaimedCounts {
    fn add_assign(&mut self, other: Self) {
        self.strings = self.strings.saturating_add(other.strings);
        self.objects = self.objects.saturating_add(other.objects);
        self.functions = self.functions.saturating_add(other.functions);
        self.environments = self.environments.saturating_add(other.environments);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocationKind {
    String,
    Object,
    Function,
    Environment,
}

impl AllocationKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Object => "object",
            Self::Function => "function",
            Self::Environment => "environment",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TraceError {
    pub kind: AllocationKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HeapCollection {
    pub reclaimed: ReclaimedCounts,
}

enum TraceNode {
    Value(RawValue),
    Environment(EnvironmentId),
}

#[derive(Default)]
pub(crate) struct Heap {
    strings: Arena<StringRecord>,
    objects: Arena<ObjectRecord>,
    functions: Arena<FunctionRecord>,
    environments: Arena<EnvironmentRecord>,
    collections: u64,
    total_reclaimed: ReclaimedCounts,
}

impl Heap {
    pub(crate) fn allocate_string(&mut self, value: JsString) -> RawValue {
        RawValue::String(self.strings.insert(StringRecord { contents: value }))
    }

    pub(crate) fn string(&self, id: StringId) -> Option<&JsString> {
        self.strings.get(id).map(|record| &record.contents)
    }

    pub(crate) fn allocate_object(
        &mut self,
        prototype: Option<ObjectRef>,
        properties: HashMap<JsString, PropertyDescriptor>,
    ) -> RawValue {
        RawValue::Object(self.objects.insert(ObjectRecord {
            data: ObjectData::new(prototype, properties),
            kind: ObjectKind::Ordinary,
        }))
    }

    pub(crate) fn allocate_array(
        &mut self,
        prototype: Option<ObjectRef>,
        length: u32,
        elements: BTreeMap<u32, PropertyDescriptor>,
    ) -> RawValue {
        RawValue::Object(self.objects.insert(ObjectRecord {
            data: ObjectData::new(prototype, HashMap::new()),
            kind: ObjectKind::Array(ArrayRecord {
                length,
                length_writable: true,
                elements,
            }),
        }))
    }

    pub(crate) fn object(&self, id: ObjectId) -> Option<&ObjectRecord> {
        self.objects.get(id)
    }

    pub(crate) fn object_mut(&mut self, id: ObjectId) -> Option<&mut ObjectRecord> {
        self.objects.get_mut(id)
    }

    pub(crate) fn allocate_function(
        &mut self,
        callable: Callable,
        property_name: JsString,
        prototype: Option<ObjectRef>,
        constructible: bool,
    ) -> RawValue {
        let arity = callable.arity();
        let name = self.allocate_string(property_name);
        let properties = HashMap::from([
            (
                JsString::from_runtime_utf8("name"),
                PropertyDescriptor::data(name, false, false, true),
            ),
            (
                JsString::from_runtime_utf8("length"),
                PropertyDescriptor::data(
                    RawValue::Number(usize_to_number(arity)),
                    false,
                    false,
                    true,
                ),
            ),
        ]);
        RawValue::Function(self.functions.insert(FunctionRecord {
            callable,
            data: ObjectData::new(prototype, properties),
            constructible,
        }))
    }

    pub(crate) fn function(&self, id: FunctionId) -> Option<&FunctionRecord> {
        self.functions.get(id)
    }

    pub(crate) fn function_mut(&mut self, id: FunctionId) -> Option<&mut FunctionRecord> {
        self.functions.get_mut(id)
    }

    pub(crate) fn object_data(&self, object: ObjectRef) -> Option<&ObjectData> {
        match object {
            ObjectRef::Object(id) => self.object(id).map(|record| &record.data),
            ObjectRef::Function(id) => self.function(id).map(|record| &record.data),
        }
    }

    pub(crate) fn object_data_mut(&mut self, object: ObjectRef) -> Option<&mut ObjectData> {
        match object {
            ObjectRef::Object(id) => self.object_mut(id).map(|record| &mut record.data),
            ObjectRef::Function(id) => self.function_mut(id).map(|record| &mut record.data),
        }
    }

    pub(crate) fn allocate_environment(&mut self, outer: Option<EnvironmentId>) -> EnvironmentId {
        self.environments.insert(EnvironmentRecord::new(outer))
    }

    pub(crate) fn environment(&self, id: EnvironmentId) -> Option<&EnvironmentRecord> {
        self.environments.get(id)
    }

    pub(crate) fn environment_mut(&mut self, id: EnvironmentId) -> Option<&mut EnvironmentRecord> {
        self.environments.get_mut(id)
    }

    pub(crate) const fn counts(&self) -> (usize, usize, usize, usize) {
        (
            self.strings.live_len(),
            self.objects.live_len(),
            self.functions.live_len(),
            self.environments.live_len(),
        )
    }

    pub(crate) fn arena_statistics(&self) -> HeapArenaStatistics {
        HeapArenaStatistics {
            strings: self.strings.statistics(),
            objects: self.objects.statistics(),
            functions: self.functions.statistics(),
            environments: self.environments.statistics(),
        }
    }

    pub(crate) const fn collection_count(&self) -> u64 {
        self.collections
    }

    pub(crate) const fn total_reclaimed(&self) -> ReclaimedCounts {
        self.total_reclaimed
    }

    pub(crate) fn collect(
        &mut self,
        roots: &[RawValue],
        permanent_environments: &[EnvironmentId],
    ) -> Result<HeapCollection, TraceError> {
        self.clear_marks();
        let traced = self.trace(roots, permanent_environments);
        if let Err(error) = traced {
            self.clear_marks();
            return Err(error);
        }

        let reclaimed = ReclaimedCounts {
            strings: self.strings.sweep(),
            objects: self.objects.sweep(),
            functions: self.functions.sweep(),
            environments: self.environments.sweep(),
        };
        self.collections = self.collections.saturating_add(1);
        self.total_reclaimed.add_assign(reclaimed);
        Ok(HeapCollection { reclaimed })
    }

    fn trace(
        &mut self,
        roots: &[RawValue],
        permanent_environments: &[EnvironmentId],
    ) -> Result<(), TraceError> {
        let mut worklist = Vec::with_capacity(roots.len() + permanent_environments.len());
        worklist.extend(roots.iter().copied().map(TraceNode::Value));
        worklist.extend(
            permanent_environments
                .iter()
                .copied()
                .map(TraceNode::Environment),
        );

        while let Some(node) = worklist.pop() {
            match node {
                TraceNode::Value(value) => self.trace_value(value, &mut worklist)?,
                TraceNode::Environment(environment) => {
                    if !self
                        .environments
                        .mark(environment)
                        .map_err(|()| trace_error(AllocationKind::Environment))?
                    {
                        continue;
                    }
                    let record = self
                        .environments
                        .get(environment)
                        .ok_or_else(|| trace_error(AllocationKind::Environment))?;
                    worklist.extend(record.outer.map(TraceNode::Environment));
                    worklist.extend(record.bindings.values().filter_map(|binding| {
                        let BindingState::Initialized(value) = binding.state else {
                            return None;
                        };
                        Some(TraceNode::Value(value))
                    }));
                }
            }
        }
        Ok(())
    }

    fn trace_value(
        &mut self,
        value: RawValue,
        worklist: &mut Vec<TraceNode>,
    ) -> Result<(), TraceError> {
        match value {
            RawValue::Undefined | RawValue::Null | RawValue::Boolean(_) | RawValue::Number(_) => {}
            RawValue::String(string) => {
                self.strings
                    .mark(string)
                    .map_err(|()| trace_error(AllocationKind::String))?;
            }
            RawValue::Object(object) => {
                if !self
                    .objects
                    .mark(object)
                    .map_err(|()| trace_error(AllocationKind::Object))?
                {
                    return Ok(());
                }
                let record = self
                    .objects
                    .get(object)
                    .ok_or_else(|| trace_error(AllocationKind::Object))?;
                trace_object_data(&record.data, worklist);
                if let ObjectKind::Array(array) = &record.kind {
                    for descriptor in array.elements.values() {
                        trace_property_descriptor(*descriptor, worklist);
                    }
                }
            }
            RawValue::Function(function) => {
                if !self
                    .functions
                    .mark(function)
                    .map_err(|()| trace_error(AllocationKind::Function))?
                {
                    return Ok(());
                }
                let record = self
                    .functions
                    .get(function)
                    .ok_or_else(|| trace_error(AllocationKind::Function))?;
                trace_object_data(&record.data, worklist);
                if let Callable::Script(script) = &record.callable {
                    worklist.push(TraceNode::Environment(script.closure));
                }
            }
        }
        Ok(())
    }

    fn clear_marks(&mut self) {
        self.strings.clear_marks();
        self.objects.clear_marks();
        self.functions.clear_marks();
        self.environments.clear_marks();
    }
}

const fn trace_error(kind: AllocationKind) -> TraceError {
    TraceError { kind }
}

fn trace_object_data(data: &ObjectData, worklist: &mut Vec<TraceNode>) {
    worklist.extend(
        data.prototype
            .map(ObjectRef::as_value)
            .map(TraceNode::Value),
    );
    for descriptor in data.properties.values() {
        trace_property_descriptor(*descriptor, worklist);
    }
}

fn trace_property_descriptor(descriptor: PropertyDescriptor, worklist: &mut Vec<TraceNode>) {
    match descriptor.kind {
        PropertyKind::Data { value, .. } => worklist.push(TraceNode::Value(value)),
        PropertyKind::Accessor { getter, setter } => {
            worklist.extend(getter.into_iter().chain(setter).map(TraceNode::Value));
        }
    }
}

fn usize_to_number(value: usize) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(f64::INFINITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SourceLocation, SourceSpan};
    use std::collections::{HashSet, VecDeque};

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum ModelNode {
        String(usize),
        Object(usize),
        Function(usize),
        Environment(usize),
    }

    fn reference_reachable(
        roots: &[ModelNode],
        edges: &HashMap<ModelNode, Vec<ModelNode>>,
    ) -> HashSet<ModelNode> {
        let mut reachable = HashSet::new();
        let mut worklist = VecDeque::from(roots.to_vec());
        while let Some(node) = worklist.pop_front() {
            if !reachable.insert(node) {
                continue;
            }
            worklist.extend(edges.get(&node).into_iter().flatten().copied());
        }
        reachable
    }

    fn test_span() -> SourceSpan {
        SourceSpan::new(SourceLocation::start(), SourceLocation::start())
    }

    fn js(value: &str) -> JsString {
        JsString::from_utf8(value).unwrap()
    }

    fn script_function(closure: EnvironmentId, name: &str) -> Callable {
        Callable::Script(ScriptFunction {
            function: Function {
                name: Some(name.to_owned()),
                parameters: Vec::new(),
                body: Vec::new(),
                span: test_span(),
            },
            closure,
            source_name: Arc::from("gc-test.js"),
        })
    }

    fn allocate_test_function(heap: &mut Heap, closure: EnvironmentId, name: &str) -> RawValue {
        heap.allocate_function(script_function(closure, name), js(name), None, true)
    }

    fn initialized(value: RawValue) -> Binding {
        Binding {
            mutable: true,
            state: BindingState::Initialized(value),
        }
    }

    fn assert_slot_accounting(statistics: ArenaStatistics) {
        assert_eq!(
            statistics.capacity,
            statistics.live + statistics.free + statistics.retired
        );
    }

    struct MixedGraph {
        global: EnvironmentId,
        live_string: StringId,
        live_object: ObjectId,
        live_function: FunctionId,
        live_environment: EnvironmentId,
        dead_string: StringId,
        dead_object: ObjectId,
        dead_function: FunctionId,
        dead_environment: EnvironmentId,
    }

    fn build_mixed_graph(heap: &mut Heap) -> MixedGraph {
        let global = heap.allocate_environment(None);
        let live_string_value = heap.allocate_string(js("live"));
        let live_object_value = heap.allocate_object(
            None,
            HashMap::from([(
                js("text"),
                PropertyDescriptor::default_data(live_string_value),
            )]),
        );
        let live_environment = heap.allocate_environment(Some(global));
        let live_function_value = allocate_test_function(heap, live_environment, "live");
        let RawValue::Function(live_function) = live_function_value else {
            unreachable!();
        };
        heap.function_mut(live_function)
            .unwrap()
            .data
            .properties
            .insert(
                js("object"),
                PropertyDescriptor::default_data(live_object_value),
            );
        heap.environment_mut(live_environment)
            .unwrap()
            .bindings
            .insert("function".to_owned(), initialized(live_function_value));
        heap.environment_mut(global)
            .unwrap()
            .bindings
            .insert("entry".to_owned(), initialized(live_function_value));

        let dead_string_value = heap.allocate_string(js("dead"));
        let dead_object_value = heap.allocate_object(None, HashMap::new());
        let RawValue::Object(dead_object) = dead_object_value else {
            unreachable!();
        };
        heap.object_mut(dead_object)
            .unwrap()
            .data
            .properties
            .insert(
                js("self"),
                PropertyDescriptor::default_data(dead_object_value),
            );
        let dead_environment = heap.allocate_environment(Some(global));
        let dead_function_value = allocate_test_function(heap, dead_environment, "dead");
        let RawValue::Function(dead_function) = dead_function_value else {
            unreachable!();
        };
        heap.function_mut(dead_function)
            .unwrap()
            .data
            .properties
            .extend([
                (
                    js("object"),
                    PropertyDescriptor::default_data(dead_object_value),
                ),
                (
                    js("text"),
                    PropertyDescriptor::default_data(dead_string_value),
                ),
            ]);
        heap.object_mut(dead_object)
            .unwrap()
            .data
            .properties
            .insert(
                js("function"),
                PropertyDescriptor::default_data(dead_function_value),
            );
        heap.environment_mut(dead_environment)
            .unwrap()
            .bindings
            .insert("function".to_owned(), initialized(dead_function_value));

        let (RawValue::String(live_string), RawValue::Object(live_object)) =
            (live_string_value, live_object_value)
        else {
            unreachable!();
        };
        let RawValue::String(dead_string) = dead_string_value else {
            unreachable!();
        };
        MixedGraph {
            global,
            live_string,
            live_object,
            live_function,
            live_environment,
            dead_string,
            dead_object,
            dead_function,
            dead_environment,
        }
    }

    fn mixed_graph_model_reachable() -> HashSet<ModelNode> {
        let edges = HashMap::from([
            (ModelNode::Environment(0), vec![ModelNode::Function(0)]),
            (
                ModelNode::Function(0),
                vec![ModelNode::Environment(1), ModelNode::Object(0)],
            ),
            (
                ModelNode::Environment(1),
                vec![ModelNode::Environment(0), ModelNode::Function(0)],
            ),
            (ModelNode::Object(0), vec![ModelNode::String(0)]),
            (
                ModelNode::Function(1),
                vec![
                    ModelNode::Environment(2),
                    ModelNode::Object(1),
                    ModelNode::String(1),
                ],
            ),
            (
                ModelNode::Environment(2),
                vec![ModelNode::Environment(0), ModelNode::Function(1)],
            ),
            (
                ModelNode::Object(1),
                vec![ModelNode::Object(1), ModelNode::Function(1)],
            ),
        ]);
        reference_reachable(&[ModelNode::Environment(0)], &edges)
    }

    #[test]
    fn arena_reuses_tombstones_with_a_new_generation() {
        let mut arena = Arena::default();
        let stale = arena.insert("first");

        assert_eq!(arena.sweep(), 1);
        assert!(arena.get(stale).is_none());

        let current = arena.insert("second");
        assert_eq!(current.index, stale.index);
        assert_ne!(current.generation, stale.generation);
        assert_eq!(arena.get(current), Some(&"second"));
        assert!(arena.get(stale).is_none());
        assert_eq!(
            arena.statistics(),
            ArenaStatistics {
                capacity: 1,
                live: 1,
                free: 0,
                retired: 0,
            }
        );
        assert_slot_accounting(arena.statistics());
    }

    #[test]
    fn generation_exhaustion_permanently_retires_a_slot() {
        let mut arena = Arena::default();
        let _initial = arena.insert("last generation");
        arena.slots[0].generation = u32::MAX;
        let exhausted = Handle::new(0, u32::MAX);

        assert_eq!(arena.sweep(), 1);
        assert!(arena.get(exhausted).is_none());
        assert_eq!(arena.statistics().retired, 1);
        assert_slot_accounting(arena.statistics());

        let replacement = arena.insert("fresh slot");
        assert_eq!(replacement.index, 1);
        assert_eq!(replacement.generation, 0);
        assert!(arena.get(exhausted).is_none());
        assert_slot_accounting(arena.statistics());
    }

    #[test]
    fn mixed_graph_collection_matches_independent_reachability_expectation() {
        let mut heap = Heap::default();
        let graph = build_mixed_graph(&mut heap);
        let expected = mixed_graph_model_reachable();

        // Function `name` metadata is represented by traced string-valued
        // descriptors, so each function contributes one additional string.
        assert_eq!(heap.counts(), (4, 2, 2, 3));
        let first = heap.collect(&[], &[graph.global]).unwrap();
        assert_eq!(
            first.reclaimed,
            ReclaimedCounts {
                strings: 2,
                objects: 1,
                functions: 1,
                environments: 1,
            }
        );
        assert_eq!(heap.counts(), (2, 1, 1, 2));

        let actual_presence = [
            (
                ModelNode::String(0),
                heap.string(graph.live_string).is_some(),
            ),
            (
                ModelNode::String(1),
                heap.string(graph.dead_string).is_some(),
            ),
            (
                ModelNode::Object(0),
                heap.object(graph.live_object).is_some(),
            ),
            (
                ModelNode::Object(1),
                heap.object(graph.dead_object).is_some(),
            ),
            (
                ModelNode::Function(0),
                heap.function(graph.live_function).is_some(),
            ),
            (
                ModelNode::Function(1),
                heap.function(graph.dead_function).is_some(),
            ),
            (
                ModelNode::Environment(0),
                heap.environment(graph.global).is_some(),
            ),
            (
                ModelNode::Environment(1),
                heap.environment(graph.live_environment).is_some(),
            ),
            (
                ModelNode::Environment(2),
                heap.environment(graph.dead_environment).is_some(),
            ),
        ];
        for (node, is_live) in actual_presence {
            assert_eq!(
                is_live,
                expected.contains(&node),
                "collector/model disagreement for {node:?}"
            );
        }

        heap.environment_mut(graph.global)
            .unwrap()
            .bindings
            .remove("entry");
        let second = heap.collect(&[], &[graph.global]).unwrap();
        assert_eq!(second.reclaimed, first.reclaimed);
        assert_eq!(heap.counts(), (0, 0, 0, 1));

        let third = heap.collect(&[], &[graph.global]).unwrap();
        assert_eq!(third.reclaimed, ReclaimedCounts::default());
        assert_eq!(heap.counts(), (0, 0, 0, 1));
        assert_eq!(
            heap.total_reclaimed(),
            ReclaimedCounts {
                strings: 4,
                objects: 2,
                functions: 2,
                environments: 2,
            }
        );
        let statistics = heap.arena_statistics();
        assert_slot_accounting(statistics.strings);
        assert_slot_accounting(statistics.objects);
        assert_slot_accounting(statistics.functions);
        assert_slot_accounting(statistics.environments);
    }

    #[test]
    fn stale_live_edges_fail_without_aliasing_or_leaving_marks() {
        let mut heap = Heap::default();
        let stale = heap.allocate_string(js("stale"));
        assert_eq!(heap.collect(&[], &[]).unwrap().reclaimed.strings, 1);

        let current = heap.allocate_string(js("current"));
        let (RawValue::String(stale_id), RawValue::String(current_id)) = (stale, current) else {
            unreachable!();
        };
        assert_eq!(stale_id.index, current_id.index);
        assert_ne!(stale_id.generation, current_id.generation);
        assert!(heap.string(stale_id).is_none());
        assert!(
            heap.string(current_id)
                .is_some_and(|value| value.eq_utf8("current"))
        );

        let error = heap.collect(&[stale, current], &[]).unwrap_err();
        assert_eq!(error.kind, AllocationKind::String);
        assert_eq!(heap.collection_count(), 1);
        assert!(
            heap.string(current_id)
                .is_some_and(|value| value.eq_utf8("current"))
        );

        let recovered = heap.collect(&[], &[]).unwrap();
        assert_eq!(recovered.reclaimed.strings, 1);
        assert!(heap.string(current_id).is_none());
        assert_eq!(heap.collection_count(), 2);
    }
}

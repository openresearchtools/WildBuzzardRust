pub mod cache;
pub mod constant_table;
mod constant_table_builder;
pub mod exception_handlers;
pub mod function;
pub mod generator;
pub mod graphviz;
pub mod instruction;
mod instruction_traits;
pub(crate) mod metadata;
mod operand;
mod register_allocator;
pub mod source_map;
pub mod stack_frame;
pub(crate) mod verifier;
pub mod vm;
mod width;

#[cfg(feature = "baseline_jit")]
pub(crate) use width::WidthEnum;
mod writer;

pub use operand::Register;
pub use width::ExtraWide;

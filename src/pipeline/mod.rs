//! Pipeline — orchestrates source -> encode -> cache.

pub mod cpu;
pub mod error;
pub mod read_budget;
pub mod render;
pub mod wms_assemble;

pub use self::cpu::CpuPool;
pub use self::render::Renderer;

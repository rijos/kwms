//! WMS 1.1.1 / 1.3.0: `request` (stage 1, syntax) → `validate` (stage 2,
//! rules against the catalog) → [`validate::WmsGetMap`].

pub mod axis_order;
pub mod capabilities;
pub mod exception;
pub mod request;
pub mod validate;

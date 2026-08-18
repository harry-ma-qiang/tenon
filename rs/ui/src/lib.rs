mod approvals;
mod boxes;
mod events;
mod html;
mod model;
mod render;
pub mod terminal;
mod transcript;
mod tree;
mod wrap;

pub mod keys;

pub use html::html;
pub use model::{Approval, EventLine, NodeInfo, Role, StatusLine, TranscriptItem, UiModel};
pub use render::render;

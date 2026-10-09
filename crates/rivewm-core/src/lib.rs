//! Platform-independent core of rivewm.
//!
//! Nothing in this crate talks to Win32. The window tree, layout maths and
//! command handling live here so they can be unit tested on any platform.

pub mod command;
pub mod event;
pub mod geometry;
pub mod layout;
pub mod tree;

pub use command::{Command, ParseCommandError};
pub use event::WindowEvent;
pub use geometry::Rect;
pub use layout::Layout;
pub use tree::{
    Axis, Direction, Gaps, MonitorSpec, NodeId, SavedFloat, SavedNode, SavedWeights,
    SavedWorkspace, Tree,
};

/// Opaque identifier for a native window.
///
/// On Windows this wraps an `HWND` value. The core never dereferences it; it
/// only uses it to key windows in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub isize);

/// Opaque identifier for a physical monitor (an `HMONITOR` on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(pub isize);

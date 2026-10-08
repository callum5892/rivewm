use crate::WindowId;

/// A window lifecycle or state change reported by the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    Shown(WindowId),
    Hidden(WindowId),
    Destroyed(WindowId),
    /// Became the foreground window.
    Focused(WindowId),
    Minimized(WindowId),
    Restored(WindowId),
    /// The user started dragging or resizing the window.
    MoveSizeStarted(WindowId),
    MoveSizeEnded(WindowId),
    /// Position or size changed, for any reason (including our own moves).
    LocationChanged(WindowId),
    TitleChanged(WindowId),
    Cloaked(WindowId),
    Uncloaked(WindowId),
}

impl WindowEvent {
    pub fn window(&self) -> WindowId {
        match *self {
            WindowEvent::Shown(id)
            | WindowEvent::Hidden(id)
            | WindowEvent::Destroyed(id)
            | WindowEvent::Focused(id)
            | WindowEvent::Minimized(id)
            | WindowEvent::Restored(id)
            | WindowEvent::MoveSizeStarted(id)
            | WindowEvent::MoveSizeEnded(id)
            | WindowEvent::LocationChanged(id)
            | WindowEvent::TitleChanged(id)
            | WindowEvent::Cloaked(id)
            | WindowEvent::Uncloaked(id) => id,
        }
    }
}

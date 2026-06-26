//! App-owned bundle of the three `SessionUpdate` consumers.

use crate::gui_projector::GuiProjector;
use crate::render_projector::RenderProjector;
use crate::runner_projector::RunnerProjector;

/// The three projector classes that consume Assembly updates: the runner-side
/// consumer, the render consumer, and the GUI consumer. Each is borrowed
/// independently at the tick drain (alongside `&mut store`), so the sub-fields
/// stay public to the `app` module rather than hiding behind a combined method.
pub(in crate::app) struct Projectors {
    pub(in crate::app) runner: RunnerProjector,
    pub(in crate::app) render: RenderProjector,
    pub(in crate::app) gui: GuiProjector,
}

impl Projectors {
    pub(in crate::app) const fn new() -> Self {
        Self {
            runner: RunnerProjector::new(),
            render: RenderProjector::new(),
            gui: GuiProjector::new(),
        }
    }
}

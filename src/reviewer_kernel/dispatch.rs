use std::sync::Arc;

use crate::reviewer_kernel::kernel_types::{RuntimeEvent, RuntimeEventContext, RuntimeEventSink};
use crate::reviewer_kernel::policy::PlannedRuntimeEvent;

#[derive(Clone)]
pub(crate) struct RuntimeEventDispatcher {
    runtime_sink: Option<Arc<dyn RuntimeEventSink>>,
}

impl RuntimeEventDispatcher {
    pub(crate) fn new(runtime_sink: Option<Arc<dyn RuntimeEventSink>>) -> Self {
        Self { runtime_sink }
    }

    pub(crate) fn emit_planned_runtime(&self, planned: PlannedRuntimeEvent) {
        self.emit_runtime_with_context(planned.context, planned.event);
    }

    pub(crate) fn emit_runtime_with_context(
        &self,
        context: RuntimeEventContext,
        event: RuntimeEvent,
    ) {
        if let Some(runtime_sink) = &self.runtime_sink {
            runtime_sink.emit_with_context(context, event);
        }
    }
}

use std::sync::Arc;

use crate::events::{EventEmitter, EventRecord};
use crate::runtime::contracts::{RuntimeEvent, RuntimeEventContext, RuntimeEventSink};
use crate::runtime::policy::PlannedRuntimeEvent;

#[derive(Clone)]
pub(crate) struct RuntimeEventDispatcher {
    runtime_sink: Option<Arc<dyn RuntimeEventSink>>,
    legacy_emitter: Option<Arc<EventEmitter>>,
}

impl RuntimeEventDispatcher {
    pub(crate) fn new(
        runtime_sink: Option<Arc<dyn RuntimeEventSink>>,
        legacy_emitter: Option<Arc<EventEmitter>>,
    ) -> Self {
        Self {
            runtime_sink,
            legacy_emitter,
        }
    }

    pub(crate) fn none() -> Self {
        Self::new(None, None)
    }

    pub(crate) fn emit_legacy(&self, event: EventRecord) {
        if let Some(emitter) = &self.legacy_emitter {
            emitter.emit(event);
        }
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

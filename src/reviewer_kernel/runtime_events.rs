use std::sync::Mutex;

pub use crate::reviewer_kernel::kernel_types::{
    RuntimeEvent, RuntimeEventContext, RuntimeEventRecord, RuntimeEventSink as EventSink,
};
use crate::reviewer_kernel::system::timestamp_utc;

#[derive(Debug, Default)]
pub struct InMemoryEventSink {
    records: Mutex<Vec<RuntimeEventRecord>>,
}

impl InMemoryEventSink {
    pub fn events(&self) -> Vec<RuntimeEvent> {
        self.records()
            .into_iter()
            .map(|record| record.event)
            .collect()
    }

    pub fn records(&self) -> Vec<RuntimeEventRecord> {
        self.records
            .lock()
            .expect("in-memory event sink poisoned")
            .clone()
    }

    fn record(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let mut records = self.records.lock().expect("in-memory event sink poisoned");
        let seq = records.len() as u64 + 1;
        records.push(RuntimeEventRecord {
            seq,
            timestamp_utc: timestamp_utc(),
            context,
            event,
        });
    }
}

impl EventSink for InMemoryEventSink {
    fn emit(&self, event: RuntimeEvent) {
        let context = RuntimeEventContext::from_event(&event);
        self.record(context, event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        self.record(context, event);
    }
}

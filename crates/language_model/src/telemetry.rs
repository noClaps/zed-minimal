use client::telemetry::Telemetry;
use std::sync::Arc;
use telemetry_events::AssistantEventData;

pub fn report_assistant_event(event: AssistantEventData, telemetry: Option<Arc<Telemetry>>) {
    if let Some(telemetry) = telemetry.as_ref() {
        telemetry.report_assistant_event(event.clone());
    }
}

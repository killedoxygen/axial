use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SseEnvelope<T> {
    pub event: &'static str,
    pub data: T,
}

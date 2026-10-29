use std::sync::Arc;

use tokio::sync::Mutex;
use uzu::session::chat::ChatSession;

use crate::common::thinking::ThinkingSupport;

pub struct ServerState {
    pub model_name: String,
    pub instance: nagare::chat::ChatInstance,
    pub inference_gate: Arc<tokio::sync::Semaphore>,
    pub session: Arc<Mutex<ChatSession>>,
    pub thinking_support: ThinkingSupport,
    pub prefix_cache: bool,
}

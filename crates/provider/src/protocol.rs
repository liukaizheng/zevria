use rig_core::completion::CompletionError;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;

pub(crate) fn websocket_message_to_text(
    message: WebSocketMessage,
) -> Result<Option<String>, CompletionError> {
    match message {
        WebSocketMessage::Text(text) => Ok(Some(text.to_string())),
        WebSocketMessage::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|error| CompletionError::ResponseError(error.to_string())),
        WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) | WebSocketMessage::Frame(_) => {
            Ok(None)
        }
        WebSocketMessage::Close(frame) => {
            let reason = frame
                .map(|frame| frame.reason.to_string())
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "without a close reason".to_string());
            Err(CompletionError::ProviderError(format!(
                "The OpenAI websocket connection closed {reason}"
            )))
        }
    }
}

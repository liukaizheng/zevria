use rig_core::error::ProviderError as CompletionError;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;

pub(crate) fn websocket_message_to_text(
    message: &WebSocketMessage,
) -> anyhow::Result<Option<&str>> {
    match message {
        WebSocketMessage::Text(text) => Ok(Some(text.as_str())),
        WebSocketMessage::Binary(bytes) => std::str::from_utf8(bytes)
            .map(Some)
            .map_err(|error| CompletionError::Response(error.to_string()).into()),
        WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) | WebSocketMessage::Frame(_) => {
            Ok(None)
        }
        WebSocketMessage::Close(frame) => {
            let reason = frame
                .as_ref()
                .map(|frame| frame.reason.as_str())
                .filter(|reason| !reason.is_empty())
                .unwrap_or("without a close reason");
            Err(CompletionError::Provider(format!(
                "The OpenAI websocket connection closed {reason}"
            ))
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_errors_retain_provider_error_categories() {
        let error =
            websocket_message_to_text(&WebSocketMessage::Binary(vec![0xff].into())).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CompletionError>(),
            Some(CompletionError::Response(_))
        ));

        let error = websocket_message_to_text(&WebSocketMessage::Close(None)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CompletionError>(),
            Some(CompletionError::Provider(message))
                if message == "The OpenAI websocket connection closed without a close reason"
        ));
    }

    #[test]
    fn websocket_payload_and_control_frames_keep_their_existing_meaning() {
        for message in [
            WebSocketMessage::Text("response payload".into()),
            WebSocketMessage::Binary(b"response payload".to_vec().into()),
        ] {
            assert_eq!(
                websocket_message_to_text(&message).unwrap(),
                Some("response payload")
            );
        }
        for message in [
            WebSocketMessage::Ping(vec![1].into()),
            WebSocketMessage::Pong(vec![1].into()),
        ] {
            assert!(websocket_message_to_text(&message).unwrap().is_none());
        }
    }
}

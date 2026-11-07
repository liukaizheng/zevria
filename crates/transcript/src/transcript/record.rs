#[cfg(test)]
mod tests {
    use crate::{
        ModelProfileRef, ModelResponse, ProviderReplay,
        provider_replay::count_canonical_derivations, transcript::TranscriptItem,
    };
    use rig_core::message::{AssistantContent, Message};
    use serde_json::json;
    use zevria_model::MessageRecord;

    fn replay() -> ProviderReplay {
        ProviderReplay::openai_responses(
            ModelProfileRef::new("fixed-provider", "fixed-model"),
            vec![
                json!({"type":"reasoning", "id":"reason-1", "summary":[{"type":"summary_text", "text":"reasoning"}], "encrypted_content":"opaque", "unknown":{"record":1}}),
                json!({"type":"message", "id":"message-1", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"answer", "annotations":[]}], "future":[null,true,17]}),
            ],
        )
    }

    #[test]
    fn completion_snapshot_and_admission_share_one_canonical_derivation() {
        let native = replay();
        let ((completed, snapshot), derivations) = count_canonical_derivations(|| {
            let response = ModelResponse::from_replay(native.clone())
                .unwrap()
                .with_attempt_id(Some("attempt-1".into()))
                .unwrap();
            let snapshot = response
                .record()
                .model_request_item()
                .to_owned_item()
                .unwrap();
            let completed = TranscriptItem::from(response.into_record());
            (completed, snapshot)
        });
        assert_eq!(derivations, 1);
        assert_eq!(snapshot.replay_ref(), Some(&native));
        assert_eq!(completed.provider_replay(), Some(&native));
        assert_eq!(snapshot.message_ref(), completed.message());
        assert!(
            !serde_json::to_string(&completed)
                .unwrap()
                .contains("attempt-1")
        );

        let bytes = serde_json::to_vec(&completed).unwrap();
        let (loaded, derivations) = count_canonical_derivations(|| {
            serde_json::from_slice::<TranscriptItem>(&bytes).unwrap()
        });
        assert_eq!(derivations, 1);
        assert_eq!(loaded, completed);
        let (_, derivations) = count_canonical_derivations(|| {
            let cloned = snapshot.clone();
            let _ = cloned.as_borrowed().to_owned_item().unwrap();
            let _ = serde_json::to_vec(&cloned).unwrap();
        });
        assert_eq!(
            derivations, 0,
            "trusted copies/serialization must not re-decode native JSON"
        );
    }

    fn text_ptr(message: &Message) -> *const u8 {
        let Message::Assistant(rig_core::message::AssistantMessage { content, .. }) = message
        else {
            panic!("assistant")
        };
        let AssistantContent::Text(text) = &content[0] else {
            panic!("text")
        };
        text.text.as_ptr()
    }

    #[test]
    fn owned_restoration_moves_backing_allocations() {
        let record = MessageRecord::plain(Message::assistant("plain assistant text")).unwrap();
        let pointer = text_ptr(record.message());
        assert_eq!(text_ptr(record.message()), pointer);
        let (message, replay_backed) = record.into_display();
        assert_eq!(text_ptr(&message), pointer);
        assert!(!replay_backed);

        let record = MessageRecord::from_replay(replay()).unwrap();
        let pointer = record.provider_replay().unwrap().items.as_ptr();
        assert_eq!(record.provider_replay().unwrap().items.as_ptr(), pointer);
        let completed = TranscriptItem::from(record);
        assert_eq!(completed.provider_replay().unwrap().items.as_ptr(), pointer);
    }

    #[test]
    fn invalid_content_cannot_become_a_completed_response() {
        assert!(
            MessageRecord::plain(Message::System {
                content: "untrusted".into()
            })
            .is_err()
        );
        assert!(
            ModelResponse::plain(Message::assistant("answer"))
                .unwrap()
                .with_attempt_id(Some("bad\nidentity".into()))
                .is_err()
        );
        let mut invalid = replay();
        invalid.version = 999;
        assert!(ModelResponse::from_replay(invalid).is_err());
        let opaque = ProviderReplay::openai_responses(
            ModelProfileRef::new("fixed", "model"),
            vec![json!({"type":"future_item", "opaque":true})],
        );
        assert!(ModelResponse::from_replay(opaque.clone()).is_err());
        assert!(crate::OwnedModelRequestItem::replay_only(opaque).is_ok());
    }
}

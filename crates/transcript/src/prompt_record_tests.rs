use crate::prompt::*;
fn image() -> PromptImage {
    PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap()
}
fn padded_png(size: usize) -> PromptImage {
    let mut bytes = image().encoded_bytes().to_vec();
    let end = bytes.split_off(bytes.len() - 12);
    let length = size - bytes.len() - end.len() - 12;
    let mut chunk = Vec::with_capacity(length + 4);
    chunk.extend_from_slice(b"tEXtpadding\0");
    chunk.resize(length + 4, b'x');
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
        }
        *entry = crc;
    }
    let mut crc = !0u32;
    for byte in &chunk {
        crc = (crc >> 8) ^ table[((crc ^ u32::from(*byte)) & 255) as usize];
    }
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.extend_from_slice(&chunk);
    bytes.extend_from_slice(&(!crc).to_be_bytes());
    bytes.extend_from_slice(&end);
    assert_eq!(bytes.len(), size);
    PromptImage::from_encoded("image/png", bytes).unwrap()
}

#[test]
fn maximum_encoded_prompt_and_mirrored_root_record_fit_the_bounded_budget() {
    let image = padded_png(MAX_IMAGE_BYTES);
    let prompt = UserPrompt::new(vec![PromptBlock::Image(image); 4]).unwrap();
    assert_eq!(
        prompt.images().map(PromptImage::encoded_len).sum::<usize>(),
        MAX_PROMPT_IMAGE_BYTES
    );
    let mut overflow = prompt.clone().into_blocks();
    overflow.push(PromptBlock::Image(self::image()));
    assert!(UserPrompt::new(overflow).is_err());
    let id = crate::WorkerControlId::new();
    let run = crate::EnsembleRunId::new();
    let worker = crate::AgentRunId::new();
    let record = crate::EnsembleRecord::WorkerReview {
        run_id: run.clone(),
        worker_id: worker.clone(),
        event: Box::new(crate::WorkerReviewEvent::InputAccepted {
            input: crate::WorkerInput {
                generation: 2,
                request_id: id.clone(),
                kind: crate::WorkerPromptKind::UserFeedback,
                text: prompt.clone(),
            },
        }),
        result: Some(crate::WorkerControlResult {
            control: crate::WorkerControl {
                request_id: id,
                target: crate::WorkerControlTarget {
                    run_id: run,
                    worker_id: worker,
                    turn_id: crate::TurnId::new(1),
                },
                action: crate::WorkerControlAction::SendFeedback { text: prompt },
            },
            accepted: true,
            detail: "accepted".into(),
        }),
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert!(bytes.len() > 50 * 1024 * 1024);
    assert!(bytes.len() < crate::ensemble::MAX_AGENT_RUN_RECORD_BYTES);
    assert!(bytes.len() < crate::transcript::MAX_ROOT_RECORD_BYTES);
    assert_eq!(
        serde_json::from_slice::<crate::EnsembleRecord>(&bytes).unwrap(),
        record
    );
}

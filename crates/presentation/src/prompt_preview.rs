//! The display-only projection of one locally admitted, uncommitted prompt.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use zevria_content::{PromptBlock, UserPrompt, readable::sanitize_readable};
use zevria_foundation::{SideChatReferenceAttachment, TurnId};

use crate::blocks::{
    BlockVisibility, PresentationBlock, PresentationBlockId, PresentationBlockKind,
    PresentationRole, TextFlavor,
};

pub const PREPARING_PROMPT: &str = "Preparing…";

#[derive(Debug)]
pub struct PromptPreview {
    id: u64,
    blocks: Vec<PresentationBlock>,
    pub(crate) references: Vec<SideChatReferenceAttachment>,
    paint: Option<PromptPaint>,
}

impl PromptPreview {
    pub(crate) fn new(prompt: &UserPrompt) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let paint = tracing::enabled!(target: "zevria::prompt_latency", tracing::Level::DEBUG)
            .then(|| {
                PromptPaint(Arc::new(PaintProbe {
                    id,
                    started: Instant::now(),
                    painted: AtomicBool::new(false),
                }))
            });
        let mut ordinal = 0;
        let blocks = prompt
            .blocks()
            .iter()
            .enumerate()
            .map(|(index, part)| PresentationBlock {
                // These IDs are local to this projection, not transcript block IDs.
                id: PresentationBlockId(index as u64),
                revision: 0,
                role: Some(PresentationRole::User),
                prompt_group: None,
                prompt: None,
                visibility: BlockVisibility::Always,
                kind: match part {
                    PromptBlock::Text(text) => PresentationBlockKind::Text {
                        text: sanitize_readable(text),
                        flavor: TextFlavor::Plain,
                        editable: false,
                        sources: Vec::new(),
                    },
                    PromptBlock::Image(image) => {
                        ordinal += 1;
                        PresentationBlockKind::Placeholder(image.label(ordinal))
                    }
                },
            })
            .collect();
        Self {
            id,
            blocks,
            references: Vec::new(),
            paint,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn blocks(&self) -> &[PresentationBlock] {
        &self.blocks
    }

    pub fn references(&self) -> &[SideChatReferenceAttachment] {
        &self.references
    }

    pub fn paint_probe(&self) -> Option<&PromptPaint> {
        self.paint.as_ref().filter(|paint| !paint.is_painted())
    }

    pub(crate) fn accept(self, turn: TurnId) -> Option<PromptPaint> {
        let paint = self.paint?;
        tracing::debug!(
            target: "zevria::prompt_latency",
            submit_id = self.id,
            turn_id = turn.get(),
            elapsed_us = paint.elapsed_us(),
            "submit to acceptance"
        );
        (!paint.is_painted()).then_some(paint)
    }
}

/// One-shot paint observation shared by the pending and canonical renderers.
/// Its contents are local diagnostic IDs and time only.
#[derive(Clone, Debug)]
pub struct PromptPaint(Arc<PaintProbe>);

#[derive(Debug)]
struct PaintProbe {
    id: u64,
    started: Instant,
    painted: AtomicBool,
}

impl PromptPaint {
    fn elapsed_us(&self) -> u64 {
        self.0.started.elapsed().as_micros() as u64
    }

    pub(crate) fn is_painted(&self) -> bool {
        self.0.painted.load(Ordering::Relaxed)
    }

    pub fn mark_painted(&self) {
        if !self.0.painted.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                target: "zevria::prompt_latency",
                submit_id = self.0.id,
                elapsed_us = self.elapsed_us(),
                "submit to first paint"
            );
        }
    }
}

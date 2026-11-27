//! A staged draft belongs to one unaccepted submission and one replacement generation.

pub struct StagedDraft<D> {
    pub draft: D,
    pub generation: u64,
}

pub struct DraftSlot<D> {
    pending: Option<StagedDraft<D>>,
    rejected: Vec<D>,
}

impl<D> Default for DraftSlot<D> {
    fn default() -> Self {
        Self {
            pending: None,
            rejected: Vec::new(),
        }
    }
}

impl<D> DraftSlot<D> {
    pub fn stage(&mut self, draft: D, generation: u64) {
        assert!(
            self.pending.is_none(),
            "work admission must precede draft staging"
        );
        self.pending = Some(StagedDraft { draft, generation });
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn take(&mut self) -> Option<StagedDraft<D>> {
        self.pending.take()
    }

    pub fn accept(&mut self) {
        self.pending = None;
    }

    /// Never overwrite work written in a newer replacement generation.
    pub fn restore_or_retain(
        &mut self,
        draft: D,
        expected: u64,
        current: u64,
        replacement_empty: bool,
    ) -> Option<D> {
        if expected == current && replacement_empty {
            Some(draft)
        } else {
            self.retain(draft);
            None
        }
    }

    pub fn retain(&mut self, draft: D) {
        self.rejected.push(draft);
    }

    pub fn recovery(&self) -> &[D] {
        &self.rejected
    }

    pub fn recover(&mut self) -> Option<D> {
        self.rejected.pop()
    }
}

//! Native request numbering, timing, and represented header indexes.

use super::{
    ConversationState, DisplayTurn, Duration, HeaderObservation, HeaderTiming, HistoryEntry,
    Instant, NativeHeader, PresentationRole,
};

impl ConversationState {
    pub fn allocate_turn(&mut self) -> DisplayTurn {
        let turn = DisplayTurn(self.prompt_count + 1);
        self.observe_header(NativeHeader::Prompt(turn));
        turn
    }

    pub fn allocate_call(&mut self, turn: DisplayTurn) -> NativeHeader {
        let call = self.call_counts.get(&turn).copied().unwrap_or_default() + 1;
        let header = NativeHeader::Assistant { turn, call };
        self.observe_header(header);
        header
    }

    fn observe_header(&mut self, header: NativeHeader) {
        self.header_indices.insert(header, self.headers.len());
        match header {
            NativeHeader::Prompt(_) => self.prompt_count += 1,
            NativeHeader::Assistant { turn, .. } => *self.call_counts.entry(turn).or_default() += 1,
        }
        self.headers.push(HeaderObservation {
            history_index: self.history.len(),
            header,
            timing: None,
        });
    }

    pub fn start_header(&mut self, header: NativeHeader, now: Instant) {
        if matches!(header, NativeHeader::Assistant { .. })
            && let Some(observation) = self
                .header_indices
                .get(&header)
                .and_then(|&index| self.headers.get_mut(index))
        {
            // A duplicate observation must never reset a running or finished call.
            if observation.timing.is_none() {
                observation.timing = Some(HeaderTiming::Running(now));
                self.running_headers.insert(header, now);
                self.timing_revision += 1;
            }
        }
    }

    pub fn finish_header(&mut self, header: Option<NativeHeader>, now: Instant) {
        if let Some(header) = header
            && let Some(observation) = self
                .header_indices
                .get(&header)
                .and_then(|&index| self.headers.get_mut(index))
            && let Some(timing @ HeaderTiming::Running(_)) = &mut observation.timing
        {
            *timing = HeaderTiming::Finished(timing.elapsed(now));
            self.running_headers.remove(&header);
            self.timing_revision += 1;
        }
    }

    pub fn header_timing_revision(&self) -> u64 {
        self.timing_revision
    }

    pub fn running_header_seconds(
        &self,
        now: Instant,
    ) -> impl Iterator<Item = (NativeHeader, u64)> + '_ {
        self.running_headers.iter().map(move |(&header, &started)| {
            (header, now.saturating_duration_since(started).as_secs())
        })
    }

    /// Unknown starts (including restored headers) do not acquire a zero duration.
    pub fn header_timings(
        &self,
        now: Instant,
    ) -> impl Iterator<Item = (NativeHeader, Duration)> + '_ {
        self.headers.iter().filter_map(move |observation| {
            observation
                .timing
                .as_ref()
                .map(|timing| (observation.header, timing.elapsed(now)))
        })
    }

    pub fn represented_header_indices(&self, header: NativeHeader) -> &[usize] {
        self.represented_headers
            .get(&header)
            .map_or(&[], Vec::as_slice)
    }

    pub fn represented_header_index(&self, header: NativeHeader) -> Option<usize> {
        self.represented_header_indices(header).last().copied()
    }

    pub fn represents_header(&self, header: NativeHeader) -> bool {
        self.represented_header_indices(header).iter().rev().any(|&index| {
            matches!(&self.history[index], HistoryEntry::Conversation(entry)
                if entry.blocks.iter().any(|block| block.role == Some(PresentationRole::Assistant) && block.visible(false)))
        })
    }
}

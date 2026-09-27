//! Admission decisions are independent of focus and rendering. In particular,
//! work in flight never disables ordinary drafting.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisabledReason {
    ReadOnly,
    ClipboardPending,
    TranscriptEditPending,
    WorkPending,
    PlanDecisionRequired,
    RecallActive,
}

pub(crate) type Admission = Result<(), DisabledReason>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Capabilities {
    pub edit_draft: Admission,
    pub submit_work: Admission,
    pub edit_transcript: Admission,
    pub manage_session: Admission,
}

//! One immutable, unbound Messages projection per input/profile/compatibility.
//! Request binding is deliberately not cached here: actual system/tools decide
//! whether native signed thinking survives completion or counting.
use std::sync::Arc;
use zevria_anthropic::{Compatibility, project::ValidatedHistory};
use zevria_foundation::ModelProfileRef;
use zevria_model::{ModelInputIdentity, ModelRequestItem, models::ReplayPreflight};

#[derive(Clone)]
pub(crate) struct HistoryPreparation {
    pub(crate) assessment: ReplayPreflight,
    pub(crate) history: Option<Arc<ValidatedHistory>>,
}

struct Entry {
    target: ModelProfileRef,
    compatibility: Compatibility,
    input_count: usize,
    input_digest: [u8; 32],
    prepared: HistoryPreparation,
}

#[derive(Default)]
pub(crate) struct HistoryCache {
    entry: Option<Entry>,
}
impl HistoryCache {
    pub(crate) fn prepare(
        &mut self,
        input: &[ModelRequestItem<'_>],
        identity: Option<&ModelInputIdentity>,
        target: &ModelProfileRef,
        compatibility: &Compatibility,
    ) -> anyhow::Result<HistoryPreparation> {
        let fallback;
        let identity = match identity {
            Some(identity) if identity.len() == input.len() => identity,
            _ => {
                fallback = ModelInputIdentity::from_items(input)?;
                &fallback
            }
        };
        if let Some(entry) = &self.entry
            && entry.target == *target
            && entry.compatibility == *compatibility
            && entry.input_count == identity.len()
            && entry.input_digest == identity.digest()
        {
            return Ok(entry.prepared.clone());
        }
        let (assessment, history) = zevria_anthropic::project::prepare_history(
            input,
            target,
            compatibility,
            Some(identity),
        )?;
        let prepared = HistoryPreparation {
            assessment,
            history: history.map(Arc::new),
        };
        self.entry = Some(Entry {
            target: target.clone(),
            compatibility: compatibility.clone(),
            input_count: identity.len(),
            input_digest: identity.digest(),
            prepared: prepared.clone(),
        });
        Ok(prepared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::Message;

    #[test]
    fn admission_validation_and_binding_share_one_history_but_not_foreign_keys() {
        let target = ModelProfileRef::new("anthropic", "model");
        let compatibility = Compatibility::default();
        let user = Message::user("prompt");
        let input = [ModelRequestItem::message(&user)];
        let identity = ModelInputIdentity::from_items(&input).unwrap();
        let mut cache = HistoryCache::default();
        let first = cache
            .prepare(&input, Some(&identity), &target, &compatibility)
            .unwrap();
        let again = cache
            .prepare(&input, None, &target, &compatibility)
            .unwrap();
        assert_eq!(first.assessment, again.assessment);
        let first = first.history.unwrap();
        assert!(Arc::ptr_eq(&first, &again.history.unwrap()));
        let different = cache
            .prepare(
                &input,
                Some(&identity),
                &ModelProfileRef::new("other", "model"),
                &compatibility,
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &different.history.unwrap()));
        let different = cache
            .prepare(
                &input,
                Some(&identity),
                &target,
                &Compatibility {
                    inline_tools: false,
                    ..compatibility.clone()
                },
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &different.history.unwrap()));
        let edited = Message::user("edited");
        let different = cache
            .prepare(
                &[ModelRequestItem::message(&edited)],
                None,
                &target,
                &compatibility,
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &different.history.unwrap()));
    }
}

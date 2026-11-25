//! One immutable, unbound Messages projection per input/profile/compatibility.
//! Request binding is deliberately not cached here: actual system/tools decide
//! whether native signed thinking survives completion or counting.
use std::sync::Arc;
use zevria_anthropic::{
    Compatibility,
    project::{self, HistoryOptions, ValidatedHistory},
};
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
    policy: zevria_foundation::image_accounting::ImageAccountingPolicy,
    policy_version: u32,
    input_count: usize,
    input_digest: [u8; 32],
    prepared: HistoryPreparation,
}

#[derive(Default)]
pub(crate) struct HistoryCache {
    entry: Option<Entry>,
}
impl HistoryCache {
    pub(crate) fn assess(
        &mut self,
        input: &[ModelRequestItem<'_>],
        identity: Option<&ModelInputIdentity>,
        target: &ModelProfileRef,
        compatibility: &Compatibility,
        policy: &zevria_foundation::image_accounting::ImageAccountingPolicy,
    ) -> anyhow::Result<ReplayPreflight> {
        let Some(identity) = identity.filter(|identity| identity.len() == input.len()) else {
            return project::assess_history(
                input,
                HistoryOptions {
                    policy,
                    ..HistoryOptions::new(target, compatibility)
                },
            );
        };
        Ok(self
            .prepare(input, Some(identity), target, compatibility, policy)?
            .assessment)
    }

    pub(crate) fn prepare(
        &mut self,
        input: &[ModelRequestItem<'_>],
        identity: Option<&ModelInputIdentity>,
        target: &ModelProfileRef,
        compatibility: &Compatibility,
        policy: &zevria_foundation::image_accounting::ImageAccountingPolicy,
    ) -> anyhow::Result<HistoryPreparation> {
        let fallback;
        let identity = match identity {
            Some(identity) if identity.len() == input.len() => identity,
            _ => {
                fallback = ModelInputIdentity::from_items(input)?;
                &fallback
            }
        };
        let prior = self.entry.as_ref().filter(|entry| {
            entry.target == *target
                && entry.compatibility == *compatibility
                && entry.policy == *policy
                && entry.policy_version == policy.version()
                && identity.prefix(entry.input_count) == Some(entry.input_digest)
        });
        if let Some(prior) = prior
            && prior.input_count == identity.len()
        {
            return Ok(prior.prepared.clone());
        }
        let prefix_media = prior.and_then(|entry| {
            if let ReplayPreflight::Compatible(mut estimate) = entry.prepared.assessment {
                estimate.payload_tokens = 0;
                estimate.estimated_tokens = 0;
                estimate.unresolved_media.files = 0;
                estimate.unresolved_media.opaque_items = 0;
                Some((entry.input_count, estimate))
            } else {
                None
            }
        });
        let (assessment, history) = project::prepare_history(
            input,
            Some(identity),
            HistoryOptions {
                policy,
                measured_prefix: prefix_media,
                ..HistoryOptions::new(target, compatibility)
            },
        )?;
        let prepared = HistoryPreparation {
            assessment,
            history: history.map(Arc::new),
        };
        self.entry = Some(Entry {
            target: target.clone(),
            policy: policy.clone(),
            policy_version: policy.version(),
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
    fn local_assessment_extends_image_facts_and_invalidates_numeric_policy_only() {
        use zevria_foundation::image_accounting::ImageAccountingPolicy;
        let target = ModelProfileRef::new("gateway", "unmapped-claude");
        let compatibility = Compatibility::default();
        let image = zevria_content::PromptImage::from_rgba(
            3,
            1,
            &[7, 11, 32, 255, 29, 31, 37, 255, 39, 41, 43, 255],
        )
        .unwrap();
        let message = Message::from(vec![rig_core::message::UserContent::image_base64(
            image.base64(),
            Some(rig_core::message::ImageMediaType::PNG),
            None,
        )]);
        let text = Message::user("text-only append");
        let input = [
            ModelRequestItem::Message(&message),
            ModelRequestItem::Message(&text),
        ];
        let mut cache = HistoryCache::default();
        let before = zevria_content::image_facts::thread_fact_derivations();
        for policy in [
            ImageAccountingPolicy::Unresolved,
            ImageAccountingPolicy::preset("claude-sonnet-4-5-20250929").unwrap(),
            ImageAccountingPolicy::Unresolved,
        ] {
            for end in 1..=input.len() {
                let identity = ModelInputIdentity::from_items(&input[..end]).unwrap();
                let actual = cache
                    .assess(
                        &input[..end],
                        Some(&identity),
                        &target,
                        &compatibility,
                        &policy,
                    )
                    .unwrap();
                let full = project::assess_history(
                    &input[..end],
                    HistoryOptions {
                        policy: &policy,
                        ..HistoryOptions::new(&target, &compatibility)
                    },
                )
                .unwrap();
                assert_eq!(actual, full);
            }
        }
        assert_eq!(
            zevria_content::image_facts::thread_fact_derivations(),
            before + 1
        );
        let system = serde_json::json!([]);
        let tools = serde_json::Value::Null;
        let full =
            project::project(&input, &target, &compatibility, &system, &tools, false).unwrap();
        let cached = cache
            .entry
            .unwrap()
            .prepared
            .history
            .unwrap()
            .completion(&system, &tools, false, &input)
            .unwrap();
        assert_eq!(cached.messages, full.messages);
        assert_eq!(cached.prefix_binding, full.prefix_binding);
    }

    #[test]
    fn admission_validation_and_binding_share_one_history_but_not_foreign_keys() {
        let policy = &zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved;
        let target = ModelProfileRef::new("anthropic", "model");
        let compatibility = Compatibility::default();
        let user = Message::user("prompt");
        let input = [ModelRequestItem::message(&user)];
        let identity = ModelInputIdentity::from_items(&input).unwrap();
        let mut cache = HistoryCache::default();
        let assessment = cache
            .assess(&input, Some(&identity), &target, &compatibility, policy)
            .unwrap();
        let first = cache.entry.as_ref().unwrap().prepared.clone();
        assert_eq!(assessment, first.assessment);
        let again = cache
            .prepare(&input, None, &target, &compatibility, policy)
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
                policy,
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
                policy,
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
                policy,
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &different.history.unwrap()));
    }
}

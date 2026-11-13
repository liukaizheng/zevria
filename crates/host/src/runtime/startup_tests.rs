use super::startup_assessments;
use std::sync::Mutex;
use zevria_foundation::ModelProfileRef;
use zevria_model::{ContextTokenEstimate, ModelRequest, ModelRequestItem, models::ReplayPreflight};
use zevria_session_api::{ModelProvider, ProgressReporter, ProviderFuture};

struct CountingPreflight(Mutex<Vec<ModelProfileRef>>);
impl ModelProvider for CountingPreflight {
    fn application_prompt(&self) -> &str {
        ""
    }
    fn complete<'a>(&'a mut self, _: ModelRequest<'a>, _: ProgressReporter) -> ProviderFuture<'a> {
        Box::pin(async { anyhow::bail!("startup preflight must not dispatch") })
    }
    fn preflight_input(
        &self,
        target: &ModelProfileRef,
        _: &[ModelRequestItem<'_>],
    ) -> anyhow::Result<ReplayPreflight> {
        self.0.lock().unwrap().push(target.clone());
        Ok(ReplayPreflight::Compatible(ContextTokenEstimate::new(1, 1)))
    }
    fn reset(&mut self) {}
}

#[test]
fn startup_preflight_assesses_each_distinct_profile_once_in_role_order() {
    let first = ModelProfileRef::new("provider", "shared");
    let other = ModelProfileRef::new("provider", "review");
    let profiles = [first.clone(), first.clone(), other.clone()];
    let provider = CountingPreflight(Mutex::new(Vec::new()));
    let assessments = startup_assessments(&provider, &[], profiles.iter());
    assert_eq!(assessments.len(), 2);
    assert_eq!(provider.0.into_inner().unwrap(), [first, other]);
}

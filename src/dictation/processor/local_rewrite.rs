//! Warmup and short-dictation handling for a selected local OpenCode model.

use super::{
    AtomicBool, ContextSnapshot, DictationLabs, Duration, Instant, Model, Processed, Profile,
    Profiles, Source, generation_service, remaining_generation_deadline,
};
use color_eyre::eyre::{Result, eyre};
use serde::Deserialize;
use url::Url;

impl Profiles {
    pub(crate) fn has_local_rewrite(&self) -> bool {
        local_rewrite_model(&self.default).is_some()
            || self
                .contextual
                .iter()
                .any(|mode| local_rewrite_model(&mode.profile).is_some())
    }

    pub(crate) fn warm_local_rewrite(&self, context: &ContextSnapshot, cancelled: &AtomicBool) {
        let profile = self.select(context);
        let Some(model) = local_rewrite_model(profile) else {
            return;
        };
        let started = Instant::now();
        let deadline = profile.deadline.min(Duration::from_secs(10));
        let result = warm_router(model, started, deadline, cancelled);
        tracing::debug!(
            model = %model.id,
            elapsed_ms = started.elapsed().as_millis(),
            ready = result.is_ok(),
            "finished warming the local dictation rewrite model"
        );
    }

    pub(crate) fn short_local_dictation(
        &self,
        transcript: &str,
        language: &str,
        context: &ContextSnapshot,
    ) -> Option<Processed> {
        let profile = self.select(context);
        local_rewrite_model(profile)?;
        Some(Processed {
            text: self.corrected(
                profile,
                transcript,
                language,
                Source::Dictation,
                DictationLabs::current(),
            ),
            nothing: false,
            observation: None,
            transformations: profile.transformations.clone(),
        })
    }
}

/// The lab opts in to the explicitly configured OpenCode `local` provider.
fn local_rewrite_model(profile: &Profile) -> Option<&Model> {
    profile
        .model
        .as_ref()
        .filter(|model| profile.ai_enabled && model.provider == "local")
}

/// Look up the provider's effective address and upstream model ID in the
/// running OpenCode catalog. The model key Hex saves may be an alias.
fn warm_router(
    model: &Model,
    started: Instant,
    deadline: Duration,
    cancelled: &AtomicBool,
) -> Result<()> {
    let service = generation_service(started, deadline, cancelled)?;
    let response = service.request(
        "GET",
        "/api/model",
        None,
        None,
        remaining_generation_deadline(started, deadline)?,
        cancelled,
    )?;
    if response.status != 200 || cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(eyre!("could not read the OpenCode model catalog"));
    }
    let catalog: RouterCatalog = serde_json::from_slice(&response.body)?;
    let (endpoint, model_id) = catalog
        .tokenize_target(model)
        .ok_or_else(|| eyre!("the selected model has no local router address"))?;

    // llama.cpp routes tokenization through the model worker even with empty
    // content. This starts an absent worker and wakes a sleeping one without
    // submitting a completion or supplying any transcript text.
    let remaining = remaining_generation_deadline(started, deadline)?;
    let body = serde_json::json!({ "model": model_id, "content": "" }).to_string();
    crate::net::loopback_agent(|config| config.max_redirects(0).timeout_global(Some(remaining)))
        .post(endpoint.as_str())
        .header("Content-Type", "application/json")
        .send(body.as_str())?;
    Ok(())
}

#[derive(Deserialize)]
struct RouterCatalog {
    data: Vec<RouterModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouterModel {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    settings: Option<RouterSettings>,
}

#[derive(Deserialize)]
struct RouterSettings {
    #[serde(rename = "baseURL")]
    base_url: Option<String>,
}

impl RouterCatalog {
    fn tokenize_target(&self, model: &Model) -> Option<(Url, &str)> {
        let target = self
            .data
            .iter()
            .find(|target| target.provider_id == model.provider && target.id == model.id)?;
        let mut url = Url::parse(target.settings.as_ref()?.base_url.as_deref()?).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        // llama.cpp serves /tokenize beside its OpenAI-compatible /v1 API.
        let has_version = url.path().trim_end_matches('/').ends_with("/v1");
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty();
        if has_version {
            path.pop();
        }
        path.push("tokenize");
        drop(path);
        Some((url, target.model_id.as_deref().unwrap_or(&target.id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_rewrite_warmup_uses_the_router_model_id_from_opencode() {
        let catalog: RouterCatalog = serde_json::from_value(serde_json::json!({
            "data": [{
                "id": "qwen",
                "providerID": "local",
                "modelID": "publisher/qwen:Q4_K_M",
                "settings": { "baseURL": "http://127.0.0.1:9931/v1/" }
            }]
        }))
        .unwrap();
        let model = Model {
            provider: "local".into(),
            id: "qwen".into(),
            variant: None,
        };
        let (endpoint, model_id) = catalog.tokenize_target(&model).unwrap();
        assert_eq!(endpoint.as_str(), "http://127.0.0.1:9931/tokenize");
        assert_eq!(model_id, "publisher/qwen:Q4_K_M");
    }
}

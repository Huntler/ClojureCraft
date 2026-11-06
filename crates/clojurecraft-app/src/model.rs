//! The loaded model: building a load from the current settings, its lifecycle status, recovery
//! actions after failures, and a prompt for trying the model.
//!
//! The inference service reports through the same event channel as other background work. Only
//! events belonging to the request currently shown are applied, so a cancelled or superseded
//! request can never overwrite newer output.

use std::time::{Duration, Instant};

use clojurecraft_hardware::{Backend, GpuVendor};
use clojurecraft_inference::{
    GenerateRequest, GenerationError, InferenceService, LlamaEngine, LoadSpec, MemoryAvailable, MemoryNeed,
    MemoryProbe, ModelStatus, Placement, Remedy, RequestId, Role, Section, ServiceEvent, StreamEvent,
};
use clojurecraft_runtime::catalog::{MemoryEstimate, estimate_from_metadata};
use clojurecraft_runtime::components;
use clojurecraft_runtime::models::ModelStore;
use clojurecraft_runtime::settings::GpuLayers;

use crate::inference::{InferenceEvent, InferenceState};

const LOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Free memory as currently observable. Runs on the inference worker before each load.
struct HardwareProbe;

impl MemoryProbe for HardwareProbe {
    fn available(&self) -> MemoryAvailable {
        let r = clojurecraft_hardware::detect();
        let device = r
            .gpus
            .iter()
            .filter(|g| !g.unified_memory && g.vendor != GpuVendor::Apple)
            .filter_map(|g| Some(g.vram_total_bytes?.saturating_sub(g.vram_used_bytes?)))
            .max();
        MemoryAvailable {
            device,
            host: r.available_ram_bytes,
        }
    }
}

/// How the last request of the "try the model" prompt ended.
#[derive(Clone, Debug, PartialEq)]
pub enum TrialOutcome {
    Done(String),
    Failed { message: String, remedies: Vec<Remedy> },
    Cancelled,
}

#[derive(Default)]
pub struct Trial {
    pub prompt: String,
    pub running: Option<RequestId>,
    pub output: String,
    pub reasoning: String,
    pub outcome: Option<TrialOutcome>,
}

/// What the interface should do after a remedy was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemedyAction {
    None,
    OpenSettings,
    OpenSetup,
}

pub struct ModelRuntime {
    service: Option<InferenceService>,
    pub status: ModelStatus,
    /// When the status last changed.
    pub since: Instant,
    /// The most recent load request and the installed model it used.
    pub loaded: Option<(LoadSpec, String)>,
    /// A load was requested and the service has not reported on it yet.
    load_pending: bool,
    pub trial: Trial,
    /// Request events that are not the trial's, for the specification assistant.
    pub unclaimed: Vec<ServiceEvent>,
}

impl Default for ModelRuntime {
    fn default() -> Self {
        Self {
            service: None,
            status: ModelStatus::Unloaded,
            since: Instant::now(),
            loaded: None,
            load_pending: false,
            trial: Trial::default(),
            unclaimed: Vec::new(),
        }
    }
}

impl ModelRuntime {
    pub fn start(emit: impl Fn(InferenceEvent) + Send + 'static) -> Self {
        let service = InferenceService::new(
            Box::new(LlamaEngine::default()),
            Some(Box::new(HardwareProbe)),
            Box::new(move |e| emit(InferenceEvent::Service(Box::new(e)))),
        );
        Self {
            service: Some(service),
            ..Self::default()
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.status, ModelStatus::Ready { .. })
    }
}

/// Estimated memory for a load with `gpu_layers` of `layers` on the GPU (`None`: all).
fn memory_need(
    est: &MemoryEstimate,
    backend: Backend,
    gpu_layers: Option<u32>,
    layers: Option<u32>,
) -> MemoryNeed {
    match (backend, gpu_layers) {
        // Unified memory and CPU inference both use system memory.
        (Backend::Cpu | Backend::Metal, _) => MemoryNeed {
            device: 0,
            host: est.total(),
        },
        (_, None) => MemoryNeed {
            device: est.total(),
            host: 0,
        },
        (_, Some(n)) => {
            let layers = u64::from(layers.unwrap_or(n).max(1));
            let on_device = est.weights / layers * u64::from(n).min(layers);
            MemoryNeed {
                device: on_device + est.kv_cache + est.overhead,
                host: est.weights.saturating_sub(on_device),
            }
        }
    }
}

/// The parts of a load that require restarting the server when they change.
fn same_load(a: &LoadSpec, b: &LoadSpec) -> bool {
    a.server == b.server && a.model == b.model && a.context == b.context && a.gpu_layers == b.gpu_layers
}

impl InferenceState {
    /// A load of the selected model with the active runtime and current settings.
    pub fn load_spec(&self) -> Result<(LoadSpec, String), String> {
        let (Some(dirs), Some(manifest)) = (&self.dirs, &self.manifest) else {
            return Err("No data directory is available.".into());
        };
        let build = self
            .active_build()
            .ok_or("The llama.cpp runtime is not installed.")?
            .clone();
        // Derived from the last refresh rather than read from disk on every frame; a missing
        // program is reported when loading.
        let server = dirs
            .runtimes()
            .join(components::LLAMA_CPP)
            .join(&build.version)
            .join(&manifest.llama_cpp.server_path);
        let record = self.active_model().ok_or("No model is selected.")?.clone();
        if let Some(p) = self.problems(&record.key).first() {
            return Err(format!("The selected model needs attention: {p}."));
        }
        let model = ModelStore::new(dirs.models(), dirs.downloads())
            .primary_file(&record)
            .ok_or("The selected model has no files.")?;
        let backend = build.backend.unwrap_or(Backend::Cpu);
        let context = self.context_for(&record);
        let gpu_layers = if backend == Backend::Cpu {
            Some(0)
        } else {
            match self.settings.gpu_layers {
                GpuLayers::All => None,
                GpuLayers::Count(n) => Some(n),
                GpuLayers::Auto => self
                    .offload_for(&record)
                    .filter(|p| !p.is_full())
                    .map(|p| p.gpu_layers),
            }
        };
        let memory = record.kv_bytes_per_token.map(|kv| {
            memory_need(
                &estimate_from_metadata(record.total_size(), kv, context),
                backend,
                gpu_layers,
                record.layers,
            )
        });
        let spec = LoadSpec {
            server,
            model,
            label: format!("{} · {}", record.display_name, record.quant),
            backend: backend.label().to_owned(),
            runtime: build.version.clone(),
            context,
            gpu_layers,
            total_layers: record.layers,
            model_max_context: record.context_length,
            threads: None,
            load_timeout: LOAD_TIMEOUT,
            memory,
            skip_memory_check: false,
            pid_file: Some(dirs.root().join("llama-server.pid")),
        };
        Ok((spec, record.key))
    }

    pub fn load_model(&mut self, skip_memory_check: bool) {
        let (mut spec, key) = match self.load_spec() {
            Ok(s) => s,
            Err(e) => {
                self.notices.push(e);
                return;
            }
        };
        spec.skip_memory_check = skip_memory_check;
        if let Some(service) = &self.model.service {
            service.load(spec.clone());
            self.model.loaded = Some((spec, key));
            self.model.load_pending = true;
        }
    }

    pub fn unload_model(&mut self) {
        if let Some(service) = &self.model.service {
            service.unload();
        }
        self.model.loaded = None;
    }

    /// Unloads the model if it uses the installed model `key`, before that model is removed.
    pub fn release_model(&mut self, key: &str) {
        if self.model.loaded.as_ref().is_some_and(|(_, k)| k == key) {
            self.unload_model();
        }
    }

    /// The model is running with settings that differ from the current ones.
    pub fn model_needs_reload(&self) -> bool {
        if !self.model.is_ready() {
            return false;
        }
        match (&self.model.loaded, self.load_spec()) {
            (Some((loaded, _)), Ok((current, _))) => !same_load(loaded, &current),
            _ => false,
        }
    }

    pub fn apply_remedy(&mut self, remedy: &Remedy) -> RemedyAction {
        match remedy {
            Remedy::FewerGpuLayers(n) => {
                self.settings.gpu_layers = GpuLayers::Count(*n);
                self.save_settings();
                self.load_model(false);
            }
            Remedy::SmallerContext(n) | Remedy::LargerContext(n) => {
                self.settings.context_limit = Some(*n);
                self.save_settings();
                self.load_model(false);
            }
            Remedy::LoadAnyway => self.load_model(true),
            Remedy::Restart => self.load_model(false),
            Remedy::SmallerModel => return RemedyAction::OpenSettings,
            Remedy::CpuBackend => return RemedyAction::OpenSetup,
            Remedy::ShortenInput(_) => {}
        }
        RemedyAction::None
    }

    /// Makes sure the selected model is loaded (or loading) with the current settings, starting a
    /// load when needed. Returns the load and the installed model's key.
    pub fn ensure_model(&mut self) -> Result<(LoadSpec, String), String> {
        if let Some(m) = self.missing().first() {
            return Err(m.describe());
        }
        let (spec, key) = self.load_spec()?;
        // A load requested a moment ago counts too; its status has not arrived yet.
        let current = (self.model.load_pending
            || matches!(
                self.model.status,
                ModelStatus::Ready { .. } | ModelStatus::Loading { .. }
            ))
            && self
                .model
                .loaded
                .as_ref()
                .is_some_and(|(loaded, k)| *k == key && same_load(loaded, &spec));
        if !current {
            if self.model.service.is_none() {
                return Err("The inference service is not running.".into());
            }
            self.load_model(false);
        }
        Ok((spec, key))
    }

    /// Queues a request after the model's pending load, if any.
    pub fn generate(&mut self, request: GenerateRequest) -> Result<RequestId, String> {
        let service = self
            .model
            .service
            .as_ref()
            .ok_or("The inference service is not running.")?;
        Ok(service.generate(request))
    }

    pub fn cancel_request(&self, id: RequestId) {
        if let Some(service) = &self.model.service {
            service.cancel(id);
        }
    }

    pub fn start_trial(&mut self) {
        let Some(service) = &self.model.service else {
            return;
        };
        let prompt = self.model.trial.prompt.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        if let Some(previous) = self.model.trial.running.take() {
            service.cancel(previous);
        }
        let request = GenerateRequest::new(
            Role::Explanation,
            vec![Section::required("Question", Placement::User, prompt)],
        );
        let t = &mut self.model.trial;
        t.output.clear();
        t.reasoning.clear();
        t.outcome = None;
        t.running = Some(service.generate(request));
    }

    pub fn cancel_trial(&mut self) {
        if let (Some(service), Some(id)) = (&self.model.service, self.model.trial.running) {
            service.cancel(id);
        }
    }

    pub fn shutdown_model(&mut self) {
        if let Some(mut service) = self.model.service.take() {
            service.shutdown();
        }
    }

    pub fn handle_service(&mut self, event: ServiceEvent) {
        let trial = &mut self.model.trial;
        match event {
            ServiceEvent::Status(status) => {
                if let ModelStatus::Failed { error, .. } = &status {
                    tracing::warn!(%error, "model failed to load");
                }
                // An earlier unload may still report `Unloaded` after a new load was requested.
                if !matches!(status, ModelStatus::Unloaded) {
                    self.model.load_pending = false;
                }
                self.model.status = status;
                self.model.since = Instant::now();
            }
            ServiceEvent::Stream { request, event } if trial.running == Some(request) => match event {
                StreamEvent::Content(t) => trial.output.push_str(&t),
                StreamEvent::Reasoning(t) => trial.reasoning.push_str(&t),
                StreamEvent::ToolCall { .. } => {}
            },
            ServiceEvent::Finished { request, result } if trial.running == Some(request) => {
                trial.running = None;
                let context = self.model.loaded.as_ref().map_or(0, |(s, _)| s.context);
                trial.outcome = Some(match result {
                    Ok(g) => {
                        let mut parts = vec![format!(
                            "prompt {} of {} tokens{}",
                            g.plan.prompt_tokens,
                            g.plan.context,
                            if g.plan.exact { "" } else { " (estimated)" }
                        )];
                        if let Some(n) = g.usage.completion_tokens {
                            parts.push(format!("{n} tokens generated"));
                        }
                        if let Some(r) = g.usage.tokens_per_second {
                            parts.push(format!("{r:.1} tokens/s"));
                        }
                        parts.push(format!("{:.1} s", g.elapsed.as_secs_f64()));
                        if g.finish == clojurecraft_inference::Finish::Length {
                            parts.push("stopped at the answer length limit".into());
                        }
                        TrialOutcome::Done(parts.join(" · "))
                    }
                    Err(GenerationError::Cancelled) => TrialOutcome::Cancelled,
                    Err(e) => {
                        let remedies = match &e {
                            GenerationError::ContextExceeded { remedies, .. } => remedies.clone(),
                            GenerationError::OutOfMemory(_) if context > 4096 => {
                                vec![Remedy::SmallerContext((context / 2).max(4096))]
                            }
                            _ => Vec::new(),
                        };
                        TrialOutcome::Failed {
                            message: e.to_string(),
                            remedies,
                        }
                    }
                });
            }
            // Output of other requests; the assistant applies only those it is waiting for.
            other @ (ServiceEvent::Stream { .. } | ServiceEvent::Finished { .. }) => {
                self.model.unclaimed.push(other);
            }
            ServiceEvent::Tool { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use clojurecraft_inference::{GenerationError, PromptPlan, StreamEvent};
    use clojurecraft_runtime::catalog::MemoryEstimate;

    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn memory_need_follows_backend_and_offload() {
        let est = MemoryEstimate {
            weights: 8 * GIB,
            kv_cache: GIB,
            overhead: GIB,
        };
        assert_eq!(
            memory_need(&est, Backend::Cpu, Some(0), Some(32)),
            MemoryNeed {
                device: 0,
                host: 10 * GIB
            }
        );
        assert_eq!(
            memory_need(&est, Backend::Metal, None, Some(32)),
            MemoryNeed {
                device: 0,
                host: 10 * GIB
            }
        );
        assert_eq!(
            memory_need(&est, Backend::Cuda, None, Some(32)),
            MemoryNeed {
                device: 10 * GIB,
                host: 0
            }
        );
        assert_eq!(
            memory_need(&est, Backend::Cuda, Some(16), Some(32)),
            MemoryNeed {
                device: 6 * GIB,
                host: 4 * GIB
            }
        );
    }

    fn generation(text: &str) -> Box<clojurecraft_inference::Generation> {
        Box::new(clojurecraft_inference::Generation {
            role: Role::Explanation,
            model: "m".into(),
            text: text.into(),
            reasoning: String::new(),
            value: None,
            finish: clojurecraft_inference::Finish::Stop,
            usage: clojurecraft_inference::Usage::default(),
            plan: PromptPlan {
                included: vec![0],
                omitted: vec![],
                prompt_tokens: 10,
                reserved_output: 100,
                context: 4096,
                exact: true,
            },
            tool_calls: vec![],
            tools_unavailable: false,
            elapsed: Duration::from_millis(10),
        })
    }

    #[test]
    fn output_of_a_superseded_request_never_replaces_the_current_one() {
        let mut st = crate::inference::tests::state();
        st.model.trial.running = Some(2);
        let stream = |request, t: &str| ServiceEvent::Stream {
            request,
            event: StreamEvent::Content(t.into()),
        };
        st.handle_service(stream(1, "old"));
        st.handle_service(stream(2, "new"));
        st.handle_service(ServiceEvent::Finished {
            request: 1,
            result: Ok(generation("old")),
        });
        assert_eq!(st.model.trial.output, "new");
        assert_eq!(st.model.trial.running, Some(2));
        assert_eq!(st.model.trial.outcome, None);
        st.handle_service(ServiceEvent::Finished {
            request: 2,
            result: Ok(generation("new")),
        });
        assert!(matches!(st.model.trial.outcome, Some(TrialOutcome::Done(_))));
        // A late event of the finished request changes nothing.
        st.handle_service(stream(2, "late"));
        assert_eq!(st.model.trial.output, "new");
    }

    #[test]
    fn context_overflow_offers_its_remedies() {
        let mut st = crate::inference::tests::state();
        st.model.trial.running = Some(1);
        st.handle_service(ServiceEvent::Finished {
            request: 1,
            result: Err(GenerationError::ContextExceeded {
                needed: 9000,
                context: 8192,
                reserved_output: 1200,
                largest: vec![("Question".into(), 7000)],
                remedies: vec![Remedy::LargerContext(9216)],
            }),
        });
        assert!(matches!(
            &st.model.trial.outcome,
            Some(TrialOutcome::Failed { remedies, .. }) if remedies == &vec![Remedy::LargerContext(9216)]
        ));
    }

    #[test]
    fn remedies_change_the_settings_they_name() {
        let mut st = crate::inference::tests::state();
        assert_eq!(st.apply_remedy(&Remedy::FewerGpuLayers(20)), RemedyAction::None);
        assert_eq!(st.settings.gpu_layers, GpuLayers::Count(20));
        st.apply_remedy(&Remedy::SmallerContext(8192));
        assert_eq!(st.settings.context_limit, Some(8192));
        assert_eq!(st.apply_remedy(&Remedy::SmallerModel), RemedyAction::OpenSettings);
        assert_eq!(st.apply_remedy(&Remedy::CpuBackend), RemedyAction::OpenSetup);
    }

    #[test]
    fn a_requested_load_is_not_requested_again_before_it_reports() {
        let mut st = crate::inference::tests::state();
        st.model.loaded = Some((dummy_spec(), "k".into()));
        st.model.load_pending = true;
        // Status is still `Unloaded`, but the pending load counts as current.
        assert!(matches!(st.model.status, ModelStatus::Unloaded));
        st.handle_service(ServiceEvent::Status(ModelStatus::Unloaded));
        assert!(
            st.model.load_pending,
            "a late Unloaded does not clear the pending load"
        );
        st.handle_service(ServiceEvent::Status(ModelStatus::Loading {
            model: "m".into(),
            phase: clojurecraft_inference::LoadPhase::Starting,
        }));
        assert!(!st.model.load_pending);
    }

    fn dummy_spec() -> LoadSpec {
        LoadSpec {
            server: "s".into(),
            model: "m".into(),
            label: "m".into(),
            backend: "CPU".into(),
            runtime: "b1-cpu".into(),
            context: 4096,
            gpu_layers: Some(0),
            total_layers: None,
            model_max_context: None,
            threads: None,
            load_timeout: Duration::from_secs(1),
            memory: None,
            skip_memory_check: false,
            pid_file: None,
        }
    }

    #[test]
    fn status_events_replace_the_shown_status() {
        let mut st = crate::inference::tests::state();
        st.handle_service(ServiceEvent::Status(ModelStatus::Crashed {
            model: "m".into(),
            detail: "terminated by signal 11".into(),
            restarting: true,
            log: String::new(),
        }));
        assert!(matches!(
            st.model.status,
            ModelStatus::Crashed { restarting: true, .. }
        ));
        st.handle_service(ServiceEvent::Status(ModelStatus::Unloaded));
        assert!(!st.model.is_ready());
    }
}

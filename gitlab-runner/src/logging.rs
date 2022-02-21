use tracing::{field, metadata::LevelFilter, Subscriber};
use tracing_subscriber::{registry::LookupSpan, Layer};

use crate::{job::JobData, runlist::RunList};

#[derive(Clone, Debug)]
struct GitlabJob(u64);

#[derive(Debug)]
struct GitlabJobFinder(Option<GitlabJob>);

impl field::Visit for GitlabJobFinder {
    fn record_u64(&mut self, field: &field::Field, value: u64) {
        if field.name() == "gitlab.job" {
            self.0 = Some(GitlabJob(value));
        }
    }

    fn record_debug(&mut self, _field: &field::Field, _value: &dyn std::fmt::Debug) {}
}

#[derive(Debug, Default)]
struct GitlabOutput(bool);
impl field::Visit for GitlabOutput {
    fn record_bool(&mut self, field: &field::Field, value: bool) {
        if field.name() == "gitlab.output" {
            self.0 = value
        }
    }

    fn record_debug(&mut self, _field: &field::Field, _value: &dyn std::fmt::Debug) {}
}

#[derive(Debug)]
struct OutputToGitlab {
    jobdata: JobData,
}

impl field::Visit for OutputToGitlab {
    fn record_str(&mut self, field: &field::Field, value: &str) {
        if field.name() == "message" {
            self.jobdata.trace(format!("{}\n", value).as_bytes());
        }
    }

    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.jobdata.trace(format!("{:?}\n", value).as_bytes());
        }
    }
}

pub struct GitlabLayer {
    run_list: RunList<u64, JobData>,
}

impl GitlabLayer {
    pub(crate) fn new(run_list: RunList<u64, JobData>) -> Self {
        GitlabLayer { run_list }
    }
}

impl<S> Layer<S> for GitlabLayer
where
    S: Subscriber + Send + Sync + 'static,
    S: for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut gitlab_output = GitlabOutput::default();
        event.record(&mut gitlab_output);

        if gitlab_output.0 {
            if let Some(scope) = ctx.event_scope(event) {
                if let Some(jobinfo) = scope
                    .from_root()
                    .find_map(|span| span.extensions().get::<GitlabJob>().cloned())
                {
                    if let Some(jobdata) = self.run_list.lookup(&jobinfo.0) {
                        event.record(&mut OutputToGitlab { jobdata });
                    }
                }
            }
        }
    }

    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        let _ = (metadata, ctx);
        true
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn on_layer(&mut self, subscriber: &mut S) {
        subscriber.register_filter();
    }

    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut f = GitlabJobFinder(None);
        attrs.record(&mut f);
        if let Some(job) = f.0 {
            let span = ctx.span(id).unwrap();
            let mut extensions = span.extensions_mut();
            extensions.insert(job);
        }
    }
}

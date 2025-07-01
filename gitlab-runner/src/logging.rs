use tracing::{field, metadata::LevelFilter, subscriber::Interest, Metadata, Subscriber};
use tracing_subscriber::{
    filter::Filtered,
    layer::{Context, Filter},
    registry::LookupSpan,
    Layer,
};

use crate::{
    job::JobLog,
    runlist::{JobRunList, RunList},
};

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

struct GitlabOutputEnabled;

#[derive(Debug, Default)]
struct GitlabOutputFinder(bool);

impl field::Visit for GitlabOutputFinder {
    fn record_bool(&mut self, field: &field::Field, value: bool) {
        if field.name() == "gitlab.output" {
            self.0 = value
        }
    }

    fn record_debug(&mut self, _field: &field::Field, _value: &dyn std::fmt::Debug) {}
}

#[derive(Debug)]
struct OutputToGitlab {
    joblog: JobLog,
}

impl field::Visit for OutputToGitlab {
    fn record_str(&mut self, field: &field::Field, value: &str) {
        if field.name() == "message" {
            self.joblog.trace(format!("{}\n", value).as_bytes());
        }
    }

    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.joblog.trace(format!("{:?}\n", value).as_bytes());
        }
    }
}

/// A [`Layer`] for gitlab
///
/// This tracing layer interfaces the tracing infrastructure with running gitlab jobs. It always
/// has to be registered in the current subscriber
pub struct GitlabLayer {
    run_list: RunList<u64, JobLog>,
}

impl GitlabLayer {
    /// Create a new GitlabLayer which should be added to the global subscriber
    /// and a jobs list which should be added to the runner
    /// ```
    /// # use gitlab_runner::GitlabLayer;
    /// # use tracing_subscriber::{prelude::*, Registry};
    /// #
    /// let (layer, _jobs) = GitlabLayer::new();
    /// let subscriber = Registry::default().with(layer).init();
    /// ```
    pub fn new<S>() -> (Filtered<Self, GitlabFilter, S>, JobRunList)
    where
        S: Subscriber + for<'span> LookupSpan<'span> + 'static,
    {
        let run_list = RunList::new();
        let job_run_list = JobRunList::from(run_list.clone());
        (
            Filtered::new(GitlabLayer { run_list }, GitlabFilter {}),
            job_run_list,
        )
    }
}

impl<S> Layer<S> for GitlabLayer
where
    S: Subscriber + Send + Sync + 'static,
    S: for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut gitlab_output = GitlabOutputFinder::default();
        event.record(&mut gitlab_output);

        if gitlab_output.0
            || ctx.event_scope(event).is_some_and(|scope| {
                scope
                    .from_root()
                    .any(|span| span.extensions().get::<GitlabOutputEnabled>().is_some())
            })
        {
            if let Some(scope) = ctx.event_scope(event) {
                if let Some(jobinfo) = scope
                    .from_root()
                    .find_map(|span| span.extensions().get::<GitlabJob>().cloned())
                {
                    if let Some(joblog) = self.run_list.lookup(&jobinfo.0) {
                        event.record(&mut OutputToGitlab { joblog });
                    }
                }
            }
        }
    }

    fn enabled(&self, _metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        // This only gets called if the filters enabled returns true, so no need for futher checks
        true
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn on_layer(&mut self, subscriber: &mut S) {
        subscriber.register_filter();
    }

    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        // This only gets called if the filters callsite_enabled returned !never, so no need to
        // check further
        Interest::always()
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let mut f = GitlabJobFinder(None);
        attrs.record(&mut f);
        if let Some(job) = f.0 {
            let span = ctx.span(id).unwrap();
            let mut extensions = span.extensions_mut();
            extensions.insert(job);
        }

        let mut f = GitlabOutputFinder(false);
        attrs.record(&mut f);
        if f.0 {
            let span = ctx.span(id).unwrap();
            let mut extensions = span.extensions_mut();
            extensions.insert(GitlabOutputEnabled);
        }
    }
}

pub struct GitlabFilter {}

impl GitlabFilter {
    // Only spans and events with gitlab fields are of interest
    fn is_enabled(&self, metadata: &Metadata) -> bool {
        metadata
            .fields()
            .iter()
            .any(|f| f.name().starts_with("gitlab."))
    }
}

impl<S: Subscriber + for<'span> LookupSpan<'span>> Filter<S> for GitlabFilter {
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        self.is_enabled(meta)
            || cx
                .current_span()
                .id()
                .and_then(|id| cx.span_scope(id))
                .is_some_and(|scope| {
                    scope
                        .from_root()
                        .any(|span| self.is_enabled(span.metadata()))
                })
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        if self.is_enabled(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }
}

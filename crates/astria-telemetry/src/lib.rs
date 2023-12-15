//! Initialize telemetry in all astria services.
//!
//! # Examples
//! ```no_run
//! astria_telemetry::configure()
//!     .otel_endpoint("http://otel-collector.monitoring:4317")
//!     .filter_directives("info")
//!     .try_init()
//!     .expect("must be able to initialize telemetry");
//! tracing::info!("telemetry initialized");
//! ```
use std::io::IsTerminal as _;

use opentelemetry::{
    global,
    trace::TracerProvider as _,
};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::{
    runtime::Tokio,
    trace::TracerProvider,
};
use opentelemetry_stdout::SpanExporter;
use tracing_subscriber::{
    filter::{
        LevelFilter,
        ParseError,
    },
    layer::SubscriberExt as _,
    util::{
        SubscriberInitExt as _,
        TryInitError,
    },
    EnvFilter,
};

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct Error(ErrorKind);

impl Error {
    fn otlp(source: opentelemetry::trace::TraceError) -> Self {
        Self(ErrorKind::Otlp(source))
    }

    fn filter_directives(source: ParseError) -> Self {
        Self(ErrorKind::FilterDirectives(source))
    }

    fn init_subscriber(source: TryInitError) -> Self {
        Self(ErrorKind::InitSubscriber(source))
    }
}

#[derive(Debug, thiserror::Error)]
enum ErrorKind {
    #[error("failed constructing opentelemetry otlp exporter")]
    Otlp(#[source] opentelemetry::trace::TraceError),
    #[error("failed to parse filter directives")]
    FilterDirectives(#[source] ParseError),
    #[error("failed installing global tracing subscriber")]
    InitSubscriber(#[source] TryInitError),
}

#[must_use = "the otel config must be initialized to be useful"]
pub fn configure() -> Config {
    Config::new()
}

#[derive(Copy, Clone, Debug, Default)]
enum Stdout {
    Always,
    #[default]
    IfTty,
    Never,
}

impl Stdout {
    fn is_always(self) -> bool {
        matches!(self, Self::Always)
    }

    fn is_if_tty(self) -> bool {
        matches!(self, Self::IfTty)
    }
}

pub struct Config {
    filter_directives: String,
    stdout: Stdout,
    otel_endpoint: Option<String>,
}

impl Config {
    #[must_use = "telemetry must be initialized to be useful"]
    fn new() -> Self {
        Self {
            filter_directives: String::new(),
            stdout: Stdout::default(),
            otel_endpoint: None,
        }
    }
}

impl Config {
    #[must_use = "telemetry must be initialized to be useful"]
    pub fn filter_directives(self, filter_directives: &str) -> Self {
        Self {
            filter_directives: filter_directives.to_string(),
            ..self
        }
    }

    #[must_use = "telemetry must be initialized to be useful"]
    pub fn stdout_always(self) -> Self {
        Self {
            stdout: Stdout::Always,
            ..self
        }
    }

    #[must_use = "telemetry must be initialized to be useful"]
    pub fn stdout_never(self) -> Self {
        Self {
            stdout: Stdout::Never,
            ..self
        }
    }

    #[must_use = "telemetry must be initialized to be useful"]
    pub fn otel_endpoint(self, otel_endpoint: &str) -> Self {
        Self {
            otel_endpoint: Some(otel_endpoint.to_string()),
            ..self
        }
    }

    /// Initialize telemetry, consuming the config.
    ///
    /// # Errors
    /// Fails if the filter directives could not be parsed, if communication with the OTLP
    /// endpoint failed, or if the global tracing subscriber could not be installed.
    pub fn try_init(self) -> Result<(), Error> {
        let Self {
            filter_directives,
            otel_endpoint,
            stdout,
        } = self;

        let env_filter = {
            let builder = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into());
            builder
                .parse(filter_directives)
                .map_err(Error::filter_directives)?
        };

        let mut tracer_provider = TracerProvider::builder();
        if let Some(otel_endpoint) = otel_endpoint {
            let otel_exporter = opentelemetry_otlp::new_exporter()
                .tonic()
                // XXX: will get overriden by env var OTEL_EXPORTER_OTLP_TRACES_ENDPOINT
                .with_endpoint(otel_endpoint)
                .build_span_exporter()
                .map_err(Error::otlp)?;

            tracer_provider = tracer_provider.with_batch_exporter(otel_exporter, Tokio);
        }

        if stdout.is_always() || (stdout.is_if_tty() && std::io::stdout().is_terminal()) {
            tracer_provider = tracer_provider.with_simple_exporter(SpanExporter::default());
        }
        let tracer_provider = tracer_provider.build();

        let tracer = tracer_provider.versioned_tracer(
            "astria-telemetry",
            Some(env!("CARGO_PKG_VERSION")),
            Some(opentelemetry_semantic_conventions::SCHEMA_URL),
            None,
        );
        let _ = global::set_tracer_provider(tracer_provider);

        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        tracing_subscriber::registry()
            .with(otel_layer)
            .with(env_filter)
            .try_init()
            .map_err(Error::init_subscriber)?;

        Ok(())
    }
}

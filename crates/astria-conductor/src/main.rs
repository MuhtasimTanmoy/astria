use std::process::ExitCode;

use astria_conductor::{
    Conductor,
    Config,
};
use color_eyre::eyre::WrapErr as _;
use tracing::{
    error,
    info,
};

// Following the BSD convention for failing to read config
// See here: https://freedesktop.org/software/systemd/man/systemd.exec.html#Process%20Exit%20Codes
const EX_CONFIG: u8 = 78;

#[tokio::main]
async fn main() -> ExitCode {
    let cfg: Config = match config::get() {
        Err(e) => {
            eprintln!("failed reading config:\n{e:?}");
            // FIXME (https://github.com/astriaorg/astria/issues/368): might have to bubble up exit codes, since we might need
            //        to exit with other exit codes if something else fails
            return ExitCode::from(EX_CONFIG);
        }
        Ok(cfg) => cfg,
    };

    if let Err(e) = telemetry::configure()
        .otel_endpoint("http://otel-collector.monitoring:4317")
        .filter_directives(&cfg.log)
        .try_init()
        .wrap_err("failed to setup telemetry")
    {
        eprintln!("initializing sequencer failed:\n{e:?}");
        return ExitCode::FAILURE;
    }

    info!(
        config = serde_json::to_string(&cfg).expect("serializing to a string cannot fail"),
        "initializing conductor"
    );

    let conductor = match Conductor::new(cfg).await {
        Err(e) => {
            let error: &(dyn std::error::Error + 'static) = e.as_ref();
            error!(error, "failed initializing conductor");
            return ExitCode::FAILURE;
        }
        Ok(conductor) => conductor,
    };

    conductor.run_until_stopped().await;
    info!("conductor stopped");
    ExitCode::SUCCESS
}

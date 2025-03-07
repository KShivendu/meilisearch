use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use actix_web::http::KeepAlive;
use actix_web::web::Data;
use actix_web::HttpServer;
use index_scheduler::IndexScheduler;
use meilisearch::analytics::Analytics;
use meilisearch::option::LogLevel;
use meilisearch::{analytics, create_app, setup_meilisearch, Opt};
use meilisearch_auth::{generate_master_key, AuthController, MASTER_KEY_MIN_SIZE};

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// does all the setup before meilisearch is launched
fn setup(opt: &Opt) -> anyhow::Result<()> {
    let mut log_builder = env_logger::Builder::new();
    log_builder.parse_filters(&opt.log_level.to_string());
    if matches!(opt.log_level, LogLevel::Info) {
        // if we are in info we only allow the warn log_level for milli
        log_builder.filter_module("milli", log::LevelFilter::Warn);
    }

    log_builder.init();

    Ok(())
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    let (opt, config_read_from) = Opt::try_build()?;

    setup(&opt)?;

    if opt.generate_master_key {
        println!("{}", generate_master_key());
        return Ok(());
    }

    match (opt.env.as_ref(), &opt.master_key) {
        ("production", Some(master_key)) if master_key.len() < MASTER_KEY_MIN_SIZE => {
            anyhow::bail!(
                "In production mode, the master key must be of at least {MASTER_KEY_MIN_SIZE} characters, but the provided key is only {} characters long

We generated a secure Master Key for you (you can safely copy this token):

>> export MEILI_MASTER_KEY={} <<",
                master_key.len(),
                generate_master_key(),
            )
        }
        ("production", None) => {
            anyhow::bail!(
                "In production mode, you must provide a master key to secure your instance. It can be specified via the MEILI_MASTER_KEY environment variable or the --master-key launch option.

We generated a secure Master Key for you (you can safely copy this token):

>> export MEILI_MASTER_KEY={} <<
",
                generate_master_key()
            )
        }
        // No error; continue
        _ => (),
    }

    let (index_scheduler, auth_controller) = setup_meilisearch(&opt)?;

    #[cfg(all(not(debug_assertions), feature = "analytics"))]
    let analytics = if !opt.no_analytics {
        analytics::SegmentAnalytics::new(&opt, index_scheduler.clone()).await
    } else {
        analytics::MockAnalytics::new(&opt)
    };
    #[cfg(any(debug_assertions, not(feature = "analytics")))]
    let analytics = analytics::MockAnalytics::new(&opt);

    print_launch_resume(&opt, analytics.clone(), config_read_from);

    run_http(index_scheduler, auth_controller, opt, analytics).await?;

    Ok(())
}

async fn run_http(
    index_scheduler: Arc<IndexScheduler>,
    auth_controller: AuthController,
    opt: Opt,
    analytics: Arc<dyn Analytics>,
) -> anyhow::Result<()> {
    let enable_dashboard = &opt.env == "development";
    let opt_clone = opt.clone();
    let index_scheduler = Data::from(index_scheduler);

    let http_server = HttpServer::new(move || {
        create_app(
            index_scheduler.clone(),
            auth_controller.clone(),
            opt.clone(),
            analytics.clone(),
            enable_dashboard,
        )
    })
    // Disable signals allows the server to terminate immediately when a user enter CTRL-C
    .disable_signals()
    .keep_alive(KeepAlive::Os);

    if let Some(config) = opt_clone.get_ssl_config()? {
        http_server.bind_rustls(opt_clone.http_addr, config)?.run().await?;
    } else {
        http_server.bind(&opt_clone.http_addr)?.run().await?;
    }
    Ok(())
}

pub fn print_launch_resume(
    opt: &Opt,
    analytics: Arc<dyn Analytics>,
    config_read_from: Option<PathBuf>,
) {
    let commit_sha = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    let commit_date = option_env!("VERGEN_GIT_COMMIT_TIMESTAMP").unwrap_or("unknown");
    let protocol =
        if opt.ssl_cert_path.is_some() && opt.ssl_key_path.is_some() { "https" } else { "http" };
    let ascii_name = r#"
888b     d888          d8b 888 d8b                                            888
8888b   d8888          Y8P 888 Y8P                                            888
88888b.d88888              888                                                888
888Y88888P888  .d88b.  888 888 888 .d8888b   .d88b.   8888b.  888d888 .d8888b 88888b.
888 Y888P 888 d8P  Y8b 888 888 888 88K      d8P  Y8b     "88b 888P"  d88P"    888 "88b
888  Y8P  888 88888888 888 888 888 "Y8888b. 88888888 .d888888 888    888      888  888
888   "   888 Y8b.     888 888 888      X88 Y8b.     888  888 888    Y88b.    888  888
888       888  "Y8888  888 888 888  88888P'  "Y8888  "Y888888 888     "Y8888P 888  888
"#;

    eprintln!("{}", ascii_name);

    eprintln!(
        "Config file path:\t{:?}",
        config_read_from
            .map(|config_file_path| config_file_path.display().to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    eprintln!("Database path:\t\t{:?}", opt.db_path);
    eprintln!("Server listening on:\t\"{}://{}\"", protocol, opt.http_addr);
    eprintln!("Environment:\t\t{:?}", opt.env);
    eprintln!("Commit SHA:\t\t{:?}", commit_sha.to_string());
    eprintln!("Commit date:\t\t{:?}", commit_date.to_string());
    eprintln!("Package version:\t{:?}", env!("CARGO_PKG_VERSION").to_string());

    #[cfg(all(not(debug_assertions), feature = "analytics"))]
    {
        if !opt.no_analytics {
            eprintln!(
                "
Thank you for using Meilisearch!

We collect anonymized analytics to improve our product and your experience. To learn more, including how to turn off analytics, visit our dedicated documentation page: https://docs.meilisearch.com/learn/what_is_meilisearch/telemetry.html

Anonymous telemetry:\t\"Enabled\""
            );
        } else {
            eprintln!("Anonymous telemetry:\t\"Disabled\"");
        }
    }

    if let Some(instance_uid) = analytics.instance_uid() {
        eprintln!("Instance UID:\t\t\"{}\"", instance_uid);
    }

    eprintln!();

    match (opt.env.as_ref(), &opt.master_key) {
        ("production", Some(_)) => {
            eprintln!("A Master Key has been set. Requests to Meilisearch won't be authorized unless you provide an authentication key.");
        }
        ("development", Some(master_key)) => {
            eprintln!("A Master Key has been set. Requests to Meilisearch won't be authorized unless you provide an authentication key.");

            if master_key.len() < MASTER_KEY_MIN_SIZE {
                eprintln!();
                log::warn!(
                    "The provided Master Key is too short (< {MASTER_KEY_MIN_SIZE} characters)"
                );
                eprintln!("A Master Key of at least {MASTER_KEY_MIN_SIZE} characters will be required when switching to the production environment.");
                eprintln!("Restart Meilisearch with the `--generate-master-key` flag to generate a secure Master Key you can use");
            }
        }
        ("development", None) => {
            log::warn!("No master key found; The server will accept unidentified requests");
            eprintln!("If you need some protection in development mode, please export a key:\n\nexport MEILI_MASTER_KEY={}", generate_master_key());
            eprintln!("\nA Master Key of at least {MASTER_KEY_MIN_SIZE} characters will be required when switching to the production environment.");
        }
        // unreachable because Opt::try_build above would have failed already if any other value had been produced
        _ => unreachable!(),
    }

    eprintln!();
    eprintln!("Documentation:\t\thttps://docs.meilisearch.com");
    eprintln!("Source code:\t\thttps://github.com/meilisearch/meilisearch");
    eprintln!("Contact:\t\thttps://docs.meilisearch.com/resources/contact.html");
    eprintln!();
}

use crate::utils::context::Context;
use crate::utils::locale::system_locale;
use crate::utils::log::{progress, CliLogger};
use anyhow::bail;
use anyhow::Result;
use clap::{Parser, Subcommand};
use crunchyroll_rs::crunchyroll::CrunchyrollBuilder;
use crunchyroll_rs::{Crunchyroll, Locale};
use log::{debug, error, warn, LevelFilter};
use reqwest::Proxy;
use std::{env, fs};

mod archive;
mod download;
mod login;
mod utils;

pub use archive::Archive;
use crunchyroll_rs::error::CrunchyrollError;
pub use download::Download;
pub use login::Login;

#[async_trait::async_trait(?Send)]
trait Execute {
    fn pre_check(&mut self) -> Result<()> {
        Ok(())
    }
    async fn execute(mut self, ctx: Context) -> Result<()>;
}

#[derive(Debug, Parser)]
#[clap(author, version = version(), about)]
#[clap(name = "crunchy-cli")]
pub struct Cli {
    #[clap(flatten)]
    verbosity: Option<Verbosity>,

    #[arg(
        help = "Overwrite the language in which results are returned. Default is your system language"
    )]
    #[arg(long)]
    lang: Option<Locale>,

    #[arg(help = "Enable experimental fixes which may resolve some unexpected errors")]
    #[arg(
        long_help = "Enable experimental fixes which may resolve some unexpected errors. \
            If everything works as intended this option isn't needed, but sometimes Crunchyroll mislabels \
            the audio of a series/season or episode or returns a wrong season number. This is when using this option might help to solve the issue"
    )]
    #[arg(long, default_value_t = false)]
    experimental_fixes: bool,

    #[clap(flatten)]
    login_method: LoginMethod,

    #[arg(help = "Use a proxy to route all traffic through")]
    #[arg(long_help = "Use a proxy to route all traffic through. \
            Make sure that the proxy can either forward TLS requests, which is needed to bypass the (cloudflare) bot protection, or that it is configured so that the proxy can bypass the protection itself")]
    #[clap(long)]
    #[arg(value_parser = crate::utils::clap::clap_parse_proxy)]
    proxy: Option<Proxy>,

    #[clap(subcommand)]
    command: Command,
}

fn version() -> String {
    let package_version = env!("CARGO_PKG_VERSION");
    let git_commit_hash = env!("GIT_HASH");
    let build_date = env!("BUILD_DATE");

    if git_commit_hash.is_empty() {
        format!("{}", package_version)
    } else {
        format!("{} ({} {})", package_version, git_commit_hash, build_date)
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    Archive(Archive),
    Download(Download),
    Login(Login),
}

#[derive(Debug, Parser)]
struct Verbosity {
    #[arg(help = "Verbose output")]
    #[arg(short)]
    v: bool,

    #[arg(help = "Very verbose output. Generally not recommended, use '-v' instead")]
    #[arg(long)]
    vv: bool,

    #[arg(help = "Quiet output. Does not print anything unless it's a error")]
    #[arg(
        long_help = "Quiet output. Does not print anything unless it's a error. Can be helpful if you pipe the output to stdout"
    )]
    #[arg(short)]
    q: bool,
}

#[derive(Debug, Parser)]
struct LoginMethod {
    #[arg(
        help = "Login with credentials (username or email and password). Must be provided as user:password"
    )]
    #[arg(long)]
    credentials: Option<String>,
    #[arg(help = "Login with the etp-rt cookie")]
    #[arg(
        long_help = "Login with the etp-rt cookie. This can be obtained when you login on crunchyroll.com and extract it from there"
    )]
    #[arg(long)]
    etp_rt: Option<String>,
    #[arg(help = "Login anonymously / without an account")]
    #[arg(long, default_value_t = false)]
    anonymous: bool,
}

pub async fn cli_entrypoint() {
    let cli: Cli = Cli::parse();

    if let Some(verbosity) = &cli.verbosity {
        if verbosity.v as u8 + verbosity.q as u8 + verbosity.vv as u8 > 1 {
            eprintln!("Output cannot be verbose ('-v') and quiet ('-q') at the same time");
            std::process::exit(1)
        } else if verbosity.v {
            CliLogger::init(false, LevelFilter::Debug).unwrap()
        } else if verbosity.q {
            CliLogger::init(false, LevelFilter::Error).unwrap()
        } else if verbosity.vv {
            CliLogger::init(true, LevelFilter::Debug).unwrap()
        }
    } else {
        CliLogger::init(false, LevelFilter::Info).unwrap()
    }

    debug!("cli input: {:?}", cli);

    let ctx = match create_ctx(&cli).await {
        Ok(ctx) => ctx,
        Err(e) => {
            error!("{}", e);
            std::process::exit(1)
        }
    };
    debug!("Created context");

    ctrlc::set_handler(move || {
        debug!("Ctrl-c detected");
        if let Ok(dir) = fs::read_dir(&env::temp_dir()) {
            for file in dir.flatten() {
                if file
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default()
                    .starts_with(".crunchy-cli_")
                {
                    let result = fs::remove_file(file.path());
                    debug!(
                        "Ctrl-c removed temporary file {} {}",
                        file.path().to_string_lossy(),
                        if result.is_ok() {
                            "successfully"
                        } else {
                            "not successfully"
                        }
                    )
                }
            }
        }
        std::process::exit(1)
    })
    .unwrap();
    debug!("Created ctrl-c handler");

    match cli.command {
        Command::Archive(archive) => execute_executor(archive, ctx).await,
        Command::Download(download) => execute_executor(download, ctx).await,
        Command::Login(login) => {
            if login.remove {
                return;
            } else {
                execute_executor(login, ctx).await
            }
        }
    };
}

/// Cannot be done in the main function. I wanted to return `dyn` [`Execute`] from the match but had to
/// box it which then conflicts with [`Execute::execute`] which consumes `self`
async fn execute_executor(mut executor: impl Execute, ctx: Context) {
    if let Err(err) = executor.pre_check() {
        error!("Misconfigurations detected: {}", err);
        std::process::exit(1)
    }

    if let Err(err) = executor.execute(ctx).await {
        error!("a unexpected error occurred: {}", err);

        if let Some(crunchy_error) = err.downcast_ref::<CrunchyrollError>() {
            let message = match crunchy_error {
                CrunchyrollError::Internal(i) => &i.message,
                CrunchyrollError::Request(r) => &r.message,
                CrunchyrollError::Decode(d) => &d.message,
                CrunchyrollError::Authentication(a) => &a.message,
                CrunchyrollError::Input(i) => &i.message,
            };
            if message.contains("content.get_video_streams_v2.cms_service_error") {
                error!("You've probably hit a rate limit. Try again later, generally after 10-20 minutes the rate limit is over and you can continue to use the cli")
            }
        }

        std::process::exit(1)
    }
}

async fn create_ctx(cli: &Cli) -> Result<Context> {
    let crunchy = crunchyroll_session(cli).await?;
    Ok(Context { crunchy })
}

async fn crunchyroll_session(cli: &Cli) -> Result<Crunchyroll> {
    let supported_langs = vec![
        Locale::ar_ME,
        Locale::de_DE,
        Locale::en_US,
        Locale::es_ES,
        Locale::es_419,
        Locale::fr_FR,
        Locale::it_IT,
        Locale::pt_BR,
        Locale::pt_PT,
        Locale::ru_RU,
    ];
    let locale = if let Some(lang) = &cli.lang {
        if !supported_langs.contains(lang) {
            bail!(
                "Via `--lang` specified language is not supported. Supported languages: {}",
                supported_langs
                    .iter()
                    .map(|l| format!("`{}` ({})", l.to_string(), l.to_human_readable()))
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        }
        lang.clone()
    } else {
        let mut lang = system_locale();
        if !supported_langs.contains(&lang) {
            warn!("Recognized system locale is not supported. Using en-US as default. Use `--lang` to overwrite the used language");
            lang = Locale::en_US
        }
        lang
    };

    let mut client_builder = CrunchyrollBuilder::predefined_client_builder();
    if let Some(proxy) = &cli.proxy {
        client_builder = client_builder.proxy(proxy.clone())
    }

    let mut builder = Crunchyroll::builder()
        .client(client_builder.build()?)
        .locale(locale)
        .stabilization_locales(cli.experimental_fixes)
        .stabilization_season_number(cli.experimental_fixes);

    if let Command::Download(download) = &cli.command {
        builder = builder.preferred_audio_locale(download.audio.clone())
    }

    let login_methods_count = cli.login_method.credentials.is_some() as u8
        + cli.login_method.etp_rt.is_some() as u8
        + cli.login_method.anonymous as u8;

    let progress_handler = progress!("Logging in");
    if login_methods_count == 0 {
        if let Some(login_file_path) = login::login_file_path() {
            if login_file_path.exists() {
                let session = fs::read_to_string(login_file_path)?;
                if let Some((token_type, token)) = session.split_once(':') {
                    match token_type {
                        "refresh_token" => {
                            return Ok(builder.login_with_refresh_token(token).await?)
                        }
                        "etp_rt" => return Ok(builder.login_with_etp_rt(token).await?),
                        _ => (),
                    }
                }
                bail!("Could not read stored session ('{}')", session)
            }
        }
        bail!("Please use a login method ('--credentials', '--etp-rt' or '--anonymous')")
    } else if login_methods_count > 1 {
        bail!("Please use only one login method ('--credentials', '--etp-rt' or '--anonymous')")
    }

    let crunchy = if let Some(credentials) = &cli.login_method.credentials {
        if let Some((user, password)) = credentials.split_once(':') {
            builder.login_with_credentials(user, password).await?
        } else {
            bail!("Invalid credentials format. Please provide your credentials as user:password")
        }
    } else if let Some(etp_rt) = &cli.login_method.etp_rt {
        builder.login_with_etp_rt(etp_rt).await?
    } else if cli.login_method.anonymous {
        builder.login_anonymously().await?
    } else {
        bail!("should never happen")
    };

    progress_handler.stop("Logged in");

    Ok(crunchy)
}

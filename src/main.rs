#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;
#[macro_use]
extern crate rocket;
#[macro_use]
extern crate rocket_contrib;

use failure::Error;
use hogan;
use hogan::config::ConfigDir;
use hogan::config::ConfigUrl;
use hogan::datadogstatsd::{CustomMetrics, DdMetrics};
use hogan::template::TemplateDir;
use lru_time_cache::LruCache;
use regex::{Regex, RegexBuilder};
use rocket::config::Config;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::Status;
use rocket::request::{self, FromRequest};
use rocket::Outcome;
use rocket::{Data, State};
use rocket::{Request, Response};
use rocket_contrib::json::{Json, JsonValue};
use rocket_lamb::RocketExt;
use serde::Serialize;
use shellexpand;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind::AlreadyExists;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;
use stderrlog;
use structopt;
use structopt::clap::AppSettings;
use structopt::StructOpt;

// static CustomMetrics: DdMetrics = DdMetrics::new();

/// Fairing for timing requests.
pub struct RequestTimer;

/// Value stored in request-local state.
#[derive(Copy, Clone)]
struct TimerStart(Option<SystemTime>);

impl Fairing for RequestTimer {
    fn info(&self) -> Info {
        Info {
            name: "Request Timer",
            kind: Kind::Request | Kind::Response,
        }
    }

    /// Stores the start time of the request in request-local state.
    fn on_request(&self, request: &mut Request, _: &Data) {
        // Store a `TimerStart` instead of directly storing a `SystemTime`
        // to ensure that this usage doesn't conflict with anything else
        // that might store a `SystemTime` in request-local cache.
        if request.uri().path() != "/ok" {
            request.local_cache(|| TimerStart(Some(SystemTime::now())));
        }
    }

    /// Adds a header to the response indicating how long the server took to
    /// process the request.
    fn on_response(&self, request: &Request, response: &mut Response) {
        info!("request uri: {}", request.uri().path());
        if request.uri().path() != "/ok" {
            let start_time = request.local_cache(|| TimerStart(None));
            if let Some(Ok(duration)) = start_time.0.map(|st| st.elapsed()) {
                let ms = duration.as_secs() * 1000 + duration.subsec_millis() as u64;
                info!("Request duration: {} ms", ms);
                let metrics = DdMetrics::new();
                metrics.gauge(
                    CustomMetrics::RequestTime.metrics_name(),
                    request.uri().path(),
                    &ms.to_string(),
                );
                response.set_raw_header("X-Response-Time", format!("{} ms", ms));
            }
        }
    }
}

/// Request guard used to retrieve the start time of a request.
#[derive(Copy, Clone)]
pub struct StartTime(pub SystemTime);

// Allows a route to access the time a request was initiated.
impl<'a, 'r> FromRequest<'a, 'r> for StartTime {
    type Error = ();

    fn from_request(request: &'a Request<'r>) -> request::Outcome<StartTime, ()> {
        match *request.local_cache(|| TimerStart(None)) {
            TimerStart(Some(time)) => Outcome::Success(StartTime(time)),
            TimerStart(None) => Outcome::Failure((Status::InternalServerError, ())),
        }
    }
}

/// Transform templates with handlebars
#[derive(StructOpt, Debug)]
#[structopt(setting = AppSettings::InferSubcommands)]
struct App {
    /// Sets the level of verbosity
    #[structopt(short = "v", long = "verbose", parse(from_occurrences), global = true)]
    verbosity: usize,

    #[structopt(subcommand)]
    cmd: AppCommand,
}

#[derive(StructOpt, Debug)]
enum AppCommand {
    /// Transform handlebars template files against config files
    #[structopt(name = "transform")]
    Transform {
        #[structopt(flatten)]
        common: AppCommon,

        /// Filter environments to render templates for
        #[structopt(
            short = "e",
            long = "environments-filter",
            parse(try_from_str = App::parse_regex),
            default_value = ".+",
            value_name = "REGEX"
        )]
        environments_regex: Regex,

        /// Template source (recursive)
        #[structopt(
            short = "t",
            long = "templates",
            parse(from_os_str),
            default_value = ".",
            value_name = "DIR"
        )]
        templates_path: PathBuf,

        /// Filter templates to transform
        #[structopt(
            short = "f",
            long = "templates-filter",
            parse(try_from_str = App::parse_regex),
            default_value = "^[^.]*(\\w+\\.)*template([-.].+)?\\.(config|ya?ml|properties)",
            value_name = "REGEX"
        )]
        templates_regex: Regex,

        /// Ignore existing config files intead of overwriting
        #[structopt(short = "i", long = "ignore-existing")]
        ignore_existing: bool,
    },
    /// Respond to HTTP requests to transform a template
    #[structopt(name = "server")]
    Server {
        #[structopt(flatten)]
        common: AppCommon,

        /// Port to serve requests on
        #[structopt(short = "p", long = "port", default_value = "80", value_name = "PORT")]
        port: u16,

        /// The address for the server to bind on
        #[structopt(
            short = "b",
            long = "address",
            default_value = "0.0.0.0",
            value_name = "ADDRESS"
        )]
        address: String,

        /// If enabled, configures the server to handle requests as a lambda behind an API Gateway Proxy
        /// See: https://github.com/GREsau/rocket-lamb
        #[structopt(long = "lambda")]
        lambda: bool,

        /// Set the size of the SHA LRU cache
        #[structopt(long = "cache", default_value = "100", value_name = "CACHE_SIZE")]
        cache_size: usize,

        /// Filter environments to render templates for
        #[structopt(
            short = "e",
            long = "environments-filter",
            parse(try_from_str = App::parse_regex),
            default_value = ".+",
            value_name = "REGEX"
        )]
        environments_regex: Regex,

        /// If datadog monitoring is enabled
        #[structopt(short = "d", long = "datadog")]
        datadog: bool,
    },
}

#[derive(StructOpt, Debug)]
struct AppCommon {
    /// Config source. Accepts file and git URLs. Paths within a git repository may be appended
    /// to a git URL, and branches may be specified as a URL fragment (recursive if applicable)
    #[structopt(short = "c", long = "configs", value_name = "URL")]
    configs_url: ConfigUrl,

    /// SSH key to use if configs URL requires authentication
    #[structopt(
        short = "k",
        long = "ssh-key",
        parse(from_str = App::parse_path_buf),
        default_value = "~/.ssh/id_rsa",
        value_name = "FILE"
    )]
    ssh_key: PathBuf,

    /// Throw errors if values do not exist in configs
    #[structopt(short = "s", long = "strict")]
    strict: bool,
}

impl App {
    fn config_regex(environment: &Regex) -> Result<Regex, Error> {
        App::parse_regex(&format!("config\\.{}\\.json$", environment))
    }

    fn parse_regex(src: &str) -> Result<Regex, Error> {
        RegexBuilder::new(src)
            .case_insensitive(true)
            .build()
            .map_err(|e| e.into())
    }

    fn parse_path_buf(src: &str) -> PathBuf {
        PathBuf::from(shellexpand::tilde(src).into_owned())
    }
}
fn main() -> Result<(), Error> {
    let opt = App::from_args();

    stderrlog::new()
        .module(module_path!())
        .verbosity(opt.verbosity + 2)
        .init()?;

    match opt.cmd {
        AppCommand::Transform {
            templates_path,
            environments_regex,
            templates_regex,
            common,
            ignore_existing,
        } => {
            let handlebars = hogan::transform::handlebars(common.strict);

            let template_dir = TemplateDir::new(templates_path)?;
            let mut templates = template_dir.find(templates_regex);
            println!("Loaded {} template file(s)", templates.len());

            let config_dir = ConfigDir::new(common.configs_url, &common.ssh_key)?;
            let environments = config_dir.find(App::config_regex(&environments_regex)?);
            println!("Loaded {} config file(s)", environments.len());

            for environment in environments {
                println!("Updating templates for {}", environment.environment);

                for template in &mut templates {
                    debug!("Transforming {:?}", template.path);

                    let rendered = template.render(&handlebars, &environment)?;
                    trace!("Rendered: {:?}", rendered.contents);

                    if ignore_existing {
                        if let Err(e) = match OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&rendered.path)
                        {
                            Ok(ref mut f) => f.write_all(&rendered.contents),
                            Err(ref e) if e.kind() == AlreadyExists => {
                                println!("Skipping {:?} - config already exists.", rendered.path);
                                trace!("Skipping {:?} - config already exists.", rendered.path);
                                Ok(())
                            }
                            Err(e) => Err(e),
                        } {
                            bail!("Error transforming {:?} due to {}", rendered.path, e)
                        }
                    } else if let Err(e) =
                        File::create(&rendered.path)?.write_all(&rendered.contents)
                    {
                        bail!("Error transforming {:?} due to {}", rendered.path, e)
                    }
                }
            }
        }
        AppCommand::Server {
            common,
            port,
            address,
            cache_size,
            lambda,
            environments_regex,
            datadog,
        } => {
            let config_dir = ConfigDir::new(common.configs_url, &common.ssh_key)?;

            let environments = Mutex::new(
                LruCache::<String, Arc<hogan::config::Environment>>::with_capacity(cache_size),
            );

            let environment_listings = Mutex::new(
                LruCache::<String, Arc<Vec<EnvDescription>>>::with_capacity(cache_size),
            );

            let config_dir = Mutex::new(config_dir);

            info!("Starting server on {}:{}", address, port);
            info!("datadog monitoring is setting: {}", datadog);
            let dd_metrics = if datadog {
                Some(DdMetrics::new())
            } else {
                None
            };
            let state = ServerState {
                environments,
                environment_listings,
                config_dir,
                environments_regex,
                strict: common.strict,
                dd_metrics,
            };
            start_server(address, port, lambda, state, datadog)?;
        }
    }

    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct EnvDescription {
    name: String,
    #[serde(rename(serialize = "Type"))]
    env_type: Option<String>,
}

impl From<&hogan::config::Environment> for EnvDescription {
    fn from(env: &hogan::config::Environment) -> EnvDescription {
        EnvDescription {
            name: env.environment.clone(),
            env_type: env.environment_type.clone(),
        }
    }
}

type EnvCache = Mutex<LruCache<String, Arc<hogan::config::Environment>>>;
type EnvListingCache = Mutex<LruCache<String, Arc<Vec<EnvDescription>>>>;

struct ServerState {
    environments: EnvCache,
    environment_listings: EnvListingCache,
    config_dir: Mutex<hogan::config::ConfigDir>,
    environments_regex: Regex,
    strict: bool,
    dd_metrics: Option<DdMetrics>,
}

fn start_server(
    address: String,
    port: u16,
    lambda: bool,
    state: ServerState,
    dd_enabled: bool,
) -> Result<(), Error> {
    let mut config = Config::development();
    config.set_port(port);
    config.set_address(address)?;
    let routes = routes![
        health_check,
        get_envs,
        get_config_by_env,
        transform_env,
        transform_all_envs,
        get_branch_sha,
    ];
    let server = if dd_enabled {
        rocket::custom(config)
            .mount("/", routes)
            .attach(RequestTimer)
    } else {
        rocket::custom(config).mount("/", routes)
    }
    .manage(state);
    if lambda {
        server.lambda().launch();
    } else {
        server.launch();
    }
    Ok(())
}

#[get("/ok")]
fn health_check() -> Status {
    Status::Ok
}

#[post("/transform/<sha>/<env>", data = "<body>")]
fn transform_env(
    body: Data,
    sha: String,
    env: String,
    state: State<ServerState>,
) -> Result<String, Status> {
    let sha = format_sha(&sha);
    let uri = format!("/transform/{}/{}", &sha, &env);
    match get_env(
        &state.environments,
        &state.config_dir,
        None,
        sha,
        &env,
        &state.environments_regex,
        &uri,
        state.dd_metrics.as_ref(),
    ) {
        Some(env) => {
            let handlebars = hogan::transform::handlebars(state.strict);
            let mut data = String::new();
            body.open().read_to_string(&mut data).map_err(|e| {
                warn!("Unable to consume transform body: {:?}", e);
                Status::InternalServerError
            })?;
            handlebars
                .render_template(&data, &env.config_data)
                .map_err(|_| Status::BadRequest)
        }
        None => Err(Status::NotFound),
    }
}

#[post("/transform/<sha>?<filename>", data = "<body>")]
fn transform_all_envs(
    sha: String,
    filename: String,
    body: Data,
    state: State<ServerState>,
) -> Result<Vec<u8>, Status> {
    Err(Status::Gone)
}

#[get("/envs/<sha>")]
fn get_envs(sha: String, state: State<ServerState>) -> Result<JsonValue, Status> {
    let uri = format!("/envs/{}", &sha);
    info!("uri: {}", uri);

    match get_env_listing(
        &state.environment_listings,
        &state.config_dir,
        None,
        &sha,
        &state.environments_regex,
        &uri,
        state.dd_metrics.as_ref(),
    ) {
        Some(envs) => Ok(json!(envs)),
        None => Err(Status::NotFound),
    }
}

#[get("/config/<sha>/<env>")]
fn get_config_by_env(
    sha: String,
    env: String,
    state: State<ServerState>,
) -> Result<JsonValue, Status> {
    let sha = format_sha(&sha);
    let uri = format!("/config/{}/{}", &sha, &env);
    match get_env(
        &state.environments,
        &state.config_dir,
        None,
        sha,
        &env,
        &state.environments_regex,
        &uri,
        state.dd_metrics.as_ref(),
    ) {
        Some(env) => Ok(json!(env)),
        None => Err(Status::NotFound),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShaResponse {
    head_sha: String,
    branch_name: String,
}

#[get("/heads/<branch_name>?<remote_name>")]
fn get_branch_sha(
    remote_name: Option<String>,
    branch_name: String,
    state: State<ServerState>,
) -> Result<Json<ShaResponse>, Status> {
    if let Ok(config_dir) = state.config_dir.lock() {
        if let Some(head_sha) = config_dir.find_branch_head(
            &remote_name.unwrap_or_else(|| String::from("origin")),
            &branch_name,
        ) {
            Ok(Json(ShaResponse {
                head_sha,
                branch_name,
            }))
        } else {
            Err(Status::NotFound)
        }
    } else {
        warn!("Error locking git repo");
        Err(Status::InternalServerError)
    }
}

fn format_key(sha: &str, env: &str) -> String {
    format!("{}::{}", sha, env)
}

fn get_env(
    cache: &EnvCache,
    repo: &Mutex<hogan::config::ConfigDir>,
    remote: Option<&str>,
    sha: &str,
    env: &str,
    environments_regex: &Regex,
    request_url: &str,
    dd_metrics: Option<&DdMetrics>,
) -> Option<Arc<hogan::config::Environment>> {
    let key = format_key(sha, env);
    let mut cache = match cache.lock() {
        Ok(cache) => cache,
        Err(e) => {
            warn!("Unable to lock cache {}", e);
            return None;
        }
    };
    if let Some(env) = cache.get(&key) {
        info!("Cache Hit {}", key);
        if let Some(custom_metrics) = dd_metrics {
            custom_metrics.incr(CustomMetrics::CacheHit.metrics_name(), request_url);
        }
        Some(env.clone())
    } else {
        info!("Cache Miss {}", key);
        if let Some(custom_metrics) = dd_metrics {
            custom_metrics.incr(CustomMetrics::CacheMiss.metrics_name(), request_url);
        }
        match repo.lock() {
            Ok(repo) => {
                if let Some(sha) = repo.refresh(remote, Some(sha)) {
                    match repo
                        .find(environments_regex.clone())
                        .iter()
                        .find(|e| e.environment == env)
                    {
                        Some(env) => cache.insert(key.clone(), Arc::new(env.clone())),
                        None => {
                            debug!("Unable to find the env {} in {}", env, sha);
                            return None;
                        }
                    };
                };
            }
            Err(e) => {
                warn!("Unable to lock repository {}", e);
                return None;
            }
        };
        if let Some(envs) = cache.get(&key) {
            Some(envs.clone())
        } else {
            info!("Unable to find the configuration sha {}", sha);
            None
        }
    }
}

fn get_env_listing(
    cache: &EnvListingCache,
    repo: &Mutex<hogan::config::ConfigDir>,
    remote: Option<&str>,
    sha: &str,
    environments_regex: &Regex,
    request_url: &str,
    dd_metrics: Option<&DdMetrics>,
) -> Option<Arc<Vec<EnvDescription>>> {
    let sha = format_sha(sha);
    let mut cache = match cache.lock() {
        Ok(cache) => cache,
        Err(e) => {
            warn!("Unable to lock cache {}", e);
            return None;
        }
    };
    if let Some(env) = cache.get(sha) {
        info!("Cache Hit {}", sha);
        if let Some(custom_metrics) = dd_metrics {
            custom_metrics.incr(CustomMetrics::CacheHit.metrics_name(), request_url);
        }
        Some(env.clone())
    } else {
        info!("Cache Miss {}", sha);
        if let Some(custom_metrics) = dd_metrics {
            custom_metrics.incr(CustomMetrics::CacheMiss.metrics_name(), request_url);
        }
        match repo.lock() {
            Ok(repo) => {
                if let Some(sha) = repo.refresh(remote, Some(sha)) {
                    let envs = format_envs(&repo.find(environments_regex.clone()));
                    if !envs.is_empty() {
                        info!("Loading envs for {}", sha);
                        cache.insert(sha, Arc::new(envs));
                    } else {
                        info!("No envs found for {}", sha);
                        return None;
                    }
                };
            }
            Err(e) => {
                warn!("Unable to lock repository {}", e);
                return None;
            }
        };
        if let Some(envs) = cache.get(sha) {
            Some(envs.clone())
        } else {
            info!("Unable to find the configuration sha {}", sha);
            None
        }
    }
}

fn format_envs(envs: &[hogan::config::Environment]) -> Vec<EnvDescription> {
    envs.iter().map(|e| e.into()).collect()
}

fn format_sha(sha: &str) -> &str {
    if sha.len() >= 7 {
        &sha[0..7]
    } else {
        sha
    }
}

#[cfg(test)]
mod tests {
    use assert_cmd;
    use dir_diff;
    use fs_extra;
    use predicates;
    use tempfile;

    use self::assert_cmd::prelude::*;
    use self::fs_extra::dir;
    use self::predicates::prelude::*;
    use std::io::Write;
    use std::path::Path;
    use std::process::Command;

    #[cfg(not(all(target_env = "msvc", target_arch = "x86_64")))]
    #[test]
    fn test_transform() {
        let temp_dir = tempfile::tempdir().unwrap();

        fs_extra::copy_items(
            &vec!["tests/fixtures/projects/templates"],
            temp_dir.path(),
            &dir::CopyOptions::new(),
        )
        .unwrap();

        let templates_path = temp_dir.path().join("templates");

        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();

        let cmd = cmd.args(&[
            "transform",
            "--configs",
            "tests/fixtures/configs",
            "--templates",
            templates_path.to_str().unwrap(),
        ]);

        cmd.assert().success();

        cmd.assert().stdout(
            predicate::str::contains(format!(r#"Finding Files: {:?}"#, templates_path)).from_utf8(),
        );

        cmd.assert().stdout(
            predicate::str::contains(
                r"regex: /^[^.]*(\w+\.)*template([-.].+)?\.(config|ya?ml|properties)/",
            )
            .from_utf8(),
        );

        cmd.assert()
            .stdout(predicate::str::contains("Loaded 6 template file(s)").from_utf8());

        cmd.assert().stdout(
            predicate::str::contains(r#"Finding Files: "tests/fixtures/configs""#).from_utf8(),
        );

        cmd.assert()
            .stdout(predicate::str::contains(r#"regex: /config\..+\.json$/"#).from_utf8());

        cmd.assert()
            .stdout(predicate::str::contains("Loaded 4 config file(s)").from_utf8());

        for environment in ["EMPTY", "ENVTYPE", "TEST", "TEST2"].iter() {
            cmd.assert().stdout(
                predicate::str::contains(format!("Updating templates for {}", environment))
                    .from_utf8(),
            );
        }

        assert!(!dir_diff::is_different(
            &templates_path.join("project-1"),
            &Path::new("tests/fixtures/projects/rendered/project-1")
        )
        .unwrap());

        assert!(!dir_diff::is_different(
            &templates_path.join("project-2"),
            &Path::new("tests/fixtures/projects/rendered/project-2")
        )
        .unwrap());
    }

    #[cfg(not(all(target_env = "msvc", target_arch = "x86_64")))]
    #[test]
    fn test_ignore_existing() {
        let temp_dir = tempfile::tempdir().unwrap();

        fs_extra::copy_items(
            &vec!["tests/fixtures/projects/templates"],
            temp_dir.path(),
            &dir::CopyOptions::new(),
        )
        .unwrap();

        let templates_path = temp_dir.path().join("templates");

        let ignore_path = templates_path.join("project-1/Web.EMPTY.config");
        if let Ok(ref mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&ignore_path)
        {
            f.write_all(b"Hamburger.")
                .expect("Failed to create test file for ignore.")
        }

        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
        let cmd = cmd.args(&[
            "transform",
            "--configs",
            "tests/fixtures/configs",
            "--templates",
            templates_path.to_str().unwrap(),
            "-i",
        ]);

        cmd.assert().success();

        // assert that running the command with the ignore flag
        // did not overwrite the manually created project-1/Web.EMPTY.config
        let data2 =
            std::fs::read_to_string(&ignore_path).expect("Failed to read test file for ignore.");
        assert!(data2 == "Hamburger.");

        // after running the command again without the ignore flag
        // assert that the configs now match those in the rendered directory
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
        let cmd = cmd.args(&[
            "transform",
            "--configs",
            "tests/fixtures/configs",
            "--templates",
            templates_path.to_str().unwrap(),
        ]);
        cmd.assert().success();

        assert!(!dir_diff::is_different(
            &templates_path.join("project-1"),
            &Path::new("tests/fixtures/projects/rendered/project-1")
        )
        .unwrap());

        assert!(!dir_diff::is_different(
            &templates_path.join("project-2"),
            &Path::new("tests/fixtures/projects/rendered/project-2")
        )
        .unwrap());
    }
}

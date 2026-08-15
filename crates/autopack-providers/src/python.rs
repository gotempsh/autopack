//! Python provider: uv, Poetry, Pipenv and pip projects.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{normalize_version_range, procfile_web_command, read_version_file};

/// Python version used when the project does not pin one.
const DEFAULT_PYTHON_VERSION: &str = "3.12";

/// Packages required when mise cannot use a precompiled Python artifact and
/// falls back to python-build. Keep these provider-specific: runtimes such as
/// Node and Go do not need a C toolchain in their packages layer.
const CPYTHON_BUILD_PACKAGES: &[&str] = &[
    "build-essential",
    "libbz2-dev",
    "libffi-dev",
    "liblzma-dev",
    "libncursesw5-dev",
    "libreadline-dev",
    "libsqlite3-dev",
    "libssl-dev",
    "xz-utils",
    "zlib1g-dev",
];

/// Shared libraries used by the core extension modules produced by the source
/// build above. The development packages stay in the builder; only these
/// runtime libraries reach the deployed image.
const CPYTHON_RUNTIME_PACKAGES: &[&str] = &[
    "libbz2-1.0",
    "libffi8",
    "liblzma5",
    "libncursesw6",
    "libreadline8",
    "libsqlite3-0",
    "libssl3",
    "zlib1g",
];

/// Virtualenv every install goes into, so dependencies live under `/app`.
const VENV: &str = "/app/.venv";

/// The virtualenv's executable directory, prepended to `PATH`.
const VENV_BIN: &str = "/app/.venv/bin";

/// Builds Python applications.
pub struct PythonProvider;

/// How dependencies are declared and installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Installer {
    Uv,
    Poetry,
    Pipenv,
    Pip,
}

impl Installer {
    fn id(self) -> &'static str {
        match self {
            Self::Uv => "uv",
            Self::Poetry => "poetry",
            Self::Pipenv => "pipenv",
            Self::Pip => "pip",
        }
    }

    /// Detect the installer from lockfiles, then from manifests.
    fn detect(app: &App, pyproject: &str) -> Self {
        if app.has_file("uv.lock") || pyproject.contains("[tool.uv") {
            return Self::Uv;
        }
        if app.has_file("poetry.lock") || pyproject.contains("[tool.poetry") {
            return Self::Poetry;
        }
        if app.has_any_file(["Pipfile.lock", "Pipfile"]) {
            return Self::Pipenv;
        }
        Self::Pip
    }

    /// The mise tool that provides the installer, if it is not part of Python.
    fn mise_tool(self) -> Option<&'static str> {
        match self {
            Self::Uv => Some("uv"),
            Self::Poetry => Some("poetry"),
            // pipenv has no mise core plugin; it is installed with pip below.
            Self::Pipenv | Self::Pip => None,
        }
    }

    /// Commands that must run before the virtualenv is on `PATH`.
    ///
    /// uv, Poetry and Pipenv create `/app/.venv` themselves. Plain pip does
    /// not, and installing into the interpreter's own site-packages puts the
    /// dependencies under `/mise` — a directory the runtime image takes from
    /// the packages step, so they would silently not be in the final image.
    fn bootstrap_commands(self) -> Vec<String> {
        match self {
            Self::Pip => vec![format!("python -m venv {VENV}")],
            Self::Uv | Self::Poetry | Self::Pipenv => Vec::new(),
        }
    }

    /// Files that must be present for the install command to run.
    fn manifest_files(self) -> &'static [&'static str] {
        match self {
            Self::Uv => &["pyproject.toml", "uv.lock", "README.md"],
            Self::Poetry => &["pyproject.toml", "poetry.lock", "README.md"],
            Self::Pipenv => &["Pipfile", "Pipfile.lock"],
            Self::Pip => &["requirements.txt", "constraints.txt"],
        }
    }

    fn install_commands(self, app: &App) -> Vec<String> {
        match self {
            Self::Uv => {
                let frozen = if app.has_file("uv.lock") {
                    " --frozen"
                } else {
                    ""
                };
                vec![format!("uv sync{frozen} --no-dev --no-install-project")]
            }
            Self::Poetry => vec![
                "poetry config virtualenvs.in-project true".to_string(),
                "poetry install --no-root --only main".to_string(),
            ],
            Self::Pipenv => vec![
                "pip install --no-cache-dir pipenv".to_string(),
                "PIPENV_VENV_IN_PROJECT=1 pipenv install --deploy".to_string(),
            ],
            Self::Pip => {
                if app.has_file("requirements.txt") {
                    vec!["pip install -r requirements.txt".to_string()]
                } else {
                    vec!["pip install .".to_string()]
                }
            }
        }
    }
}

impl Provider for PythonProvider {
    fn id(&self) -> &'static str {
        "python"
    }

    fn display_name(&self) -> &'static str {
        "Python"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_any_file([
            "requirements.txt",
            "pyproject.toml",
            "Pipfile",
            "setup.py",
            "manage.py",
            "main.py",
        ]) || app.has_match("**/*.py"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let pyproject = ctx.app.read_file_opt("pyproject.toml")?.unwrap_or_default();
        let installer = Installer::detect(ctx.app, &pyproject);

        let (version, source) = python_version(ctx.app, &pyproject)?;
        ctx.packages.add("python", &version, source);
        ctx.build_apt_packages.extend(
            CPYTHON_BUILD_PACKAGES
                .iter()
                .map(|package| package.to_string()),
        );
        ctx.deploy_apt_packages.extend(
            CPYTHON_RUNTIME_PACKAGES
                .iter()
                .map(|package| package.to_string()),
        );
        if let Some(tool) = installer.mise_tool() {
            ctx.packages.add(tool, "latest", "installer");
        }

        ctx.add_metadata("pythonVersion", &version);
        ctx.add_metadata("installer", installer.id());

        // Packages such as psycopg2 link against a system library. Without the
        // runtime package the build still succeeds and the container dies on
        // the first import.
        let declared = declared_dependencies(ctx.app)?;
        let (build_packages, runtime_packages) =
            crate::native::required_packages(&declared, crate::native::PYTHON);
        if !build_packages.is_empty() || !runtime_packages.is_empty() {
            ctx.add_metadata(
                "systemPackages",
                format!(
                    "build: [{}], runtime: [{}]",
                    build_packages.join(" "),
                    runtime_packages.join(" ")
                ),
            );
        }
        ctx.build_apt_packages.extend(build_packages);
        ctx.deploy_apt_packages.extend(runtime_packages);

        self.plan_install(ctx, installer)?;

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        // Unbuffered output, or logs vanish when the container is killed.
        ctx.add_deploy_variable("PYTHONUNBUFFERED", "1");
        ctx.add_deploy_variable("PYTHONDONTWRITEBYTECODE", "1");
        ctx.add_deploy_path(VENV_BIN);

        if let Some(command) = self.start_command(ctx, installer)? {
            ctx.set_start_command(command);
        }
        Ok(())
    }
}

impl PythonProvider {
    fn plan_install(&self, ctx: &mut BuildContext<'_>, installer: Installer) -> Result<()> {
        let pip_cache = ctx.shared_cache("pip", "/cache/pip");
        let uv_cache = ctx.shared_cache("uv", "/cache/uv");

        let manifests: Vec<&str> = installer
            .manifest_files()
            .iter()
            .copied()
            .filter(|file| ctx.app.has_file(file))
            .collect();
        ctx.add_metadata("installContext", manifests.join(", "));

        let commands = installer.install_commands(ctx.app);

        let bootstrap = installer.bootstrap_commands();

        let step = ctx.step(steps::INSTALL);
        step.add_input(Layer::local().including(manifests));
        step.add_variable("PIP_CACHE_DIR", "/cache/pip");
        step.add_variable("PIP_DISABLE_PIP_VERSION_CHECK", "1");
        step.add_variable("UV_CACHE_DIR", "/cache/uv");
        step.add_variable("UV_PROJECT_ENVIRONMENT", VENV);
        // Makes `uv pip install` and `pip install` target the project venv.
        step.add_variable("VIRTUAL_ENV", VENV);
        step.add_cache(pip_cache);
        step.add_cache(uv_cache);

        for command in bootstrap {
            step.add_command(Command::shell(command));
        }
        // Added before the installs so anything appended later — such as a
        // missing WSGI server — also lands in the venv.
        step.add_command(Command::path(VENV_BIN));
        for command in commands {
            step.add_command(Command::shell(command));
        }
        Ok(())
    }

    fn start_command(
        &self,
        ctx: &mut BuildContext<'_>,
        installer: Installer,
    ) -> Result<Option<String>> {
        if let Some(command) = procfile_web_command(ctx.app)? {
            return Ok(Some(command));
        }

        let dependencies = declared_dependencies(ctx.app)?;
        let has_gunicorn = dependencies.contains("gunicorn");
        let has_uvicorn = dependencies.contains("uvicorn");

        if let Some(module) = django_wsgi_module(ctx.app)? {
            ctx.add_metadata("framework", "django");
            if !has_gunicorn {
                // Django's own `runserver` is a development server and must not
                // reach production, so a WSGI server is added to the build.
                add_server_package(ctx, installer, "gunicorn");
                ctx.add_metadata("addedDependency", "gunicorn (no WSGI server declared)");
            }
            return Ok(Some(format!(
                "gunicorn {module}:application --bind 0.0.0.0:${{PORT:-8000}}"
            )));
        }

        if let Some((module, kind)) = asgi_or_wsgi_entrypoint(ctx.app)? {
            ctx.add_metadata("framework", kind.framework());
            return Ok(Some(match kind {
                EntryKind::Asgi => {
                    if !has_uvicorn {
                        add_server_package(ctx, installer, "uvicorn");
                        ctx.add_metadata("addedDependency", "uvicorn (no ASGI server declared)");
                    }
                    format!("uvicorn {module}:app --host 0.0.0.0 --port ${{PORT:-8000}}")
                }
                EntryKind::Wsgi => {
                    if !has_gunicorn {
                        add_server_package(ctx, installer, "gunicorn");
                        ctx.add_metadata("addedDependency", "gunicorn (no WSGI server declared)");
                    }
                    format!("gunicorn {module}:app --bind 0.0.0.0:${{PORT:-8000}}")
                }
            }));
        }

        for entry in ["main.py", "app.py", "server.py", "bot.py"] {
            if ctx.app.has_file(entry) {
                return Ok(Some(format!("python {entry}")));
            }
        }

        Ok(None)
    }
}

/// Add a server package the project did not declare.
fn add_server_package(ctx: &mut BuildContext<'_>, installer: Installer, package: &str) {
    let command = match installer {
        Installer::Uv => format!("uv pip install {package}"),
        Installer::Poetry | Installer::Pipenv | Installer::Pip => {
            format!("pip install --no-cache-dir {package}")
        }
    };
    ctx.step(steps::INSTALL)
        .add_command(Command::shell(command));
}

/// Which WSGI/ASGI convention an entry point follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Asgi,
    Wsgi,
}

impl EntryKind {
    fn framework(self) -> &'static str {
        match self {
            Self::Asgi => "fastapi",
            Self::Wsgi => "flask",
        }
    }
}

/// Find a FastAPI or Flask app object in the usual entry point files.
fn asgi_or_wsgi_entrypoint(app: &App) -> Result<Option<(String, EntryKind)>> {
    for file in ["main.py", "app.py", "asgi.py", "wsgi.py", "server.py"] {
        let Some(contents) = app.read_file_opt(file)? else {
            continue;
        };
        let module = file.trim_end_matches(".py").to_string();

        if contents.contains("FastAPI(") || contents.contains("Starlette(") {
            return Ok(Some((module, EntryKind::Asgi)));
        }
        if contents.contains("Flask(") {
            return Ok(Some((module, EntryKind::Wsgi)));
        }
    }
    Ok(None)
}

/// The dotted module path of a Django project's `wsgi.py`.
fn django_wsgi_module(app: &App) -> Result<Option<String>> {
    if !app.has_file("manage.py") {
        return Ok(None);
    }

    let mut candidates = app.find_files("*/wsgi.py")?;
    if candidates.is_empty() {
        candidates = app.find_files("**/wsgi.py")?;
    }

    Ok(candidates
        .first()
        .map(|path| path.trim_end_matches(".py").replace('/', ".")))
}

/// Raw text of every dependency declaration, for substring checks.
fn declared_dependencies(app: &App) -> Result<String> {
    let mut combined = String::new();
    for file in ["requirements.txt", "pyproject.toml", "Pipfile", "setup.py"] {
        if let Some(contents) = app.read_file_opt(file)? {
            combined.push_str(&contents);
            combined.push('\n');
        }
    }
    Ok(combined)
}

/// The Python version to install, and where it came from.
fn python_version(app: &App, pyproject: &str) -> Result<(String, String)> {
    if let Some(version) = read_version_file(app, ".python-version")? {
        if let Some(version) = normalize_version_range(&version) {
            return Ok((version, ".python-version".into()));
        }
    }

    // Heroku-style `runtime.txt` contains `python-3.12.2`.
    if let Some(contents) = read_version_file(app, "runtime.txt")? {
        if let Some(version) = contents.strip_prefix("python-") {
            if let Some(version) = normalize_version_range(version) {
                return Ok((version, "runtime.txt".into()));
            }
        }
    }

    if let Some(line) = pyproject
        .lines()
        .find(|line| line.trim_start().starts_with("requires-python"))
    {
        if let Some((_, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if let Some(version) = normalize_version_range(value) {
                return Ok((version, "pyproject.toml requires-python".into()));
            }
        }
    }

    Ok((
        DEFAULT_PYTHON_VERSION.to_string(),
        "autopack default".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn pip_projects_install_from_requirements() {
        let (_dir, app) = write_app(&[
            ("requirements.txt", "flask==3.0.0\ngunicorn==22.0.0\n"),
            ("app.py", "from flask import Flask\napp = Flask(__name__)\n"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "python");
        assert_eq!(analysis.metadata["installer"], "pip");
        // pip has no venv of its own, so autopack creates one under /app —
        // otherwise the packages land in /mise and never reach the image.
        let commands: Vec<String> = analysis
            .plan
            .step("install")
            .unwrap()
            .commands
            .iter()
            .map(|command| command.display_name())
            .collect();
        assert_eq!(
            commands,
            vec![
                "python -m venv /app/.venv".to_string(),
                "PATH += /app/.venv/bin".to_string(),
                "pip install -r requirements.txt".to_string(),
            ]
        );
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("gunicorn app:app --bind 0.0.0.0:${PORT:-8000}")
        );
    }

    #[test]
    fn python_source_build_has_compiler_and_runtime_libraries() {
        let (_dir, app) = write_app(&[("main.py", "print('hi')\n")]);
        let analysis = plan_for(&app);

        let package_commands: Vec<String> = analysis
            .plan
            .step(steps::PACKAGES)
            .expect("Python must have a packages step")
            .commands
            .iter()
            .map(|command| command.display_name())
            .collect();
        let apt_install = package_commands
            .first()
            .expect("system packages must be installed before mise");
        for package in CPYTHON_BUILD_PACKAGES {
            assert!(
                apt_install.contains(package),
                "Python source-build package `{package}` is missing: {apt_install}"
            );
        }
        assert!(
            package_commands
                .iter()
                .position(|command| command.contains("mise install"))
                .is_some_and(|mise| mise > 0),
            "mise must run after the source-build dependencies: {package_commands:?}"
        );

        let runtime_commands: Vec<String> = analysis
            .plan
            .step(steps::RUNTIME)
            .expect("Python must have a runtime step")
            .commands
            .iter()
            .map(|command| command.display_name())
            .collect();
        let runtime_apt = runtime_commands
            .first()
            .expect("Python runtime libraries must be installed");
        for package in CPYTHON_RUNTIME_PACKAGES {
            assert!(
                runtime_apt.contains(package),
                "Python runtime package `{package}` is missing: {runtime_apt}"
            );
        }
        assert!(!runtime_apt.contains("build-essential"), "{runtime_apt}");
    }

    #[test]
    fn uv_lock_selects_uv_and_the_venv_path() {
        let (_dir, app) = write_app(&[
            (
                "pyproject.toml",
                "[project]\nname = \"api\"\nrequires-python = \">=3.12\"\n",
            ),
            ("uv.lock", ""),
            ("main.py", "from fastapi import FastAPI\napp = FastAPI()\n"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["installer"], "uv");
        assert_eq!(analysis.metadata["pythonVersion"], "3.12");
        assert!(analysis.plan.deploy.paths.contains(&VENV_BIN.to_string()));
        assert!(analysis
            .plan
            .deploy
            .start_command
            .as_deref()
            .unwrap()
            .starts_with("uvicorn main:app"));
    }

    #[test]
    fn django_gets_a_wsgi_server_added() {
        let (_dir, app) = write_app(&[
            ("requirements.txt", "django==5.0\n"),
            ("manage.py", "#!/usr/bin/env python"),
            ("mysite/wsgi.py", "application = None"),
            ("mysite/__init__.py", ""),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["framework"], "django");
        assert!(analysis.metadata["addedDependency"].contains("gunicorn"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("gunicorn mysite.wsgi:application --bind 0.0.0.0:${PORT:-8000}")
        );
    }

    #[test]
    fn declared_servers_are_not_reinstalled() {
        let (_dir, app) = write_app(&[
            ("requirements.txt", "django==5.0\ngunicorn==22.0\n"),
            ("manage.py", ""),
            ("mysite/wsgi.py", "application = None"),
        ]);
        let analysis = plan_for(&app);
        assert!(!analysis.metadata.contains_key("addedDependency"));
    }

    #[test]
    fn poetry_lock_selects_poetry() {
        let (_dir, app) = write_app(&[
            ("pyproject.toml", "[tool.poetry]\nname = \"api\"\n"),
            ("poetry.lock", ""),
            ("main.py", "print('hi')"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["installer"], "poetry");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("python main.py")
        );
    }
}

use std::fmt::Write;

use anyhow::Result;
use itertools::Itertools;
use owo_colors::OwoColorize;
use tracing::debug;

use distribution_types::{IndexLocations, Resolution};
use install_wheel_rs::linker::LinkMode;
use uv_cache::Cache;
use uv_client::{BaseClientBuilder, Connectivity, RegistryClientBuilder};
use uv_configuration::{
    Concurrency, ConfigSettings, ExtrasSpecification, NoBinary, NoBuild, PreviewMode, Reinstall,
    SetupPyStrategy, Upgrade,
};
use uv_dispatch::BuildDispatch;
use uv_distribution::ProjectWorkspace;
use uv_fs::Simplified;
use uv_installer::{SatisfiesResult, SitePackages};
use uv_interpreter::{InterpreterRequest, PythonEnvironment, SystemPython};
use uv_requirements::{RequirementsSource, RequirementsSpecification};
use uv_resolver::{FlatIndex, InMemoryIndex, Options};
use uv_types::{BuildIsolation, HashStrategy, InFlight};

use crate::commands::pip;
use crate::printer::Printer;

pub(crate) mod lock;
pub(crate) mod run;
pub(crate) mod sync;

#[derive(thiserror::Error, Debug)]
pub(crate) enum ProjectError {
    #[error(transparent)]
    Interpreter(#[from] uv_interpreter::Error),

    #[error(transparent)]
    Virtualenv(#[from] uv_virtualenv::Error),

    #[error(transparent)]
    Tags(#[from] platform_tags::TagsError),

    #[error(transparent)]
    Lock(#[from] uv_resolver::LockError),

    #[error(transparent)]
    Fmt(#[from] std::fmt::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Serialize(#[from] toml::ser::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),

    #[error(transparent)]
    Operation(#[from] pip::operations::Error),
}

/// Initialize a virtual environment for the current project.
pub(crate) fn init_environment(
    project: &ProjectWorkspace,
    python: Option<&str>,
    preview: PreviewMode,
    cache: &Cache,
    printer: Printer,
) -> Result<PythonEnvironment, ProjectError> {
    let venv = project.workspace().root().join(".venv");

    let requires_python = project
        .current_project()
        .pyproject_toml()
        .project
        .as_ref()
        .and_then(|project| project.requires_python.as_ref());

    // Discover or create the virtual environment.
    match PythonEnvironment::from_root(&venv, cache) {
        Ok(venv) => {
            // `--python` has highest precedence, after that we check `requires_python` from
            // `pyproject.toml`. If `--python` and `requires_python` are mutually incompatible,
            // we'll fail at the build or at last the install step when we aren't able to install
            // the editable wheel for the current project into the venv.
            // TODO(konsti): Do we want to support a workspace python version requirement?

            let venv_python_satisfactory = if let Some(python) = python {
                InterpreterRequest::parse(python).satisfied(venv.interpreter())
            } else if let Some(requires_python) = requires_python {
                requires_python.contains(venv.interpreter().python_version())
            } else {
                true
            };

            if venv_python_satisfactory {
                return Ok(venv);
            }

            writeln!(
                printer.stderr(),
                "Removing virtualenv at: {}",
                venv.root().user_display().cyan()
            )?;
            fs_err::remove_dir_all(venv.root())?;
        }
        Err(uv_interpreter::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }

    // TODO(konsti): If `--python` is unset, respect `Requires-Python`. This requires extending
    //   `VersionRequest` to support `VersionSpecifiers`.
    let interpreter = if let Some(python) = python.as_ref() {
        PythonEnvironment::from_requested_python(python, SystemPython::Allowed, preview, cache)?
            .into_interpreter()
    } else {
        PythonEnvironment::from_default_python(preview, cache)?.into_interpreter()
    };

    writeln!(
        printer.stderr(),
        "Using Python {} interpreter at: {}",
        interpreter.python_version(),
        interpreter.sys_executable().user_display().cyan()
    )?;

    writeln!(
        printer.stderr(),
        "Creating virtualenv at: {}",
        venv.user_display().cyan()
    )?;

    Ok(uv_virtualenv::create_venv(
        &venv,
        interpreter,
        uv_virtualenv::Prompt::None,
        false,
        false,
    )?)
}

/// Update a [`PythonEnvironment`] to satisfy a set of [`RequirementsSource`]s.
pub(crate) async fn update_environment(
    venv: PythonEnvironment,
    requirements: &[RequirementsSource],
    connectivity: Connectivity,
    cache: &Cache,
    printer: Printer,
    preview: PreviewMode,
) -> Result<PythonEnvironment> {
    // TODO(zanieb): Support client configuration
    let client_builder = BaseClientBuilder::default().connectivity(connectivity);

    // Read all requirements from the provided sources.
    // TODO(zanieb): Consider allowing constraints and extras
    // TODO(zanieb): Allow specifying extras somehow
    let spec =
        RequirementsSpecification::from_sources(requirements, &[], &[], &client_builder).await?;

    // Check if the current environment satisfies the requirements
    let site_packages = SitePackages::from_executable(&venv)?;
    if spec.source_trees.is_empty() {
        match site_packages.satisfies(&spec.requirements, &spec.constraints)? {
            // If the requirements are already satisfied, we're done.
            SatisfiesResult::Fresh {
                recursive_requirements,
            } => {
                debug!(
                    "All requirements satisfied: {}",
                    recursive_requirements
                        .iter()
                        .map(|entry| entry.requirement.to_string())
                        .sorted()
                        .join(" | ")
                );
                return Ok(venv);
            }
            SatisfiesResult::Unsatisfied(requirement) => {
                debug!("At least one requirement is not satisfied: {requirement}");
            }
        }
    }

    // Determine the tags, markers, and interpreter to use for resolution.
    let interpreter = venv.interpreter().clone();
    let tags = venv.interpreter().tags()?;
    let markers = venv.interpreter().markers();

    // Initialize the registry client.
    // TODO(zanieb): Support client options e.g. offline, tls, etc.
    let client = RegistryClientBuilder::new(cache.clone())
        .connectivity(connectivity)
        .markers(markers)
        .platform(venv.interpreter().platform())
        .build();

    // TODO(charlie): Respect project configuration.
    let build_isolation = BuildIsolation::default();
    let compile = false;
    let concurrency = Concurrency::default();
    let config_settings = ConfigSettings::default();
    let dry_run = false;
    let extras = ExtrasSpecification::default();
    let flat_index = FlatIndex::default();
    let hasher = HashStrategy::default();
    let in_flight = InFlight::default();
    let index = InMemoryIndex::default();
    let index_locations = IndexLocations::default();
    let link_mode = LinkMode::default();
    let no_binary = NoBinary::default();
    let no_build = NoBuild::default();
    let options = Options::default();
    let preferences = Vec::default();
    let reinstall = Reinstall::default();
    let setup_py = SetupPyStrategy::default();
    let upgrade = Upgrade::default();

    // Create a build dispatch.
    let resolve_dispatch = BuildDispatch::new(
        &client,
        cache,
        &interpreter,
        &index_locations,
        &flat_index,
        &index,
        &in_flight,
        setup_py,
        &config_settings,
        build_isolation,
        link_mode,
        &no_build,
        &no_binary,
        concurrency,
        preview,
    );

    // Resolve the requirements.
    let resolution = match pip::operations::resolve(
        spec.requirements,
        spec.constraints,
        spec.overrides,
        spec.source_trees,
        spec.project,
        &extras,
        preferences,
        site_packages.clone(),
        &hasher,
        &reinstall,
        &upgrade,
        &interpreter,
        tags,
        markers,
        &client,
        &flat_index,
        &index,
        &resolve_dispatch,
        concurrency,
        options,
        printer,
        preview,
    )
    .await
    {
        Ok(resolution) => Resolution::from(resolution),
        Err(err) => return Err(err.into()),
    };

    // Re-initialize the in-flight map.
    let in_flight = InFlight::default();

    // If we're running with `--reinstall`, initialize a separate `BuildDispatch`, since we may
    // end up removing some distributions from the environment.
    let install_dispatch = if reinstall.is_none() {
        resolve_dispatch
    } else {
        BuildDispatch::new(
            &client,
            cache,
            &interpreter,
            &index_locations,
            &flat_index,
            &index,
            &in_flight,
            setup_py,
            &config_settings,
            build_isolation,
            link_mode,
            &no_build,
            &no_binary,
            concurrency,
            preview,
        )
    };

    // Sync the environment.
    pip::operations::install(
        &resolution,
        site_packages,
        pip::operations::Modifications::Sufficient,
        &reinstall,
        &no_binary,
        link_mode,
        compile,
        &index_locations,
        &hasher,
        tags,
        &client,
        &in_flight,
        concurrency,
        &install_dispatch,
        cache,
        &venv,
        dry_run,
        printer,
        preview,
    )
    .await?;

    // Notify the user of any resolution diagnostics.
    pip::operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    Ok(venv)
}

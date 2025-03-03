use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use indicatif::MultiProgress;
use marzano_gritmodule::config::{
    init_config_from_path, init_global_grit_modules, GritPatternTestInfo,
};
use marzano_gritmodule::fetcher::KeepFetcherKind;
use marzano_gritmodule::patterns_directory::PatternsDirectory;
use marzano_gritmodule::searcher::find_grit_modules_dir;
use marzano_gritmodule::utils::is_pattern_name;
use marzano_messenger::emit::{ApplyDetails, Messager, VisibilityLevels};
use serde::{Deserialize, Serialize};
use std::env::current_dir;
use std::io::{stdin, Read};
use std::path::Path;
use std::path::PathBuf;
use tracing::Instrument as _;

use crate::analytics::track_event_line;
use crate::error::GoodError;
use crate::flags::{GlobalFormatFlags, OutputFormat};
use crate::lister::list_applyables;
use crate::messenger_variant::create_emitter;
use crate::resolver::{get_grit_files_from, resolve_from, Source};
use crate::updater::Updater;

use super::super::analytics::AnalyticsArgs;
use super::apply_pattern::{run_apply_pattern, ApplyPatternArgs};
use super::check::{run_check, CheckArg};
use super::filters::SharedFilterArgs;
use super::list::ListArgs;
use super::parse::{run_parse, ParseInput};
use super::patterns::PatternsTestArgs;
use super::patterns_test::get_marzano_pattern_test_results;

#[derive(Deserialize)]
struct PlumbingApplyInput {
    pub pattern_body: String,
    pub paths: Vec<PathBuf>,
    pub root_path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct PlumbingCheckInput {
    pub paths: Vec<PathBuf>,
    pub root_path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct PlumbingPatternsListInput {
    pub grit_dir: PathBuf,
}

#[derive(Args, Debug, Serialize)]
pub struct SharedPlumbingArgs {
    /// The path to the input file, if unspecified, stdin is used
    #[clap(long = "input")]
    input: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Serialize)]
pub enum PlumbingArgs {
    /// Run `apply` from input
    Apply {
        #[command(flatten)]
        apply_pattern_args: ApplyPatternArgs,
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
    },
    /// Run `parse` via stdin
    Parse {
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
    },
    /// Send an analytics event via stdin
    Analytics {
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
        #[command(flatten)]
        args: AnalyticsArgs,
    },
    /// Run `check` via stdin
    Check {
        #[command(flatten)]
        args: CheckArg,
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
    },
    List {
        #[command(flatten)]
        args: ListArgs,
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
    },
    /// Run `patterns test` via stdin
    Test {
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
    },
    /// Run a workflow
    #[cfg(feature = "workflows_v2")]
    Run {
        #[command(flatten)]
        shared_args: SharedPlumbingArgs,
        /// Workflow definition file
        definition: String,
    },
}

fn read_input(shared_args: &SharedPlumbingArgs) -> Result<String> {
    let buffer = if let Some(input) = &shared_args.input {
        fs_err::read_to_string(input)?
    } else {
        let mut buffer = String::new();
        stdin().read_to_string(&mut buffer)?;
        buffer
    };
    Ok(buffer)
}

fn ensure_trailing_slash(root_path: &Path) -> PathBuf {
    let mut path_str = root_path.to_str().unwrap_or_default().to_string();
    if !path_str.ends_with('/') {
        path_str.push('/');
    }
    PathBuf::from(path_str)
}

pub(crate) async fn run_plumbing(
    args: PlumbingArgs,
    multi: MultiProgress,
    details: &mut ApplyDetails,
    parent: GlobalFormatFlags,
) -> Result<()> {
    let result = match args {
        PlumbingArgs::Apply {
            apply_pattern_args,
            shared_args,
        } => {
            let buffer = read_input(&shared_args)?;
            let input: PlumbingApplyInput = serde_json::from_str::<PlumbingApplyInput>(&buffer).map_err(|e| {
                anyhow!(
                    "Failed to parse input JSON: {}. Ensure that input matches schema \
                    {{ pattern_body: string; pattern_libs: {{ [string]: string }}; paths: string[]; }}",
                    e
                )
            })?;
            let grit_files = if input.paths.is_empty() {
                PatternsDirectory::new()
            } else {
                let path = PathBuf::from(input.paths.first().unwrap());
                init_config_from_path::<KeepFetcherKind>(path.clone(), false).await?;
                get_grit_files_from(Some(path)).await?
            };
            let raw_name = input.pattern_body.trim_end_matches("()");
            let pattern_libs = grit_files.get_pattern_libraries(raw_name)?;
            let body = if is_pattern_name(&input.pattern_body) && !input.pattern_body.ends_with(')')
            {
                format!("{}()", input.pattern_body)
            } else {
                input.pattern_body
            };
            run_apply_pattern(
                body,
                SharedFilterArgs::default(),
                input.paths,
                apply_pattern_args,
                multi,
                details,
                Some(pattern_libs.library()),
                Some(pattern_libs.language()),
                &parent,
                input.root_path.map(|p| ensure_trailing_slash(&p)),
            )
            .await
        }
        PlumbingArgs::Parse { shared_args } => {
            let buffer = read_input(&shared_args)?;
            let input = serde_json::from_str::<ParseInput>(&buffer).map_err(|e| {
                anyhow!(
                    "Failed to parse input JSON: {}. Ensure that input matches schema \
                    {{ pattern_body: string; paths: string[]; }}",
                    e
                )
            })?;
            let pattern_body = input.pattern_body.clone();
            run_parse(input.into(), parent, Some(pattern_body)).await
        }
        PlumbingArgs::Analytics { args, shared_args } => {
            let buffer = read_input(&shared_args)?;
            for line in buffer.lines() {
                let result = track_event_line(
                    line,
                    args.command.clone(),
                    args.args.clone(),
                    args.installation_id,
                    args.user_id.clone(),
                )
                .await;
                if let Err(e) = result {
                    eprintln!("Error when processing {}: {:#}", line, e);
                }
            }

            Ok(())
        }
        PlumbingArgs::Check { args, shared_args } => {
            let buffer = read_input(&shared_args)?;
            let input = serde_json::from_str::<PlumbingCheckInput>(&buffer).map_err(|e| {
                anyhow!(
                    "Failed to parse input JSON: {}. Ensure that input matches schema \
                    {{ paths: string[]; }}",
                    e
                )
            })?;
            if input.paths.is_empty() {
                return Ok(());
            }
            init_global_grit_modules::<KeepFetcherKind>(None).await?;
            let combined_args = CheckArg {
                paths: input.paths,
                ..args
            };
            run_check(
                combined_args,
                &parent,
                multi,
                true,
                input.root_path.map(|p| ensure_trailing_slash(&p)),
            )
            .await
        }
        PlumbingArgs::List { args, shared_args } => {
            let buffer = read_input(&shared_args)?;
            let input =
                serde_json::from_str::<PlumbingPatternsListInput>(&buffer).map_err(|e| {
                    anyhow!(
                        "Failed to parse input JSON: {}. Ensure that input matches schema \
                    {{ grit_dir: string; }}",
                        e
                    )
                })?;
            let grit_parent = match input.grit_dir.parent() {
                Some(parent) => parent,
                None => return Ok(()),
            };

            let (resolved, curr_repo) =
                resolve_from(grit_parent.to_path_buf(), &Source::All).await?;

            if resolved.is_empty() {
                let existing = find_grit_modules_dir(grit_parent.to_path_buf()).await?;
                if !existing.exists() {
                    bail!(
                    "No grit modules found in {}. Run `grit init` to initialize a grit project.",
                    grit_parent.to_string_lossy());
                } else {
                    bail!("No patterns found.");
                }
            }

            list_applyables(false, false, resolved, args.level, &parent, curr_repo).await
        }
        PlumbingArgs::Test { shared_args } => {
            let buffer = read_input(&shared_args)?;
            let patterns =
                serde_json::from_str::<Vec<GritPatternTestInfo>>(&buffer).map_err(|e| {
                    anyhow!(
                        "Failed to parse input JSON: {}. Ensure that input has correct schema. This command is
                        compatible with the output of `grit patterns list` --json`",
                        e
                    )
                })?;

            let cwd = std::env::current_dir()?;
            let libs = get_grit_files_from(Some(cwd)).await?;
            let res = get_marzano_pattern_test_results(
                patterns,
                &libs,
                &PatternsTestArgs {
                    update: false,
                    verbose: false,
                    watch: false,
                    filter: None,
                    exclude: vec![],
                },
                parent.into(),
            )
            .await?;
            match res {
                super::patterns_test::AggregatedTestResult::SomeFailed(message) => {
                    Err(anyhow::anyhow!(message))
                }
                super::patterns_test::AggregatedTestResult::AllPassed => Ok(()),
            }
        }
        #[cfg(feature = "workflows_v2")]
        PlumbingArgs::Run {
            shared_args,
            definition,
        } => {
            let buffer = read_input(&shared_args)?;

            let execution_id = std::env::var("GRIT_EXECUTION_ID")
                .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

            let tenant_id = std::env::var("GRIT_TENANT_ID").unwrap_or_default();

            tracing::info!(
                "Running workflow with execution_id: {} in tenant {}",
                execution_id,
                tenant_id
            );

            let format = OutputFormat::from(&parent);
            let mut emitter = create_emitter(
                &format,
                marzano_messenger::output_mode::OutputMode::default(),
                None,
                false,
                None,
                None,
                VisibilityLevels::default(),
            )
            .await?;

            let current_dir = current_dir()?;
            let mut updater = Updater::from_current_bin().await?;
            let auth = updater
                .get_valid_auth()
                .instrument(tracing::span!(
                    tracing::Level::INFO,
                    "grit_marzano.plumbing.auth",
                    "execution_id" = execution_id.as_str(),
                    "tenant_id" = tenant_id.as_str(),
                ))
                .await
                .ok();

            let custom_workflow = match crate::workflows::find_workflow_file_from(
                current_dir.clone(),
                &definition,
                auth,
            )
            .instrument(tracing::span!(
                tracing::Level::INFO,
                "grit_marzano.plumbing.find_workflow",
                "execution_id" = execution_id.as_str(),
                "tenant_id" = tenant_id.as_str(),
            ))
            .await
            .context("Failed to find workflow file")
            {
                Ok(workflow) => workflow,
                Err(e) => {
                    let log = marzano_messenger::SimpleLogMessage::new_error(format!(
                        "Failed to find workflow file: {}",
                        e
                    ));
                    emitter.emit_log(&log)?;
                    return Err(e);
                }
            };

            super::apply_migration::run_apply_migration(
                custom_workflow,
                vec![current_dir],
                None,
                super::apply_migration::ApplyMigrationArgs {
                    input: Some(buffer),
                    remote: false,
                    workflow_id: None,
                    verbose: true,
                    watch: false,
                },
                emitter,
                execution_id.clone(),
            )
            .instrument(tracing::span!(
                tracing::Level::INFO,
                "grit_marzano.plumbing.run_workflow",
                "execution_id" = execution_id.as_str(),
                "tenant_id" = tenant_id.as_str(),
            ))
            .await?;

            Ok(())
        }
    };
    // We want plumbing to always return a success code, even for "good" errors (failed checks, etc)
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            if e.downcast_ref::<GoodError>().is_some() {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

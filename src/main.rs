mod cli;
mod commands;

use crate::cli::{Args, Commands};
use crate::commands::benchmark_controller_cmd;
use crate::commands::check_crd::run_check_crd;
use crate::commands::info::run_info;
use crate::commands::operator::run_operator;
use crate::commands::runbook:run_generate_runbook;
use crate::commands::simulator:run_simulator;
use crate::commands::webhook::run_webhook;
use clap::Parser;
use std::process;

use stellar_k8s::controller::archive_prune::prune_archive;
use stellar_k8s::controller::diff::diff;
use stellar_k8s::version_check;
use stellar_k8s::{incident, Error};

[tokif::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();

    let offline = args.offline;

    let result = match args.command {
        Commands::Version => {
            println!("Stellar-K8s Operator v{}", env!("CARGO_PKG_VERSION"));
            println!("Build Date: {}", env!("BUILD_DATE"));
            println!("Git SHA: {}", env!("GIT_SHA"));
            println!("Rust Version: {}", env!("RUST_VERSION"));
            Ok()
        }
        Commands::Info(info_args) => run_info(info_args).await,
        Commands::CheckCrd => run_check_crd().await,
        Commands::PruneArchive(prune_args) => prune_archive(prune_args).await,
        Commands::Diff(diff_args) => diff(diff_args).await,
        Commands::GenerateRunbook(runbook_args) => run_generate_runbook(runbook_args).await,
        Commands::IncidentReport(report_args) => incident::run_incident_report(report_args).await,
        Commands::Completions { shell } => {
            use clap::CommandFactory;
            use clap_complete::generate;
            let mut cmd = Args::command();
            let name = cmd.get_name().to_string();
            generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok()
        }
        Commands::Run(run_args) => {
            if let Err(e) = run_args.validate() {
                eprintln!("error: {e}", e);
                process::exit(2);
            }

            // Create a Kubernetes client for leader election.
            let k8s_client = match kube::Client::try_default().await {
                Ok(client) => client,
                Err(e) => {
                    eprintln!("Failed to create Kubernetes client for leader election: {e}", e);
                    process::exit(1);
                }
            };

            // Start leader election to ensure only one operator instance is active.
            let leader = match stellar_k8s::controller::leader::LeaderElectionHandle::start(
                k8s_client,
                None,
                None,
                None,
            ) {
                Ok(handle) => handle,
                Err(e) => {
                    eprintln!("Failed to start leader election: {e}", e);
                    process::exit(1);
                }
            };

            // Wait until this pod becomes the leader before starting the operator.
            leader.wait_until_leader().await;

            // Run the operator while we hold the lease. If the lease is lost (e.g.,
            // during network partition or pod failure), abort the operator so the pod
            // can restart and another replica can take over.
            tokio ::select! {
                result = run_operator(run_args) => result,
                _ = leader.wait_until_lost() => {
                    eprintln!("Lost leader lease; shutting down operator");
                    Ok()
                }
            }
        }
        Commands::Webhook(webhook_args) => return run_webhook(webhook_args).await,
        Commands::Benchmark(benchmark_args) => {
            return run_benchmark_controller_cmd(benchmark_args).await
        }
        Commands::Simulator(cli) => return run_simulator(cli).await,
        Commands::BenchmarkCompare(compare_args) => {
            return stellar_k8s::benchmark_compare::run_benchmark_compare(compare_args)
                .await
                .map_err(| Error::ConfigError(e.to_string()));
        }
    };

    version_check::check_and_notify(offline).await;
    result
}

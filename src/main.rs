mod acp;
mod config;
mod listen;
mod mm;
mod probe;
mod prompt;
mod read;
mod scratch;
mod self_dm;
mod smoke;
mod store;
mod stub_agent;

use std::process::exit;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    let result = match command {
        Some("listen") => listen::run(rest).await,
        Some("probe") => probe::run().await,
        Some("smoke") => smoke::run(rest).await,
        Some("read") => read::run(rest).await,
        Some("cleanup-scratch") => scratch::run_cleanup(rest),
        Some("stub-agent") => stub_agent::run().await,
        _ => {
            eprintln!(
                "usage: mattermost-acp-assist <listen [--as <owner>]|probe|smoke [--web <URL> --profile <username> [--question <text>]]|read [--as <owner>] <permalink|post-id>|cleanup-scratch [--dry-run]|stub-agent>"
            );
            exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("{error:#}");
        exit(1);
    }
}

//! sshp - Parallel SSH Executor (Rust Port)
//!
//! A Rust implementation of sshp that executes commands on multiple hosts in parallel
//! using the system's SSH client via the openssh crate.

use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, StreamExt};
use openssh::SessionBuilder;
use std::process::Output;
use tokio::time::timeout;

/// Program metadata
const PROG_NAME: &str = "sshp";
const PROG_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_MAX_JOBS: usize = 50;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 300;

/// SSH execution configuration
#[derive(Debug, Clone)]
struct SshConfig {
    username: Option<String>,
    port: Option<u16>,
    identity: Option<PathBuf>,
    connect_timeout: Duration,
    command_timeout: Duration,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            username: None,
            port: None,
            identity: None,
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            command_timeout: Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS),
        }
    }
}

/// Result of a single SSH execution
#[derive(Debug)]
struct ExecutionResult {
    host: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
    success: bool,
}

/// CLI arguments using clap derive macro
#[derive(Parser, Debug)]
#[command(
    name = PROG_NAME,
    version = PROG_VERSION,
    about = "Parallel SSH Executor - run commands on multiple hosts",
    long_about = "Executes SSH commands on multiple hosts in parallel with configurable concurrency."
)]
struct Args {
    /// Hosts file (use - for stdin)
    #[arg(short, long, value_name = "FILE", default_value = "-")]
    file: String,

    /// Maximum parallel SSH connections
    #[arg(short, long, value_name = "N", default_value_t = DEFAULT_MAX_JOBS)]
    max_jobs: usize,

    /// SSH port (overrides default 22)
    #[arg(short, long, value_name = "PORT")]
    port: Option<u16>,

    /// SSH username (overrides current user)
    #[arg(short = 'l', long, value_name = "USER")]
    login: Option<String>,

    /// SSH identity file (private key)
    #[arg(short = 'i', long, value_name = "FILE")]
    identity: Option<PathBuf>,

    /// Connection timeout in seconds
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_CONNECT_TIMEOUT_SECS)]
    connect_timeout: u64,

    /// Command timeout in seconds
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_COMMAND_TIMEOUT_SECS)]
    command_timeout: u64,

    /// Remote command to execute
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

/// Parses host entries from a buffered input source
fn parse_hosts<R: BufRead>(reader: R) -> Vec<String> {
    reader
        .lines()
        .filter_map(|line| line.ok().map(|l| l.trim().to_string()))
        .filter(|line| {
            let trimmed = line.as_str();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .collect()
}

/// Creates a host reader from file path or stdin
fn create_host_reader(file_path: &str) -> Result<Box<dyn BufRead>> {
    match file_path {
        "-" => Ok(Box::new(BufReader::new(io::stdin()))),
        path => {
            let file = std::fs::File::open(path)
                .with_context(|| format!("Failed to open hosts file: {}", path))?;
            Ok(Box::new(BufReader::new(file)))
        }
    }
}

/// Executes a command on a remote host via SSH
async fn execute_on_host(
    host: &str,
    command: &[String],
    config: &SshConfig,
) -> Result<ExecutionResult> {
    let mut builder = SessionBuilder::default();

    // Configure SSH options
    if let Some(ref user) = config.username {
        builder.user(user.clone());
    }

    if let Some(port) = config.port {
        builder.port(port);
    }

    if let Some(ref identity) = config.identity {
        builder.keyfile(identity);
    }

    // Connect with timeout
    let session = timeout(config.connect_timeout, builder.connect(host))
        .await
        .map_err(|_| anyhow::anyhow!("Connection timeout"))?
        .with_context(|| format!("Failed to connect to {}", host))?;

    let remote_cmd = command.join(" ");

    // Execute command with timeout
    let output: Output = timeout(
        config.command_timeout,
        session.command("sh").arg("-c").arg(&remote_cmd).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Command execution timeout"))?
    .with_context(|| format!("Failed to execute command on {}", host))?;

    // Gracefully close the session
    let _ = session.close().await;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(ExecutionResult {
        host: host.to_string(),
        exit_code,
        stdout,
        stderr,
        success: exit_code == 0,
    })
}

/// Prints execution results in a formatted manner
/// Returns true if any execution failed
fn print_results(results: &[ExecutionResult]) -> bool {
    let any_failed = results.iter().any(|r| !r.success);

    for result in results {
        let status_indicator = if result.success { "✓" } else { "✗" };
        eprintln!(
            "[{}] {} exited: {}",
            status_indicator, result.host, result.exit_code
        );

        let output = if result.stdout.is_empty() {
            &result.stderr
        } else {
            &result.stdout
        };

        if !output.is_empty() {
            for line in output.lines() {
                println!("  {}", line);
            }
        }
        eprintln!();
    }

    // Summary
    let total = results.len();
    let successful = results.iter().filter(|r| r.success).count();
    let failed = total - successful;

    eprintln!("─────────────────────────────");
    eprintln!("Summary: {}/{} successful", successful, total);
    if failed > 0 {
        eprintln!("         {} failed", failed);
    }

    any_failed
}

/// Main entry point
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize args
    let args = Args::parse();

    // Create SSH configuration
    let ssh_config = SshConfig {
        username: args.login,
        port: args.port,
        identity: args.identity,
        connect_timeout: Duration::from_secs(args.connect_timeout),
        command_timeout: Duration::from_secs(args.command_timeout),
    };

    // Read hosts
    let reader = create_host_reader(&args.file)?;
    let hosts = parse_hosts(reader);

    if hosts.is_empty() {
        anyhow::bail!("No hosts specified");
    }

    // Print execution info to stderr
    eprintln!("Executing: {:?}", args.command.join(" "));
    eprintln!(
        "On {} host(s) with max {} parallel connection(s)",
        hosts.len(),
        args.max_jobs
    );
    eprintln!();

    // Create futures for each host
    let futures = hosts.into_iter().map(|host| {
        let config = ssh_config.clone();
        let command = args.command.clone();

        async move {
            let result = execute_on_host(&host, &command, &config).await;
            (host, result)
        }
    });

    // Execute all futures with bounded concurrency using buffer_unordered
    let results: Vec<_> = stream::iter(futures)
        .buffer_unordered(args.max_jobs)
        .collect()
        .await;

    // Process and collect results
    let mut execution_results: Vec<ExecutionResult> = results
        .into_iter()
        .map(|(host, result)| match result {
            Ok(exec_result) => exec_result,
            Err(e) => ExecutionResult {
                host,
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("Error: {}", e),
                success: false,
            },
        })
        .collect();

    // Sort by host name for consistent output
    execution_results.sort_by(|a, b| a.host.cmp(&b.host));

    // Print results and get exit status
    let any_failed = print_results(&execution_results);

    // Exit with appropriate code
    if any_failed {
        std::process::exit(1);
    }

    Ok(())
}

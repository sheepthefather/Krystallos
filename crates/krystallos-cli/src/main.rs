//! Debug CLI for the Krystallos kernel.
//!
//! This exists so that backend work can be driven and inspected on the host,
//! without an emulator, an APK build, or a device. Everything a backend can do
//! is reachable from here, which makes it the primary verification tool for the
//! SMB layer: if a behaviour cannot be exercised through this CLI, it is
//! effectively untestable until the Android side exists.
//!
//! It is also what makes the differential test possible —`krystallos get`
//! against a `file://` URI and against an `smb://` URI must produce byte-
//! identical output for the same file.

use clap::{Parser, Subcommand};
use krystallos_core::{BackendRegistry, ConnectionOptions, Credentials};
use krystallos_local::LocalDriver;
use krystallos_smb::SmbDriver;
use std::path::PathBuf;
use std::process::ExitCode;

mod commands;
mod format;

#[derive(Parser)]
#[command(
    name = "krystallos",
    version,
    about = "Debug CLI for the Krystallos storage kernel",
    long_about = "Connects to a storage endpoint and performs filesystem operations \
                  against it.\n\nEndpoints are URIs: `file:///D:/media` for a local \
                  directory, `smb://host/share` for an SMB share.\n\nPasswords are \
                  read from the KRYSTALLOS_PASSWORD environment variable so they stay \
                  out of shell history."
)]
struct Cli {
    #[arg(long, global = true, env = "KRYSTALLOS_USER", value_name = "NAME")]
    user: Option<String>,

    /// Prefer the KRYSTALLOS_PASSWORD environment variable over this flag.
    #[arg(long, global = true, env = "KRYSTALLOS_PASSWORD", value_name = "SECRET")]
    password: Option<String>,

    #[arg(long, global = true, env = "KRYSTALLOS_DOMAIN", value_name = "DOMAIN")]
    domain: Option<String>,

    /// Request SMB3 transport encryption.
    ///
    /// Off by default. libsmb2 uses its own portable reference AES everywhere
    /// except Apple, including Android, and enabling encryption was measured
    /// dropping reads from hundreds of megabytes per second to under four. Turn
    /// it on when the network is not trusted and the slower transfer is the
    /// right trade.
    #[arg(long, global = true, env = "KRYSTALLOS_SMB_SEAL")]
    smb_seal: bool,

    /// Read-ahead window size in bytes for `cat` and `get`. 0 disables it.
    ///
    /// Read-ahead trades memory for latency, not throughput: it does not make
    /// the network faster, it just means a sequential read usually does not pay
    /// a round-trip per request. Whether that helps depends on the access
    /// pattern, so it is a flag rather than always-on —and 0 is how you measure
    /// the baseline it is being compared against.
    #[arg(long, global = true, default_value_t = 1024 * 1024, value_name = "BYTES")]
    read_ahead: usize,

    /// How many bytes `cat` and `get` ask for per read.
    ///
    /// Separate from `--read-ahead` because the two interact, and that
    /// interaction is the whole story: read-ahead only helps when the caller
    /// reads *smaller* than the window. A media player reads a few tens of
    /// kilobytes at a time, so this defaults to something closer to that than
    /// to the window size.
    #[arg(long, global = true, default_value_t = 64 * 1024, value_name = "BYTES")]
    chunk: usize,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the URI schemes this build can connect to
    Schemes,

    /// List a directory
    Ls {
        uri: String,
        #[arg(default_value = "/")]
        path: String,
    },

    /// Show metadata for a single entry
    Stat { uri: String, path: String },

    /// Write a file's contents to standard output
    Cat { uri: String, path: String },

    /// Download a file to the local filesystem
    Get {
        uri: String,
        path: String,
        dest: PathBuf,
    },

    /// Upload a local file
    Put {
        uri: String,
        path: String,
        src: PathBuf,
    },

    /// Create a directory
    Mkdir { uri: String, path: String },

    /// Delete a file
    Rm { uri: String, path: String },

    /// Delete an empty directory
    Rmdir { uri: String, path: String },

    /// Rename or move an entry within the backend
    Mv { uri: String, from: String, to: String },
}

/// Build the registry of available backends.
///
/// Registering a protocol here is all it takes to make it reachable from every
/// subcommand.
fn registry() -> BackendRegistry {
    let mut reg = BackendRegistry::new();
    reg.register(Box::new(LocalDriver::new()));
    reg.register(Box::new(SmbDriver::new()));
    reg
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let credentials = Credentials {
        username: cli.user.clone(),
        password: cli.password.clone(),
        domain: cli.domain.clone(),
    };

    let mut options = ConnectionOptions::new();
    if cli.smb_seal {
        options.set(krystallos_smb::OPT_SEAL, "true");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = runtime.block_on(run(cli, credentials, options));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            // Walk the source chain so a wrapped backend error is not lost —
            // the outer message alone is often just "I/O error".
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(
    cli: Cli,
    credentials: Credentials,
    options: ConnectionOptions,
) -> krystallos_core::Result<()> {
    let reg = registry();

    match cli.command {
        Command::Schemes => {
            for scheme in reg.schemes() {
                let description = reg
                    .driver_for(scheme)
                    .map(|d| d.description())
                    .unwrap_or("");
                println!("{scheme:<8} {description}");
            }
            Ok(())
        }
        Command::Ls { uri, path } => commands::ls(&reg, &credentials, &options, &uri, &path).await,
        Command::Stat { uri, path } => commands::stat(&reg, &credentials, &options, &uri, &path).await,
        Command::Cat { uri, path } => {
            commands::cat(&reg, &credentials, &options, &uri, &path, cli.read_ahead, cli.chunk).await
        }
        Command::Get { uri, path, dest } => {
            commands::get(&reg, &credentials, &options, &uri, &path, &dest, cli.read_ahead, cli.chunk).await
        }
        Command::Put { uri, path, src } => {
            commands::put(&reg, &credentials, &options, &uri, &path, &src).await
        }
        Command::Mkdir { uri, path } => commands::mkdir(&reg, &credentials, &options, &uri, &path).await,
        Command::Rm { uri, path } => commands::rm(&reg, &credentials, &options, &uri, &path).await,
        Command::Rmdir { uri, path } => commands::rmdir(&reg, &credentials, &options, &uri, &path).await,
        Command::Mv { uri, from, to } => commands::mv(&reg, &credentials, &options, &uri, &from, &to).await,
    }
}

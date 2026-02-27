use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::{env, io, ptr};

use catacomb_ipc::{CliToggle, IpcMessage};
use clap::{Parser, Subcommand};
#[cfg(feature = "profiling")]
use profiling::puffin;
#[cfg(feature = "profiling")]
use puffin_http::Server;
use tracing_subscriber::{EnvFilter};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

mod backend;
mod config;
mod input;
mod output;
mod protocols;
mod shell;
mod state;
mod utils;

/// Command line arguments.
#[derive(Parser, Debug)]
#[clap(author, about, version, max_term_width = 80)]
struct Options {
    #[clap(subcommand)]
    pub subcommands: Option<Subcommands>,
}

#[derive(Subcommand, Debug)]
pub enum Subcommands {
    /// Send IPC messages to Catacomb.
    #[clap(subcommand)]
    Msg(IpcMessage),
}

pub fn run() {
    #[cfg(feature = "profiling")]
    let _server = {
        puffin::set_scopes_on(true);
        Server::new(&format!("0.0.0.0:{}", puffin_http::DEFAULT_PORT)).unwrap()
    };

    let directives = env::var("RUST_LOG").unwrap_or(
        "info,catacomb=info,smithay::xwayland::xwm=error,smithay::backend::drm=error,calloop::loop_logic=error"
            .into(),
    );
    let env_filter = EnvFilter::builder().parse_lossy(directives);

    let base_state = env::var("XDG_STATE_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            dirs::home_dir().map(|h| h.join(".local").join("state"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let log_dir = base_state.join("jollypad").join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file_path = log_dir.join("jollypad-catacomb.log");

    // Truncate the log file at the start of each run to avoid accumulation across runs
    let _ = std::fs::File::create(&log_file_path);

    let file_writer = {
        let path = log_file_path.clone();
        move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map(|f| -> Box<dyn std::io::Write + Send> { Box::new(f) })
                .unwrap_or_else(|_| Box::new(std::io::stderr()))
        }
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_line_number(true))
        .with(fmt::layer().with_line_number(true).with_writer(file_writer))
        .init();

    // Add panic hook
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!("Catacomb PANIC: {}\nBacktrace:\n{}", info, backtrace);
        // Also write to a separate file just in case
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open("/home/jolly/phs/jollypad/catacomb_panic.log") {
            use std::io::Write;
            let _ = writeln!(file, "PANIC: {}\n{:?}", info, backtrace);
        }
    }));

    tracing::info!("🚀 Catacomb v1.0.6 starting...");

    match Options::parse().subcommands {
        Some(Subcommands::Msg(msg)) => match catacomb_ipc::send_message(&msg) {
            Err(err) => eprintln!("\x1b[31merror\x1b[0m: {err}"),
            Ok(Some(IpcMessage::DpmsReply { state: CliToggle::On })) => println!("on"),
            Ok(Some(IpcMessage::DpmsReply { state: CliToggle::Off })) => println!("off"),
            Ok(_) => (),
        },
        None => backend::udev::run(),
    }
}

/// Spawn unsupervised daemons.
///
/// This will double-fork to avoid spawning zombies, but does not provide any
/// ability to retrieve the process output.
pub fn daemon<I, S>(program: S, args: I) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    unsafe {
        command.pre_exec(|| {
            // Perform second fork.
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => (),
                _ => libc::_exit(0),
            }

            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }

            // Reset signal handlers.
            let mut signal_set = MaybeUninit::uninit();
            libc::sigemptyset(signal_set.as_mut_ptr());
            libc::sigprocmask(libc::SIG_SETMASK, signal_set.as_mut_ptr(), ptr::null_mut());

            Ok(())
        });
    }

    command.spawn()?.wait()?;

    Ok(())
}

/// Log an error, ignoring success.
///
/// This is a macro to preserve log message line numbers.
#[macro_export]
macro_rules! trace_error {
    ($result:expr) => {{
        if let Err(err) = &$result {
            tracing::error!("{err}");
        }
    }};
}

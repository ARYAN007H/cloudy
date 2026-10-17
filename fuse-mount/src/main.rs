mod filesystem;
mod inode;

use clap::Parser;
use clouddrive_core::{AppConfig, CoreEngine};
use fuser::{MountOption, mount2};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::runtime::Runtime;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::filesystem::CloudDriveFs;
use crate::inode::InodeMap;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Mount point for CloudDrive
    #[arg(short, long, default_value = "~/CloudDrive")]
    mount_point: String,

    /// Run in foreground
    #[arg(short, long)]
    foreground: bool,

    /// Path to config file
    #[arg(short, long, default_value = "~/.config/clouddrive/config.toml")]
    config: String,

    /// Log level
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize logging
    let log_level = match args.log_level.to_lowercase().as_str() {
        "debug" => Level::DEBUG,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        "trace" => Level::TRACE,
        _ => Level::INFO,
    };
    
    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Setting default subscriber failed");

    let mount_path = expand_tilde(&args.mount_point);

    info!("Starting CloudDrive FUSE mount at {:?}", mount_path);

    // Ensure mount point exists
    if !mount_path.exists() {
        fs::create_dir_all(&mount_path)?;
    }

    // Set up tokio runtime
    let rt = Runtime::new()?;
    let runtime_handle = rt.handle().clone();

    // Load config from file or use defaults
    let mut config = AppConfig::load_or_default();
    config.mount_point = mount_path.clone();
    config.log_level = args.log_level.clone();

    // CoreEngine::new is synchronous
    let engine = CoreEngine::new(config)
        .expect("Failed to initialize CoreEngine");
    
    let engine = Arc::new(engine);
    // The engine was built outside the runtime: tell it which runtime to use for
    // background work, otherwise start_sync() below cannot spawn the sync tasks.
    engine.set_runtime(runtime_handle.clone());

    if !engine.has_client_credentials() {
        error!("No client credentials found. Please configure OAuth credentials.");
        println!("No OAuth credentials configured.");
        println!("Run the CloudDrive GUI to set up credentials, or see SETUP.md for instructions.");
        std::process::exit(1);
    }

    // list_accounts is synchronous
    let accounts: Vec<String> = engine.list_accounts();

    if accounts.is_empty() {
        error!("No linked accounts found. Please add an account.");
        println!("No accounts linked. Run the CloudDrive GUI to add accounts.");
        std::process::exit(1);
    }

    info!("Found {} linked account(s): {:?}", accounts.len(), accounts);

    // Start sync engine (synchronous)
    if let Err(e) = engine.start_sync() {
        error!("Failed to start sync engine: {:?}", e);
    }

    // We use the current process's uid and gid for file ownership
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let cloud_fs = CloudDriveFs {
        engine: engine.clone(),
        runtime: runtime_handle,
        inodes: RwLock::new(InodeMap::new()),
        open_files: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(1),
        uid,
        gid,
    };

    let options = vec![
        MountOption::RW,
        MountOption::FSName("clouddrive".to_string()),
        MountOption::AutoUnmount,
    ];

    info!("Mounting FUSE filesystem...");
    let mount_res = mount2(cloud_fs, &mount_path, &options);
    
    // On unmount, stop sync engine (synchronous)
    engine.stop_sync();

    if let Err(e) = mount_res {
        error!("FUSE mount failed: {:?}", e);
        std::process::exit(1);
    }

    info!("CloudDrive FUSE mount exited cleanly");
    Ok(())
}

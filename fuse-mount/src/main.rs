//! `clouddrive-fuse` – exposes the Cloudy pool as a normal folder.
//! FUSE is a Linux feature here; on other systems this binary only explains that.

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod filesystem;
#[cfg(target_os = "linux")]
mod inode;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    app::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("clouddrive-fuse only works on Linux.");
    eprintln!("On this system, use the Cloudy desktop app (clouddrive-gui) to browse your pool.");
    std::process::exit(2);
}

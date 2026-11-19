//! Ctrl-C, and SIGTERM and SIGHUP on Unix, for the command line and the
//! worker.
//!
//! The first interrupt asks the running command to stop. A compile or a
//! download passes [`requested`] to the library as its cancel callback, so it
//! stops within moments and removes its scratch space and partial outputs;
//! the command then exits with status 130. A command that cannot stop part
//! way instead registers the partial file it writes with [`remove_on_exit`],
//! and the interrupt removes that file and exits at once. A second
//! interrupt always exits at once, after removing whatever is registered.
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The exit status after an interrupt: 128 + SIGINT, as shells report it.
pub const EXIT_STATUS: i32 = 130;

static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
/// Whether the running command stops by itself when asked.
static COOPERATIVE: AtomicBool = AtomicBool::new(false);
/// Paths removed when an interrupt ends the process at once.
static REMOVE_ON_EXIT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Installs the handler. Without the `ctrlc` feature, or if another handler
/// is installed already, interrupts keep their default effect.
pub fn install() {
    #[cfg(feature = "ctrlc")]
    let _ = ctrlc::set_handler(on_interrupt);
}

#[cfg_attr(not(feature = "ctrlc"), allow(dead_code))]
fn on_interrupt() {
    let first = INTERRUPTS.fetch_add(1, Ordering::SeqCst) == 0;
    if first && COOPERATIVE.load(Ordering::SeqCst) {
        eprintln!("\nStopping and cleaning up; press Ctrl-C again to quit at once");
        return;
    }
    exit_now();
}

/// Removes what is registered and exits with [`EXIT_STATUS`].
fn exit_now() -> ! {
    let paths = match REMOVE_ON_EXIT.lock() {
        Ok(paths) => paths.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    for path in paths {
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove {}: {error}", path.display());
        }
    }
    eprintln!("Interrupted");
    std::process::exit(EXIT_STATUS);
}

/// Whether an interrupt has asked the process to stop.
pub fn requested() -> bool {
    INTERRUPTS.load(Ordering::SeqCst) > 0
}

/// Marks the running command as one that stops by itself when
/// [`requested`] turns true, until the guard is dropped.
pub fn cooperate() -> Cooperating {
    COOPERATIVE.store(true, Ordering::SeqCst);
    Cooperating(())
}

pub struct Cooperating(());

impl Drop for Cooperating {
    fn drop(&mut self) {
        COOPERATIVE.store(false, Ordering::SeqCst);
    }
}

/// Registers `path` for removal if an interrupt ends the process at once,
/// until the guard is dropped. The guard does not remove it.
pub fn remove_on_exit(path: PathBuf) -> Registered {
    let mut paths = REMOVE_ON_EXIT.lock().unwrap_or_else(|e| e.into_inner());
    paths.push(path.clone());
    Registered(path)
}

pub struct Registered(PathBuf);

impl Drop for Registered {
    fn drop(&mut self) {
        let mut paths = REMOVE_ON_EXIT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = paths.iter().rposition(|path| *path == self.0) {
            paths.remove(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_unregister_their_paths() {
        let a = remove_on_exit(PathBuf::from("a"));
        let b = remove_on_exit(PathBuf::from("b"));
        drop(a);
        assert_eq!(*REMOVE_ON_EXIT.lock().unwrap(), [PathBuf::from("b")]);
        drop(b);
        assert!(REMOVE_ON_EXIT.lock().unwrap().is_empty());
        let cooperating = cooperate();
        assert!(COOPERATIVE.load(Ordering::SeqCst));
        drop(cooperating);
        assert!(!COOPERATIVE.load(Ordering::SeqCst));
    }
}

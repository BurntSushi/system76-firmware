extern crate buildchain;
extern crate ecflash;
extern crate libc;
extern crate lzma;
extern crate plain;
extern crate serde;
extern crate serde_json;
extern crate sha2;
extern crate tar;
extern crate tempdir;
extern crate uuid;

use buildchain::{Block, Downloader, Manifest};
use std::fs;
use std::path::Path;

pub mod config;
pub mod download;
pub mod util;

mod bios;
mod boot;
mod ec;
mod me;
mod mount;
mod thelio_io;

pub use bios::bios;
pub use ec::ec;
pub use me::me;
pub use thelio_io::{
    ThelioIo, ThelioIoMetadata,
    thelio_io_download, thelio_io_list, thelio_io_update
};

const SECONDS_IN_DAY: u64 = 60 * 60 * 24;

const MODEL_WHITELIST: &[&str] = &[
    "addw1",
    "bonw11",
    "bonw12",
    "bonw13",
    "darp5",
    "galp2",
    "galp3",
    "galp3-b",
    "galp3-c",
    "gaze10",
    "gaze11",
    "gaze12",
    "gaze13",
    "gaze14",
    "kudu2",
    "kudu3",
    "kudu4",
    "kudu5",
    "lemu6",
    "lemu7",
    "lemu8",
    "meer4",
    "orxp1",
    "oryp2",
    "oryp2-ess",
    "oryp3",
    "oryp3-b",
    "oryp3-ess",
    "oryp4",
    "oryp4-b",
    "oryp5",
    "serw9",
    "serw10",
    "serw11",
    "serw11-b",
    "thelio-b1",
    "thelio-major-b1",
    "thelio-major-b1.1",
    "thelio-major-b2",
    "thelio-major-r1",
    "thelio-r1",
];

pub fn model_is_whitelisted(model: &str) -> bool {
    MODEL_WHITELIST
        .into_iter()
        .find(|whitelist| model == **whitelist)
        .is_some()
}

// Helper function for errors
pub fn err_str<E: ::std::fmt::Display>(err: E) -> String {
    format!("{}", err)
}

pub fn firmware_id() -> Result<String, String> {
    let (bios_model, _bios_version) = bios::bios()?;
    let (ec_project, _ec_version) = ec::ec_or_none(true);
    let ec_hash = util::sha256(ec_project.as_bytes());
    Ok(format!("{}_{}", bios_model, ec_hash))
}

fn remove_dir<P: AsRef<Path>>(path: P) -> Result<(), String> {
    if path.as_ref().exists() {
        eprintln!("removing {}", path.as_ref().display());
        match fs::remove_dir_all(&path) {
            Ok(()) => (),
            Err(err) => {
                return Err(format!("failed to remove {}: {}", path.as_ref().display(), err));
            }
        }
    }

    Ok(())
}

pub fn download() -> Result<(String, String), String> {
    let firmware_id = firmware_id()?;

    let dl = Downloader::new(
        config::KEY,
        config::URL,
        config::PROJECT,
        config::BRANCH,
        Some(config::CERT)
    )?;

    let tail = {
        let path = Path::new(config::CACHE).join("tail");
        cached_block(&path, SECONDS_IN_DAY, || dl.tail())?
    };

    let cache = download::Cache::new(config::CACHE, Some(dl))?;

    eprintln!("downloading manifest.json");
    let manifest_json = cache.object(&tail.digest)?;
    let manifest = serde_json::from_slice::<Manifest>(&manifest_json).map_err(|e| e.to_string())?;

    let _updater_data = {
        let file = "system76-firmware-update.tar.xz";
        eprintln!("downloading {}", file);
        let digest = manifest.files.get(file).ok_or(format!("{} not found", file))?;
        cache.object(&digest)?
    };

    let firmware_data = {
        let file = format!("{}.tar.xz", firmware_id);
        eprintln!("downloading {}", file);
        let digest = manifest.files.get(&file).ok_or(format!("{} not found", file))?;
        cache.object(&digest)?
    };

    let changelog = util::extract_file(&firmware_data, "./changelog.json").map_err(err_str)?;

    Ok((tail.digest.to_string(), changelog))
}

/// Retrieves a `Block` from the cached path if it exists and the modified time is recent.
///
/// - If the modified time is older than `stale_after` seconds, the cache will be updated.
/// - The most recent `Block` from cache will be returned after the cache is updated.
/// - If the cache does not require an update, it will be returned after being deserialized.
fn cached_block<F: FnMut() -> Result<Block, String>>(
    path: &Path,
    stale_after: u64,
    mut func: F
) -> Result<Block, String>  {
    // - Fetch the timestamp of the cached tail block
    // - If recent, attempt to deserialize it
    let read_cache = |modified| -> Result<Option<Block>, String> {
        let now = timestamp::current();
        let cached_tail = if timestamp::exceeded(modified, now, stale_after) {
            None
        } else {
            let file = fs::File::open(&path).map_err(err_str)?;
            let block = bincode::deserialize_from(file).map_err(err_str)?;

            Some(block)
        };

        Ok(cached_tail)
    };

    // Fetches a new tail block
    let mut update_cache = || {
        let block = func()?;
        let file = fs::File::create(&path).map_err(err_str)?;
        bincode::serialize_into(file, &block).map_err(err_str)?;
        Ok(block)
    };

    match timestamp::modified_since_unix(&path) {
        Ok(modified) => read_cache(modified)?.map_or_else(update_cache, Result::Ok),
        Err(_) => update_cache()
    }
}

fn extract<P: AsRef<Path>>(digest: &str, file: &str, path: P) -> Result<(), String> {
    let cache = download::Cache::new(config::CACHE, None)?;

    let manifest_json = cache.object(&digest)?;
    let manifest = serde_json::from_slice::<Manifest>(&manifest_json).map_err(|e| e.to_string())?;

    let data = {
        let digest = manifest.files.get(file).ok_or(format!("{} not found", file))?;
        cache.object(&digest)?
    };

    eprintln!("extracting {} to {}", file, path.as_ref().display());
    match util::extract(&data, &path) {
        Ok(()) => (),
        Err(err) => {
            return Err(format!("failed to extract {} to {}: {}", file, path.as_ref().display(), err));
        }
    }

    Ok(())
}

pub fn schedule(digest: &str) -> Result<(), String> {
    let firmware_id = firmware_id()?;

    if ! Path::new("/sys/firmware/efi").exists() {
        return Err(format!("must be run using UEFI boot"));
    }

    let updater_file = "system76-firmware-update.tar.xz";
    let firmware_file = format!("{}.tar.xz", firmware_id);
    let updater_dir = Path::new("/boot/efi/system76-firmware-update");

    boot::unset_next_boot()?;

    remove_dir(&updater_dir)?;

    let updater_tmp = match tempdir::TempDir::new_in("/boot/efi", "system76-firmware-update") {
        Ok(ok) => ok,
        Err(err) => {
            return Err(format!("failed to create temporary directory: {}", err));
        }
    };

    extract(digest, updater_file, updater_tmp.path())?;

    extract(digest, &firmware_file, &updater_tmp.path().join("firmware"))?;

    let updater_tmp_dir = updater_tmp.into_path();
    eprintln!("moving {} to {}", updater_tmp_dir.display(), updater_dir.display());
    match fs::rename(&updater_tmp_dir, &updater_dir) {
        Ok(()) => (),
        Err(err) => {
            let _ = remove_dir(&updater_tmp_dir);
            return Err(format!("failed to move {} to {}: {}", updater_tmp_dir.display(), updater_dir.display(), err));
        }
    }

    boot::set_next_boot()?;

    eprintln!("Firmware update scheduled. Reboot your machine to install.");

    Ok(())
}

pub fn unschedule() -> Result<(), String> {
    let updater_dir = Path::new("/boot/efi/system76-firmware-update");

    boot::unset_next_boot()?;

    remove_dir(&updater_dir)?;

    eprintln!("Firmware update cancelled.");

    Ok(())
}

mod timestamp {
    use std::{io, path::Path, time::{Duration, SystemTime}};

    /// Convenience function for fetching the current time in seconds since the UNIX Epoch.
    pub fn current() -> u64 {
        seconds_since_unix(SystemTime::now())
    }

    pub fn modified_since_unix(path: &Path) -> io::Result<u64> {
        path.metadata()
            .and_then(|md| md.modified())
            .map(seconds_since_unix)
    }

    pub fn seconds_since_unix(time: SystemTime) -> u64 {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .as_ref()
            .map(Duration::as_secs)
            .unwrap_or(0)
    }

    pub fn exceeded(last: u64, current: u64, limit: u64) -> bool {
        current == 0 || last > current || current - last > limit
    }
}
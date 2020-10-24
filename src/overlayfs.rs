use crate::common;
use anyhow::{anyhow, Result};
//use log::debug;
use libmount::{mountinfo::Parser, Overlay};
use nix::mount::{umount2, MntFlags};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{ FileTypeExt, MetadataExt, PermissionsExt };
use std::path::{Path, PathBuf};

pub trait LayerManager {
    /// Return the name of the layer manager, e.g. "overlay".
    /// This name should be the same as the fs_type listed in the /proc/<>/mountinfo file
    fn name() -> String
    where
        Self: Sized;
    /// Create a new layer manager from the given distribution directory
    /// dist: distribution directory, inst: instance name (not directory)
    fn from_inst_dir<P: AsRef<Path>>(
        dist_path: P,
        inst_path: P,
        inst_name: P,
    ) -> Result<Box<dyn LayerManager>>
    where
        Self: Sized;
    /// Mount the filesystem to the given path
    fn mount(&mut self, to: &Path) -> Result<()>;
    /// Return if the filesystem is mounted
    fn is_mounted(&self, target: &Path) -> Result<bool>;
    /// Rollback the filesystem to the distribution state
    fn rollback(&mut self) -> Result<()>;
    /// Commit the current state of the instance filesystem to the distribution state
    fn commit(&mut self) -> Result<()>;
    /// Un-mount the filesystem
    fn unmount(&mut self, target: &Path) -> Result<()>;
    /// Return the directory where the configuration layer is located
    /// You may temporary mount this directory if your backend does not expose this directory directly
    fn get_config_layer(&mut self) -> Result<PathBuf>;
    /// Return the directory where the base layer is located
    fn get_base_layer(&mut self) -> Result<PathBuf>;
    /// Destroy the filesystem of the current instance
    fn destroy(&mut self) -> Result<()>;
}

struct OverlayFS {
    inst: PathBuf,
    base: PathBuf,
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
}

pub fn create_new_instance_fs<P: AsRef<Path>>(inst_path: P, inst_name: P) -> Result<()> {
    let inst = inst_path.as_ref().join(inst_name.as_ref());
    fs::create_dir_all(&inst)?;
    Ok(())
}

enum Diff {
    Symlink(PathBuf),
    OverrideDir(PathBuf),
    RenamedDir(PathBuf, PathBuf),
    NewDir(PathBuf),
    ModifiedDir(PathBuf), // Modify permission only
    WhiteoutFile(PathBuf), // Dir or File
    File(PathBuf), // Simple modified or new file
}

impl OverlayFS {
    /// Generate a list of changes made in the upper layer
    fn diff(&self) -> Result<Vec<Diff>> {
        let mut mods: Vec<Diff> = Vec::new();

        for entry in walkdir::WalkDir::new(&self.upper).into_iter().skip(1) { // SKip the root
            let path: PathBuf = entry?.path().to_path_buf();
            let rel_path = path.strip_prefix(&self.upper)?.to_path_buf();
            let lower_path = self.lower.join(&rel_path).to_path_buf();

            let meta = fs::symlink_metadata(&path)?;
            let file_type = meta.file_type();

            if file_type.is_symlink() {
                // Just move the symlink
                mods.push(Diff::Symlink(path.clone()));
            } else if meta.is_dir() { // Deal with dirs 
                let opaque = xattr::get(&path, "trusted.overlay.opaque")?;
                let redirect = xattr::get(&path, "trusted.overlay.redirect")?;

                if let Some(text) = opaque { // the new dir (completely) replace the old one
                    let msg = String::from_utf8(text)?;
                    if msg == "y" { // Delete corresponding dir
                        mods.push(Diff::OverrideDir(rel_path.clone()));
                    }
                } else if let Some(from_utf8) = redirect { // Renamed
                    let from = String::from_utf8(from_utf8)?;
                    let mut from_rel_path = PathBuf::from(&from);
                    if from_rel_path.is_absolute() { // abs path from root of OverlayFS
                        from_rel_path = from_rel_path.strip_prefix("/")?.to_path_buf();
                    } else { // rel path, same parent dir as the origin
                        let mut from_path = path.clone();
                        from_path.pop();
                        from_path.push(PathBuf::from(&from_rel_path));
                        from_rel_path = from_path.strip_prefix(&self.upper)?.to_path_buf();
                    }
                    mods.push(Diff::RenamedDir(from_rel_path, rel_path));
                } else if !lower_path.is_dir() { // New dir
                    mods.push(Diff::NewDir(rel_path.clone()));
                } else { // Modified
                    mods.push(Diff::ModifiedDir(rel_path.clone()));
                }
            } else { // Deal with files
                if file_type.is_char_device() && meta.rdev() == 0 { // Whiteout file!
                    mods.push(Diff::WhiteoutFile(rel_path.clone()));
                } else {
                    mods.push(Diff::File(rel_path.clone()));
                }
            }
        }
       
        Ok(mods)
    }
}

impl LayerManager for OverlayFS {
    fn name() -> String
    where
        Self: Sized,
    {
        "overlay".to_owned()
    }
    // The overlayfs structure inherited from older CIEL looks like this:
    // |- work: .ciel/container/instances/<inst_name>/diff.tmp/
    // |- upper: .ciel/container/instances/<inst_name>/diff/
    // |- lower: .ciel/container/instances/<inst_name>/local/
    // ||- lower (base): .ciel/container/dist/
    fn from_inst_dir<P: AsRef<Path>>(
        dist_path: P,
        inst_path: P,
        inst_name: P,
    ) -> Result<Box<dyn LayerManager>>
    where
        Self: Sized,
    {
        let dist = dist_path.as_ref();
        let inst = inst_path.as_ref().join(inst_name.as_ref());
        Ok(Box::new(OverlayFS {
            inst: inst.to_owned(),
            base: dist.to_owned(),
            lower: inst.join("layers/local"),
            upper: inst.join("layers/diff"),
            work: inst.join("layers/diff.tmp"),
        }))
    }
    fn mount(&mut self, to: &Path) -> Result<()> {
        let base_dirs = [self.lower.clone(), self.base.clone()];
        let overlay = Overlay::writable(
            // base_dirs variable contains the base and lower directories
            base_dirs.iter().map(|x| x.as_ref()),
            self.upper.clone(),
            self.work.clone(),
            to,
        );
        // create the directories if they don't exist (work directory may be missing)
        fs::create_dir_all(&self.work)?;
        fs::create_dir_all(&self.upper)?;
        fs::create_dir_all(&self.lower)?;
        // let's mount them
        overlay
            .mount()
            .or_else(|e| Err(anyhow!("{}", e.to_string())))?;

        Ok(())
    }

    /// is_mounted: check if a path is a mountpoint with corresponding fs_type
    fn is_mounted(&self, target: &Path) -> Result<bool> {
        is_mounted(target, &OsStr::new("overlay"))
    }

    fn commit(&mut self) -> Result<()> {
        let mods = self.diff()?;
        for i in mods {
            match i {
                Diff::Symlink(path) => {
                    let lower_path = self.lower.join(&path).to_path_buf();
                    fs::rename(&path, &lower_path)?;
                },
                Diff::OverrideDir(path) => {
                    let upper_path = self.upper.join(&path).to_path_buf();
                    let lower_path = self.lower.join(&path).to_path_buf();
                    // Replace lower dir with upper
                    fs::rename(&upper_path, &lower_path)?;
                },
                Diff::RenamedDir(from, to) => {
                    // TODO: test me
                    // It is unknown if such dir will include any files, so this
                    // section need more testing
                    let from_path = self.lower.join(&from).to_path_buf();
                    let to_path = self.lower.join(&to).to_path_buf();
                    // Replace lower dir with upper
                    fs::rename(&from_path, &to_path)?;
                },
                Diff::NewDir(path) => {
                    let lower_path = self.lower.join(&path).to_path_buf();
                    // Construct lower path
                    // All preceeding path should be created by previous iteration
                    // So create_dir should be enough
                    fs::create_dir(&lower_path)?;
                },
                Diff::ModifiedDir(path) => {
                    // Do nothing, just sync permission
                    let upper_path = self.upper.join(&path).to_path_buf();
                    let lower_path = self.lower.join(&path).to_path_buf();
                    sync_permission(&upper_path, &lower_path)?;
                },
                Diff::WhiteoutFile(path) => {
                    let lower_path = self.lower.join(&path).to_path_buf();
                    if lower_path.is_dir() {
                        fs::remove_dir_all(&lower_path)?;
                    } else {
                        fs::remove_file(&lower_path)?;
                    }
                },
                Diff::File(path) => {
                    let upper_path = self.upper.join(&path).to_path_buf();
                    let lower_path = self.lower.join(&path).to_path_buf();
                    // Move upper file to overwrite the lower
                    fs::rename(&upper_path, &lower_path)?;
                    // Sync permission
                    sync_permission(&upper_path, &lower_path)?;
                }
            }
        }

        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        fs::remove_dir_all(&self.upper)?;
        fs::remove_dir_all(&self.work)?;
        fs::create_dir(&self.upper)?;
        fs::create_dir(&self.work)?;

        Ok(())
    }

    fn unmount(&mut self, target: &Path) -> Result<()> {
        umount2(target, MntFlags::MNT_DETACH)?;

        Ok(())
    }

    fn get_config_layer(&mut self) -> Result<PathBuf> {
        Ok(self.lower.clone())
    }

    fn get_base_layer(&mut self) -> Result<PathBuf> {
        Ok(self.base.clone())
    }

    fn destroy(&mut self) -> Result<()> {
        fs::remove_dir_all(&self.inst)?;

        Ok(())
    }
}

/// is_mounted: check if a path is a mountpoint with corresponding fs_type
pub(crate) fn is_mounted(mountpoint: &Path, fs_type: &OsStr) -> Result<bool> {
    let mountinfo_content: Vec<u8> = fs::read("/proc/self/mountinfo")?;
    let parser = Parser::new(&mountinfo_content);

    for mount in parser {
        let mount = mount?;
        if mount.mount_point == mountpoint && mount.fstype == fs_type {
            return Ok(true);
        }
    }

    Ok(false)
}

/// A convenience function for getting a overlayfs type LayerManager
pub(crate) fn get_overlayfs_manager(inst_name: &str) -> Result<Box<dyn LayerManager>> {
    OverlayFS::from_inst_dir(common::CIEL_DIST_DIR, common::CIEL_INST_DIR, inst_name)
}

/// Check if path have all specified prefixes (with order)
fn has_prefix(path: &Path, prefixs: &Vec<PathBuf>) -> bool {
    for prefix in prefixs {
        if path.strip_prefix(prefix).is_ok() {
            return true;
        }
    }

    false
}

/// Set permission of to according to from
fn sync_permission(from: &Path, to: &Path) -> Result<()> {
    let from_meta = fs::metadata(from)?;
    let to_meta = fs::metadata(to)?;

    if from_meta.mode() != to_meta.mode() {
        to_meta.permissions().set_mode(to_meta.mode());
    }
    Ok(())
}

// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use crate::err::{AnkiError, Result};
use crate::media::database::{MediaDatabaseContext, MediaEntry};
use crate::media::files::{
    mtime_as_i64, sha1_of_file, MEDIA_SYNC_FILESIZE_LIMIT, NONSYNCABLE_FILENAME,
};
use std::collections::HashMap;
use std::path::Path;
use std::time;

struct FilesystemEntry {
    fname: String,
    sha1: Option<[u8; 20]>,
    mtime: i64,
    is_new: bool,
}

pub(super) struct ChangeTracker<'a, F>
where
    F: FnMut(usize) -> bool,
{
    media_folder: &'a Path,
    progress_cb: F,
    checked: usize,
}

impl<F> ChangeTracker<'_, F>
where
    F: FnMut(usize) -> bool,
{
    pub(super) fn new(media_folder: &Path, progress: F) -> ChangeTracker<F> {
        ChangeTracker {
            media_folder,
            progress_cb: progress,
            checked: 0,
        }
    }

    fn fire_progress_cb(&mut self) -> Result<()> {
        if (self.progress_cb)(self.checked) {
            Ok(())
        } else {
            Err(AnkiError::Interrupted)
        }
    }

    pub(super) fn register_changes(&mut self, ctx: &mut MediaDatabaseContext) -> Result<()> {
        ctx.transact(|ctx| {
            // folder mtime unchanged?
            let dirmod = mtime_as_i64(self.media_folder)?;

            let mut meta = ctx.get_meta()?;
            if dirmod == meta.folder_mtime {
                return Ok(());
            } else {
                meta.folder_mtime = dirmod;
            }

            let mtimes = ctx.all_mtimes()?;
            self.checked += mtimes.len();
            self.fire_progress_cb()?;

            let (changed, removed) = self.media_folder_changes(mtimes)?;

            self.add_updated_entries(ctx, changed)?;
            self.remove_deleted_files(ctx, removed)?;

            ctx.set_meta(&meta)?;

            // unconditional fire at end of op for accurate counts
            self.fire_progress_cb()?;

            Ok(())
        })
    }

    /// Scan through the media folder, finding changes.
    /// Returns (added/changed files, removed files).
    ///
    /// Checks for invalid filenames and unicode normalization are deferred
    /// until syncing time, as we can't trust the entries previous Anki versions
    /// wrote are correct.
    fn media_folder_changes(
        &mut self,
        mut mtimes: HashMap<String, i64>,
    ) -> Result<(Vec<FilesystemEntry>, Vec<String>)> {
        let mut added_or_changed = vec![];

        // loop through on-disk files
        for dentry in self.media_folder.read_dir()? {
            let dentry = dentry?;

            // skip folders
            if dentry.file_type()?.is_dir() {
                continue;
            }

            // if the filename is not valid unicode, skip it
            let fname_os = dentry.file_name();
            let fname = match fname_os.to_str() {
                Some(s) => s,
                None => continue,
            };

            // ignore blacklisted files
            if NONSYNCABLE_FILENAME.is_match(fname) {
                continue;
            }

            // ignore large files
            let metadata = dentry.metadata()?;
            if metadata.len() > MEDIA_SYNC_FILESIZE_LIMIT as u64 {
                continue;
            }

            // remove from mtimes for later deletion tracking
            let previous_mtime = mtimes.remove(fname);

            // skip files that have not been modified
            let mtime = metadata
                .modified()?
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            if let Some(previous_mtime) = previous_mtime {
                if previous_mtime == mtime {
                    continue;
                }
            }

            // add entry to the list
            let sha1 = Some(sha1_of_file(&dentry.path())?);
            added_or_changed.push(FilesystemEntry {
                fname: fname.to_string(),
                sha1,
                mtime,
                is_new: previous_mtime.is_none(),
            });

            self.checked += 1;
            if self.checked % 10 == 0 {
                self.fire_progress_cb()?;
            }
        }

        // any remaining entries from the database have been deleted
        let removed: Vec<_> = mtimes.into_iter().map(|(k, _)| k).collect();

        Ok((added_or_changed, removed))
    }

    /// Add added/updated entries to the media DB.
    ///
    /// Skip files where the mod time differed, but checksums are the same.
    fn add_updated_entries(
        &mut self,
        ctx: &mut MediaDatabaseContext,
        entries: Vec<FilesystemEntry>,
    ) -> Result<()> {
        for fentry in entries {
            let mut sync_required = true;
            if !fentry.is_new {
                if let Some(db_entry) = ctx.get_entry(&fentry.fname)? {
                    if db_entry.sha1 == fentry.sha1 {
                        // mtime bumped but file contents are the same,
                        // so we can preserve the current updated flag.
                        // we still need to update the mtime however.
                        sync_required = db_entry.sync_required
                    }
                }
            };

            ctx.set_entry(&MediaEntry {
                fname: fentry.fname,
                sha1: fentry.sha1,
                mtime: fentry.mtime,
                sync_required,
            })?;

            self.checked += 1;
            if self.checked % 10 == 0 {
                self.fire_progress_cb()?;
            }
        }

        Ok(())
    }

    /// Remove deleted files from the media DB.
    fn remove_deleted_files(
        &mut self,
        ctx: &mut MediaDatabaseContext,
        removed: Vec<String>,
    ) -> Result<()> {
        for fname in removed {
            ctx.set_entry(&MediaEntry {
                fname,
                sha1: None,
                mtime: 0,
                sync_required: true,
            })?;

            self.checked += 1;
            if self.checked % 10 == 0 {
                self.fire_progress_cb()?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::err::Result;
    use crate::media::changetracker::ChangeTracker;
    use crate::media::database::MediaEntry;
    use crate::media::files::sha1_of_data;
    use crate::media::MediaManager;
    use std::path::Path;
    use std::time::Duration;
    use std::{fs, time};
    use tempfile::tempdir;

    // helper
    fn change_mtime(p: &Path) {
        let mtime = p.metadata().unwrap().modified().unwrap();
        let new_mtime = mtime - Duration::from_secs(3);
        let secs = new_mtime
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        utime::set_file_times(p, secs, secs).unwrap();
    }

    #[test]
    fn test_change_tracking() -> Result<()> {
        let dir = tempdir()?;
        let media_dir = dir.path().join("media");
        std::fs::create_dir(&media_dir)?;
        let media_db = dir.path().join("media.db");

        let mgr = MediaManager::new(&media_dir, media_db)?;
        let mut ctx = mgr.dbctx();

        assert_eq!(ctx.count()?, 0);

        // add a file and check it's picked up
        let f1 = media_dir.join("file.jpg");
        fs::write(&f1, "hello")?;

        change_mtime(&media_dir);

        let progress_cb = |_n| true;

        ChangeTracker::new(&mgr.media_folder, progress_cb).register_changes(&mut ctx)?;

        let mut entry = ctx.transact(|ctx| {
            assert_eq!(ctx.count()?, 1);
            assert!(!ctx.get_pending_uploads(1)?.is_empty());
            let mut entry = ctx.get_entry("file.jpg")?.unwrap();
            assert_eq!(
                entry,
                MediaEntry {
                    fname: "file.jpg".into(),
                    sha1: Some(sha1_of_data("hello".as_bytes())),
                    mtime: f1
                        .metadata()?
                        .modified()?
                        .duration_since(time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                    sync_required: true,
                }
            );

            // mark it as unmodified
            entry.sync_required = false;
            ctx.set_entry(&entry)?;
            assert!(ctx.get_pending_uploads(1)?.is_empty());

            // modify it
            fs::write(&f1, "hello1")?;
            change_mtime(&f1);

            change_mtime(&media_dir);

            Ok(entry)
        })?;

        ChangeTracker::new(&mgr.media_folder, progress_cb).register_changes(&mut ctx)?;

        ctx.transact(|ctx| {
            assert_eq!(ctx.count()?, 1);
            assert!(!ctx.get_pending_uploads(1)?.is_empty());
            assert_eq!(
                ctx.get_entry("file.jpg")?.unwrap(),
                MediaEntry {
                    fname: "file.jpg".into(),
                    sha1: Some(sha1_of_data("hello1".as_bytes())),
                    mtime: f1
                        .metadata()?
                        .modified()?
                        .duration_since(time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                    sync_required: true,
                }
            );

            // mark it as unmodified
            entry.sync_required = false;
            ctx.set_entry(&entry)?;
            assert!(ctx.get_pending_uploads(1)?.is_empty());

            Ok(())
        })?;

        // delete it
        fs::remove_file(&f1)?;

        change_mtime(&media_dir);

        ChangeTracker::new(&mgr.media_folder, progress_cb).register_changes(&mut ctx)?;

        assert_eq!(ctx.count()?, 0);
        assert!(!ctx.get_pending_uploads(1)?.is_empty());
        assert_eq!(
            ctx.get_entry("file.jpg")?.unwrap(),
            MediaEntry {
                fname: "file.jpg".into(),
                sha1: None,
                mtime: 0,
                sync_required: true,
            }
        );

        Ok(())
    }
}
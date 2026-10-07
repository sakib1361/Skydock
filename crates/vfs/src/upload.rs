//! Sending changed files to the provider and settling what comes back.

use std::sync::Arc;
use std::time::Duration;

use fuser::INodeNo;
use skydock_core::{Error, RemoteItem, UploadTarget};
use skydock_state::{Item, is_local_id};

use crate::fs::{Inner, UPLOAD_DELAY};
use crate::names::{conflict_name, device_name, is_transient};
use crate::pending::{hash_file, move_file};

pub(crate) enum Outcome {
    /// Nothing is left to upload for now.
    Settled,
    /// The file was written to while its upload ran; it has to go again.
    ChangedMeanwhile,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum UploadError {
    #[error("{name}: {source}")]
    Provider { name: String, source: Error },

    #[error("{name}: {source}")]
    Io {
        name: String,
        source: std::io::Error,
    },

    #[error("{0}: the state database could not be updated")]
    State(String),

    /// The provider stored something other than what this device holds.
    /// The local content is kept and sent again.
    #[error("{0}: the provider reports different content than was sent")]
    Mismatch(String),
}

/// How long to wait before trying a failed upload again: half a minute at
/// first, doubling to at most ten minutes.
pub(crate) fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs((30 << attempt.min(5)).min(600))
}

impl Inner {
    /// Pick up the changes a previous run did not get to upload, and clear
    /// out what no longer belongs to any.
    pub(crate) fn resume_uploads(self: &Arc<Self>) -> skydock_state::Result<()> {
        let mut store = self.store.lock().unwrap();
        let mut waiting = Vec::new();
        for change in store.dirty(&self.account)? {
            let content_kept = self.pending.path_for(&change.id).is_file();
            match store.item(&self.account, &change.id)? {
                Some(_) if content_kept => waiting.push(change.id),
                // Without the content there is nothing to send. A file the
                // provider never saw has nothing else to show either.
                Some(_) if is_local_id(&change.id) => store.remove(&self.account, &change.id)?,
                _ => store.clear_dirty(&self.account, &change.id)?,
            }
        }
        drop(store);
        self.pending.retain(waiting.iter().map(String::as_str));
        for id in waiting {
            let ino = self.ino_of(&id);
            self.files.lock().unwrap().dirty.insert(ino, 0);
            self.plan_upload(ino, UPLOAD_DELAY, 0);
        }
        Ok(())
    }

    /// Upload the changes to `ino`, if it has any and nobody is writing.
    /// The caller holds the item's busy lock.
    pub(crate) async fn upload(self: &Arc<Self>, ino: u64) -> Result<Outcome, UploadError> {
        let Ok(item) = self.item(INodeNo(ino)) else {
            return Ok(Outcome::Settled);
        };
        let writes = {
            let files = self.files.lock().unwrap();
            match files.dirty.get(&ino) {
                // Closing the file plans the upload again.
                Some(_) if files.has_writers(ino) => return Ok(Outcome::Settled),
                Some(writes) => *writes,
                None => return Ok(Outcome::Settled),
            }
        };
        if is_transient(&item.name) {
            return Ok(Outcome::Settled);
        }
        let name = item.name.clone();
        let state = |_| UploadError::State(name.clone());
        let (change, parent_id) = self
            .store(|store, account| {
                Ok((
                    store.dirty_entry(account, &item.id)?,
                    store.parent_id(account, &item.id)?,
                ))
            })
            .map_err(state)?;
        let (Some(change), Some(parent_id)) = (change, parent_id) else {
            return Ok(Outcome::Settled);
        };

        let path = self.pending.path_for(&item.id);
        let io = |source| UploadError::Io {
            name: name.clone(),
            source,
        };
        let (size, hash) = {
            let (path, kind) = (path.clone(), self.provider.hash_kind());
            tokio::task::spawn_blocking(move || hash_file(&path, kind))
                .await
                .map_err(std::io::Error::other)
                .flatten()
                .map_err(io)?
        };

        let provider = |source| UploadError::Provider {
            name: name.clone(),
            source,
        };
        let as_new = UploadTarget::New {
            parent_id: &parent_id,
            name: &item.name,
        };
        let target = match is_local_id(&item.id) {
            true => as_new,
            false => UploadTarget::Replace {
                item_id: &item.id,
                base_version: change.base_version.as_deref(),
                base_hash: change.base_hash.as_deref(),
            },
        };
        let (uploaded, kept_both) = match self.provider.upload(target, &path).await {
            Ok(uploaded) => (uploaded, false),
            // Someone else changed the file too. Theirs stays; this
            // device's content goes beside it under a name that says so.
            Err(Error::Conflict) => {
                let copy_name = conflict_name(&item.name, &device_name());
                let copy = UploadTarget::New {
                    parent_id: &parent_id,
                    name: &copy_name,
                };
                let uploaded = self.provider.upload(copy, &path).await.map_err(provider)?;
                (uploaded, true)
            }
            // Deleted remotely while it was being edited here: put it back.
            Err(error) if target != as_new && error.is_not_found() => {
                let uploaded = self
                    .provider
                    .upload(as_new, &path)
                    .await
                    .map_err(provider)?;
                (uploaded, false)
            }
            Err(error) => return Err(provider(error)),
        };
        let intact = uploaded.hash.as_deref().is_none_or(|remote| remote == hash)
            && uploaded.size.is_none_or(|remote| remote == size);

        // From here on no write may slip in between checking and settling.
        let mut files = self.files.lock().unwrap();
        let settled = intact && files.dirty.get(&ino) == Some(&writes);
        if kept_both {
            self.store(|store, account| {
                store.put(account, &uploaded)?;
                match settled {
                    // The file itself goes back to what the provider holds.
                    true => store.clear_dirty(account, &item.id),
                    false => Ok(()),
                }
            })
            .map_err(state)?;
            if settled {
                files.dirty.remove(&ino);
                move_file(&path, &self.cache.path_for(&as_item(&uploaded))).map_err(io)?;
            }
        } else {
            self.store(|store, account| store.replace(account, &item.id, &uploaded, !settled))
                .map_err(state)?;
            if uploaded.id != item.id {
                self.inodes.lock().unwrap().rebind(&item.id, &uploaded.id);
            }
            let current = as_item(&uploaded);
            if settled {
                // What was uploaded is now the cached copy of the new version.
                let cached = self.cache.path_for(&current);
                move_file(&path, &cached).map_err(io)?;
                files.dirty.remove(&ino);
                files.reopen(ino, &cached, Some(&current));
                self.cache.remove_older_copies(&current, &cached);
            } else if uploaded.id != item.id {
                let renamed = self.pending.path_for(&uploaded.id);
                std::fs::rename(&path, &renamed).map_err(io)?;
                files.reopen(ino, &renamed, None);
            }
        }
        match (settled, intact) {
            (true, _) => Ok(Outcome::Settled),
            (false, true) => Ok(Outcome::ChangedMeanwhile),
            (false, false) => Err(UploadError::Mismatch(name.clone())),
        }
    }
}

fn as_item(remote: &RemoteItem) -> Item {
    Item {
        id: remote.id.clone(),
        name: remote.name.clone(),
        is_folder: remote.is_folder,
        size: remote.size,
        version: remote.version.clone(),
        hash: remote.hash.clone(),
        modified: remote.modified.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_to_ten_minutes() {
        let seconds: Vec<_> = (0..7)
            .map(|attempt| retry_delay(attempt).as_secs())
            .collect();
        assert_eq!(seconds, [30, 60, 120, 240, 480, 600, 600]);
    }
}

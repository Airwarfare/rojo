use std::{
    fs,
    sync::{Arc, Mutex},
};

use crossbeam_channel::{select, Receiver, RecvError, Sender};
use jod_thread::JoinHandle;
use rbx_dom_weak::{RbxId, RbxValue};

use crate::{
    message_queue::MessageQueue,
    snapshot::{
        apply_patch_set, compute_patch_set, AppliedPatchSet, InstigatingSource, PatchSet, RojoTree,
    },
    snapshot_middleware::{snapshot_from_vfs, snapshot_project_node},
    vfs::{FsResultExt, Vfs, VfsEvent, VfsFetcher},
};

/// Owns the connection between Rojo's VFS and its DOM by holding onto another
/// thread that processes messages.
///
/// Consumers of ChangeProcessor, like ServeSession, are intended to communicate
/// with this object via channels.
///
/// ChangeProcessor expects to be the only writer to the RojoTree and Vfs
/// objects passed to it.
pub struct ChangeProcessor {
    /// Controls the runtime of the processor thread. When signaled, the job
    /// thread will finish its current work and terminate.
    ///
    /// This channel should be signaled before dropping ChangeProcessor or we'll
    /// hang forever waiting for the message processing loop to terminate.
    shutdown_sender: Sender<()>,

    /// A handle to the message processing thread. When dropped, we'll block
    /// until it's done.
    ///
    /// Allowed to be unused because dropping this value has side effects.
    #[allow(unused)]
    job_thread: JoinHandle<Result<(), RecvError>>,
}

impl ChangeProcessor {
    /// Spin up the ChangeProcessor, connecting it to the given tree, VFS, and
    /// outbound message queue.
    pub fn start<F: VfsFetcher + Send + Sync + 'static>(
        tree: Arc<Mutex<RojoTree>>,
        vfs: Arc<Vfs<F>>,
        message_queue: Arc<MessageQueue<AppliedPatchSet>>,
        tree_mutation_receiver: Receiver<PatchSet>,
    ) -> Self {
        let (shutdown_sender, shutdown_receiver) = crossbeam_channel::bounded(1);
        let vfs_receiver = vfs.change_receiver();
        let task = JobThreadContext {
            tree,
            vfs,
            message_queue,
        };

        let job_thread = jod_thread::Builder::new()
            .name("ChangeProcessor thread".to_owned())
            .spawn(move || {
                log::trace!("ChangeProcessor thread started");

                #[allow(
                    // Crossbeam's select macro generates code that Clippy doesn't like,
                    // and Clippy blames us for it.
                    clippy::drop_copy,

                    // Crossbeam uses 0 as *const _ and Clippy doesn't like that either,
                    // but this isn't our fault.
                    clippy::zero_ptr,
                )]
                loop {
                    select! {
                        recv(vfs_receiver) -> event => {
                            task.handle_vfs_event(event?);
                        },
                        recv(tree_mutation_receiver) -> patch_set => {
                            task.handle_tree_event(patch_set?);
                        },
                        recv(shutdown_receiver) -> _ => {
                            log::trace!("ChangeProcessor shutdown signal received...");
                            return Ok(());
                        },
                    }
                }
            })
            .expect("Could not start ChangeProcessor thread");

        Self {
            shutdown_sender,
            job_thread,
        }
    }
}

impl Drop for ChangeProcessor {
    fn drop(&mut self) {
        // Signal the job thread to start spinning down. Without this we'll hang
        // forever waiting for the thread to finish its infinite loop.
        let _ = self.shutdown_sender.send(());

        // After this function ends, the job thread will be joined. It might
        // block for a small period of time while it processes its last work.
    }
}

/// Contains all of the state needed to synchronize the DOM and VFS.
struct JobThreadContext<F> {
    /// A handle to the DOM we're managing.
    tree: Arc<Mutex<RojoTree>>,

    /// A handle to the VFS we're managing.
    vfs: Arc<Vfs<F>>,

    /// Whenever changes are applied to the DOM, we should push those changes
    /// into this message queue to inform any connected clients.
    message_queue: Arc<MessageQueue<AppliedPatchSet>>,
}

impl<F: VfsFetcher> JobThreadContext<F> {
    fn handle_vfs_event(&self, event: VfsEvent) {
        log::trace!("Vfs event: {:?}", event);

        // Update the VFS immediately with the event.
        self.vfs
            .commit_change(&event)
            .expect("Error applying VFS change");

        // For a given VFS event, we might have many changes to different parts
        // of the tree. Calculate and apply all of these changes.
        let applied_patches = {
            let mut tree = self.tree.lock().unwrap();
            let mut applied_patches = Vec::new();

            match event {
                VfsEvent::Created(path) | VfsEvent::Modified(path) | VfsEvent::Removed(path) => {
                    // Find the nearest ancestor to this path that has
                    // associated instances in the tree. This helps make sure
                    // that we handle additions correctly, especially if we
                    // receive events for descendants of a large tree being
                    // created all at once.
                    let mut current_path = path.as_path();
                    let affected_ids = loop {
                        let ids = tree.get_ids_at_path(&current_path);

                        log::trace!("Path {} affects IDs {:?}", current_path.display(), ids);

                        if !ids.is_empty() {
                            break ids.to_vec();
                        }

                        log::trace!("Trying parent path...");
                        match current_path.parent() {
                            Some(parent) => current_path = parent,
                            None => break Vec::new(),
                        }
                    };

                    for id in affected_ids {
                        if let Some(patch) = compute_and_apply_changes(&mut tree, &self.vfs, id) {
                            applied_patches.push(patch);
                        }
                    }
                }
            }

            applied_patches
        };

        // Notify anyone listening to the message queue about the changes we
        // just made.
        self.message_queue.push_messages(&applied_patches);
    }

    fn handle_tree_event(&self, patch_set: PatchSet) {
        log::trace!("Applying PatchSet from client: {:#?}", patch_set);

        let applied_patch = {
            let mut tree = self.tree.lock().unwrap();

            for &id in &patch_set.removed_instances {
                if let Some(instance) = tree.get_instance(id) {
                    if let Some(instigating_source) = &instance.metadata().instigating_source {
                        match instigating_source {
                            InstigatingSource::Path(path) => fs::remove_file(path).unwrap(),
                            InstigatingSource::ProjectNode(_, _, _) => {
                                log::warn!(
                                    "Cannot remove instance {}, it's from a project file",
                                    id
                                );
                            }
                        }
                    } else {
                        // TODO
                        log::warn!(
                            "Cannot remove instance {}, it is not an instigating source.",
                            id
                        );
                    }
                } else {
                    log::warn!("Cannot remove instance {}, it does not exist.", id);
                }
            }

            for update in &patch_set.updated_instances {
                let id = update.id;

                if let Some(instance) = tree.get_instance(id) {
                    if update.changed_name.is_some() {
                        log::warn!("Cannot rename instances yet.");
                    }

                    if update.changed_class_name.is_some() {
                        log::warn!("Cannot change ClassName yet.");
                    }

                    if update.changed_metadata.is_some() {
                        log::warn!("Cannot change metadata yet.");
                    }

                    for (key, changed_value) in &update.changed_properties {
                        if key == "Source" {
                            if let Some(instigating_source) =
                                &instance.metadata().instigating_source
                            {
                                match instigating_source {
                                    InstigatingSource::Path(path) => {
                                        if let Some(RbxValue::String { value }) = changed_value {
                                            fs::write(path, value).unwrap();
                                        } else {
                                            log::warn!("Cannot change Source to non-string value.");
                                        }
                                    }
                                    InstigatingSource::ProjectNode(_, _, _) => {
                                        log::warn!(
                                            "Cannot remove instance {}, it's from a project file",
                                            id
                                        );
                                    }
                                }
                            } else {
                                log::warn!(
                                    "Cannot update instance {}, it is not an instigating source.",
                                    id
                                );
                            }
                        } else {
                            log::warn!("Cannot change properties besides BaseScript.Source.");
                        }
                    }
                } else {
                    log::warn!("Cannot update instance {}, it does not exist.", id);
                }
            }

            apply_patch_set(&mut tree, patch_set)
        };

        self.message_queue.push_messages(&[applied_patch]);
    }
}

fn compute_and_apply_changes<F: VfsFetcher>(
    tree: &mut RojoTree,
    vfs: &Vfs<F>,
    id: RbxId,
) -> Option<AppliedPatchSet> {
    let metadata = tree
        .get_metadata(id)
        .expect("metadata missing for instance present in tree");

    let instigating_source = match &metadata.instigating_source {
        Some(path) => path,
        None => {
            log::warn!(
                "Instance {} did not have an instigating source, but was considered for an update.",
                id
            );
            log::warn!("This is a Rojo bug. Please file an issue!");

            return None;
        }
    };

    // How we process a file change event depends on what created this
    // file/folder in the first place.
    let applied_patch_set = match instigating_source {
        InstigatingSource::Path(path) => {
            let maybe_entry = vfs
                .get(path)
                .with_not_found()
                .expect("unexpected VFS error");

            match maybe_entry {
                Some(entry) => {
                    // Our instance was previously created from a path and
                    // that path still exists. We can generate a snapshot
                    // starting at that path and use it as the source for
                    // our patch.

                    let snapshot = snapshot_from_vfs(&metadata.context, &vfs, &entry)
                        .expect("snapshot failed")
                        .expect("snapshot did not return an instance");

                    let patch_set = compute_patch_set(&snapshot, &tree, id);
                    apply_patch_set(tree, patch_set)
                }
                None => {
                    // Our instance was previously created from a path, but
                    // that path no longer exists.
                    //
                    // We associate deleting the instigating file for an
                    // instance with deleting that instance.

                    let mut patch_set = PatchSet::new();
                    patch_set.removed_instances.push(id);

                    apply_patch_set(tree, patch_set)
                }
            }
        }
        InstigatingSource::ProjectNode(project_path, instance_name, project_node) => {
            // This instance is the direct subject of a project node. Since
            // there might be information associated with our instance from
            // the project file, we snapshot the entire project node again.

            let snapshot = snapshot_project_node(
                &metadata.context,
                &project_path,
                instance_name,
                project_node,
                &vfs,
            )
            .expect("snapshot failed")
            .expect("snapshot did not return an instance");

            let patch_set = compute_patch_set(&snapshot, &tree, id);
            apply_patch_set(tree, patch_set)
        }
    };

    Some(applied_patch_set)
}

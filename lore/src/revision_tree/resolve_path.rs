// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_resolve_path` — translate a UTF-8 path string to a
//! `NodeID` against the loaded revision tree. An empty path resolves to the
//! root node id. The verb does not touch disk.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::errors::StateErrors;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeResolvePathCompleteEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_resolve_path`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(resolve_path_local)]
pub struct LoreRevisionTreeResolvePathArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to resolve against
    pub handle: LoreRevisionTree,
    /// UTF-8 path relative to the tree root; empty resolves to the root node
    pub path: LoreString,
}

#[error_set]
enum ResolvePathError {
    InvalidArguments,
}

impl EventError for ResolvePathError {
    fn translated(&self) -> LoreError {
        match self {
            ResolvePathError::InvalidArguments(_) => LoreError::InvalidArguments,
            ResolvePathError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_resolve_complete(id: u64, node_id: NodeID, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeResolvePathComplete(LoreRevisionTreeResolvePathCompleteEventData {
        id,
        node_id,
        error_code,
    })
    .send();
}

/// Resolve a UTF-8 path against the loaded revision tree to a `NodeID`.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_RESOLVE_PATH_COMPLETE`
/// carrying the resolved node and `error_code = NONE` before `Complete {status: 0}`.
/// An empty path resolves to the root node. A path that does not resolve to a
/// node — because it does not exist or is not valid UTF-8 — completes with
/// `error_code = INVALID_ARGUMENTS`. The verb materializes no bytes to disk.
pub async fn resolve_path(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeResolvePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, resolve_path_local).await
}

async fn resolve_path_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeResolvePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        resolve_path,
        move || emit_resolve_complete(miss_id, INVALID_NODE, LoreErrorCode::InvalidArguments),
        async move |internal, args: LoreRevisionTreeResolvePathArgs| {
            let id = args.id;
            let Ok(path) = std::str::from_utf8(args.path.as_bytes()) else {
                emit_resolve_complete(id, INVALID_NODE, LoreErrorCode::InvalidArguments);
                return Err(ResolvePathError::from(InvalidArguments {
                    reason: "path is not valid UTF-8".into(),
                }));
            };

            if path.is_empty() {
                emit_resolve_complete(id, ROOT_NODE, LoreErrorCode::None);
                return Ok(());
            }

            match internal
                .state
                .find_node_link(internal.repository_context.clone(), path)
                .await
            {
                Ok(link) => {
                    emit_resolve_complete(id, link.node, LoreErrorCode::None);
                    Ok(())
                }
                Err(error) => {
                    let not_found = matches!(
                        error,
                        StateErrors::NotFound(_)
                            | StateErrors::NodeNotFound(_)
                            | StateErrors::LinkNotFound(_)
                            | StateErrors::RevisionNotFound(_)
                            | StateErrors::AddressNotFound(_)
                    );
                    if not_found {
                        emit_resolve_complete(id, INVALID_NODE, LoreErrorCode::InvalidArguments);
                        Err(ResolvePathError::from(InvalidArguments {
                            reason: "path does not resolve to a node".into(),
                        }))
                    } else {
                        emit_resolve_complete(id, INVALID_NODE, LoreErrorCode::Internal);
                        Err(ResolvePathError::internal_with_context(
                            error,
                            "State::find_node_link",
                        ))
                    }
                }
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Hash;
    use lore_base::types::Partition;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::handle::LoreRevisionTree;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        ResolvePathComplete(u64, NodeID, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeResolvePathComplete(data) => {
                    Self::ResolvePathComplete(data.id, data.node_id, data.error_code)
                }
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn resolve_outcome(events: &[CapturedEvent], id: u64) -> Option<(NodeID, LoreErrorCode)> {
        events.iter().find_map(|event| match event {
            CapturedEvent::ResolvePathComplete(event_id, node_id, error_code)
                if *event_id == id =>
            {
                Some((*node_id, *error_code))
            }
            _ => None,
        })
    }

    async fn load_handle(label: &str, repository: Partition) -> (LoreRevisionTree, u64) {
        let store = in_memory_for_tests(label).await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository,
                revision_hash: Hash::default(),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "load fixture must succeed");
        let id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
                _ => None,
            })
            .expect("load fixture must emit RevisionTreeLoaded");
        (LoreRevisionTree { handle_id: id }, store_handle.handle_id)
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn resolve_empty_path_returns_root() {
        let (handle, store_handle_id) =
            load_handle("resolve-empty", Partition::from([0x11u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 7,
                handle,
                path: LoreString::default(),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "resolving the empty path must succeed");
        let events = sink.lock().unwrap().clone();
        assert_eq!(
            resolve_outcome(&events, 7),
            Some((ROOT_NODE, LoreErrorCode::None)),
            "empty path must resolve to the root node, got {events:?}"
        );
        let complete_pos = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::Complete(_)))
            .expect("Complete must fire");
        let resolve_pos = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::ResolvePathComplete(..)))
            .expect("ResolvePathComplete must fire");
        assert!(
            resolve_pos < complete_pos,
            "ResolvePathComplete must fire before Complete, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_missing_path_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("resolve-missing", Partition::from([0x22u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 8,
                handle,
                path: LoreString::from_str("no/such/path"),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "resolving a missing path must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, error_code) =
            resolve_outcome(&events, 8).expect("ResolvePathComplete must fire for the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "a missing path must report InvalidArguments, got {events:?}"
        );
        assert_eq!(
            node_id, INVALID_NODE,
            "a failed resolve must report the invalid-node sentinel, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "missing path must complete with status=1, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_non_utf8_path_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("resolve-non-utf8", Partition::from([0x33u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 9,
                handle,
                path: LoreString::from_bytes(&[0xFF, 0xFE, 0xFD]),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "a non-UTF-8 path must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, error_code) =
            resolve_outcome(&events, 9).expect("ResolvePathComplete must fire for the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "a non-UTF-8 path must report InvalidArguments, got {events:?}"
        );
        assert_eq!(
            node_id, INVALID_NODE,
            "a failed resolve must report the invalid-node sentinel, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_path_on_unknown_handle_emits_resolve_complete_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 10,
                handle: LoreRevisionTree::INVALID,
                path: LoreString::default(),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "resolving against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, error_code) = resolve_outcome(&events, 10)
            .expect("a handle miss must still emit ResolvePathComplete carrying the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "a handle miss must report InvalidArguments, got {events:?}"
        );
        assert_eq!(
            node_id, INVALID_NODE,
            "a handle miss must report the invalid-node sentinel, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "a handle miss must complete with status=1, got {events:?}"
        );
    }
}

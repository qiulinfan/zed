//! Keeps the sidebar's threads of external agents in step with the agents' own session lists,
//! once the user has imported an agent's threads (see [`crate::thread_import`]).
//!
//! Every [`SYNC_INTERVAL`] a local agent panel lists the sessions of those agents. A session that
//! started or changed since the last sync and has no thread gets one in the main list; a thread
//! whose session went on elsewhere moves up to its last activity and takes the session's new
//! title; a thread whose session is gone from the list is deleted, and one whose session the
//! agent archived itself (omp's `omp gc`) is archived. Only the sidebar's rows change: the agents
//! never hear of it.

use std::time::{Duration, Instant};

use agent::ThreadStore;
use agent_client_protocol::schema::v2 as acp;
use agent_servers::AcpConnection;
use anyhow::{Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{App, AsyncApp, Global, Task, WeakEntity};
use project::AgentId;
use util::ResultExt as _;

use crate::{
    Agent,
    agent_connection_store::AgentConnectionStore,
    thread_import::{SessionByAgent, collect_all_sessions},
    thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths},
};

/// How often the threads follow the agents' session lists.
const SYNC_INTERVAL: Duration = Duration::from_secs(120);
/// The first sync lets the window settle instead of starting every agent at launch.
const FIRST_SYNC_DELAY: Duration = Duration::from_secs(10);

/// How far back the first, manual import reaches.
pub(crate) fn first_import_window() -> TimeDelta {
    TimeDelta::days(10)
}

fn synced_until_key(agent_id: &AgentId) -> String {
    format!("external-thread-sync-until:{}", agent_id.0)
}

/// When this agent's threads were last brought in step; `None` until its first import.
fn read_synced_until(agent_id: &AgentId, cx: &App) -> Option<DateTime<Utc>> {
    let value = KeyValueStore::global(cx)
        .read_kvp(&synced_until_key(agent_id))
        .log_err()??;
    DateTime::parse_from_rfc3339(&value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

pub(crate) fn write_synced_until(agent_id: &AgentId, at: DateTime<Utc>, cx: &App) {
    let db = KeyValueStore::global(cx);
    let key = synced_until_key(agent_id);
    db::write_and_log(cx, move || async move { db.write_kvp(key, at.to_rfc3339()).await });
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct SyncPlan {
    pub(crate) insert: Vec<ThreadMetadata>,
    /// Threads whose session went on outside Zed, moved up to its last activity.
    pub(crate) update: Vec<ThreadMetadata>,
    pub(crate) delete: Vec<ThreadId>,
    pub(crate) archive: Vec<ThreadId>,
}

/// What brings `threads` in step with `listing`, the complete session list of one agent taken at
/// `listed_at`. `synced_until` is when the previous sync listed: a session that has not changed
/// since has had its chance at a thread, so one the user deleted stays deleted. `is_open` names
/// the sessions a thread holds now, which are left alone.
pub(crate) fn plan_sync(
    listing: &SessionByAgent,
    threads: &[ThreadMetadata],
    synced_until: DateTime<Utc>,
    listed_at: DateTime<Utc>,
    is_open: impl Fn(&acp::SessionId) -> bool,
) -> SyncPlan {
    let mut plan = SyncPlan::default();

    let mut known: HashSet<acp::SessionId> = threads
        .iter()
        .filter_map(|thread| thread.session_id.clone())
        .collect();
    for session in &listing.sessions {
        let Some(folder_paths) = session.work_dirs.as_ref() else {
            continue;
        };
        let changed = session
            .updated_at
            .is_some_and(|updated_at| updated_at > synced_until);
        if !changed || !known.insert(session.session_id.clone()) {
            continue;
        }
        plan.insert.push(ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(session.session_id.clone()),
            agent_id: listing.agent_id.clone(),
            title: session.title.clone(),
            title_override: None,
            updated_at: session.updated_at.unwrap_or(listed_at),
            created_at: session.created_at,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(folder_paths),
            remote_connection: listing.remote_connection.clone(),
            archived: false,
        });
    }

    // A list without a single session more likely failed to read the store than found it empty.
    let can_delete = !listing.sessions.is_empty();
    let listed: HashMap<&acp::SessionId, &acp_thread::AgentSessionInfo> = listing
        .sessions
        .iter()
        .map(|session| (&session.session_id, session))
        .collect();
    for thread in threads {
        let Some(session_id) = &thread.session_id else {
            continue;
        };
        if thread.agent_id != listing.agent_id
            || thread.remote_connection != listing.remote_connection
            || thread.updated_at >= listed_at
            || is_open(session_id)
        {
            continue;
        }
        if let Some(session) = listed.get(session_id) {
            if let Some(updated) = went_on_elsewhere(thread, session) {
                plan.update.push(updated);
            }
            continue;
        }
        if listing.archived_session_ids.contains(session_id) {
            if !thread.archived {
                plan.archive.push(thread.thread_id);
            }
        } else if can_delete {
            plan.delete.push(thread.thread_id);
        }
    }

    plan
}

/// `thread` brought up to `session` when the session's last activity is newer than what the
/// sidebar orders the thread by (its last message sent in Zed, else its update time), or the
/// session has a title the thread lacks. Its archive state stays: an archive is the user's.
fn went_on_elsewhere(
    thread: &ThreadMetadata,
    session: &acp_thread::AgentSessionInfo,
) -> Option<ThreadMetadata> {
    let shown_at = thread.interacted_at.unwrap_or(thread.updated_at);
    let went_on = session.updated_at.filter(|updated_at| *updated_at > shown_at);
    let retitled = session.title.clone().filter(|title| thread.title.as_ref() != Some(title));
    if went_on.is_none() && retitled.is_none() {
        return None;
    }
    let mut updated = thread.clone();
    if let Some(at) = went_on {
        updated.updated_at = updated.updated_at.max(at);
        // The sidebar orders by the last message sent in Zed when there is one, so a message sent
        // elsewhere has to move that time too.
        if updated.interacted_at.is_some() {
            updated.interacted_at = Some(at);
        }
    }
    if let Some(title) = retitled {
        updated.title = Some(title);
    }
    Some(updated)
}

/// When each agent was last synced, by any panel: one sync an interval is enough for them all.
#[derive(Default)]
struct LastSyncs(HashMap<AgentId, Instant>);

impl Global for LastSyncs {}

/// Keeps the threads of the agents the store's project reaches in step, until the store goes.
pub(crate) fn run(connection_store: WeakEntity<AgentConnectionStore>, cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_SYNC_DELAY).await;
        loop {
            let Some(agent_ids) = cx.update(|cx| due_agents(&connection_store, cx)) else {
                break;
            };
            for agent_id in agent_ids {
                sync_agent(&connection_store, agent_id, cx).await.log_err();
            }
            cx.background_executor().timer(SYNC_INTERVAL).await;
        }
    })
}

/// The agents to sync now: those imported once and not synced this interval. `None` once the
/// store is gone; a remote project syncs nothing.
fn due_agents(
    connection_store: &WeakEntity<AgentConnectionStore>,
    cx: &mut App,
) -> Option<Vec<AgentId>> {
    let store = connection_store.upgrade()?;
    let project = store.read(cx).project().clone();
    if !project.read(cx).is_local() {
        return Some(Vec::new());
    }
    let imported: Vec<AgentId> = project
        .read(cx)
        .agent_server_store()
        .read(cx)
        .external_agents()
        .filter(|agent_id| read_synced_until(agent_id, cx).is_some())
        .cloned()
        .collect();

    let now = Instant::now();
    // A little slack, so panels whose timers drifted apart do not skip each other's turn.
    let interval = SYNC_INTERVAL - Duration::from_secs(5);
    let last_syncs = cx.default_global::<LastSyncs>();
    let due: Vec<AgentId> = imported
        .into_iter()
        .filter(|agent_id| {
            last_syncs
                .0
                .get(agent_id)
                .map_or(true, |at| now.duration_since(*at) >= interval)
        })
        .collect();
    for agent_id in &due {
        last_syncs.0.insert(agent_id.clone(), now);
    }
    Some(due)
}

async fn sync_agent(
    connection_store: &WeakEntity<AgentConnectionStore>,
    agent_id: AgentId,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Some((synced_until, connect)) = cx.update(|cx| {
        let synced_until = read_synced_until(&agent_id, cx)?;
        let store = connection_store.upgrade()?;
        let agent = Agent::from(agent_id.clone());
        let server = agent.server(<dyn Fs>::global(cx), ThreadStore::global(cx));
        let entry = store.update(cx, |store, cx| store.request_connection(agent, server, cx));
        Some((synced_until, entry.read(cx).wait_for_connection()))
    }) else {
        return Ok(());
    };
    let state = connect
        .await
        .map_err(|error| anyhow!("connecting to {agent_id}: {error}"))?;
    let Some(list) = cx.update(|cx| state.connection.session_list(cx)) else {
        return Ok(());
    };

    let listed_at = Utc::now();
    let listing = collect_all_sessions(agent_id.clone(), None, list, cx).await?;
    let connection = state.connection.clone().downcast::<AcpConnection>();

    cx.update(|cx| {
        let store = ThreadMetadataStore::global(cx);
        let threads: Vec<ThreadMetadata> = store.read(cx).entries().cloned().collect();
        let plan = plan_sync(&listing, &threads, synced_until, listed_at, |session_id| {
            connection
                .as_ref()
                .is_some_and(|connection| connection.has_loaded_session(session_id))
        });
        store.update(cx, |store, cx| {
            if !plan.insert.is_empty() || !plan.update.is_empty() {
                store.save_all(plan.insert.into_iter().chain(plan.update).collect(), cx);
            }
            for thread_id in plan.delete {
                store.delete(thread_id, cx);
            }
            for thread_id in plan.archive {
                store.archive(thread_id, None, cx);
            }
        });
        write_synced_until(&agent_id, listed_at, cx);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::AgentSessionInfo;
    use std::path::Path;
    use workspace::PathList;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(0, 0).unwrap() + TimeDelta::hours(hours)
    }

    fn session(id: &str, updated_at: i64) -> AgentSessionInfo {
        AgentSessionInfo {
            session_id: acp::SessionId::new(id),
            work_dirs: Some(PathList::new(&[Path::new("/project")])),
            title: Some(id.to_string().into()),
            updated_at: Some(at(updated_at)),
            created_at: None,
            meta: None,
        }
    }

    fn listing(sessions: Vec<AgentSessionInfo>, archived: &[&str]) -> SessionByAgent {
        SessionByAgent {
            agent_id: AgentId::new("omp"),
            remote_connection: None,
            sessions,
            archived_session_ids: archived.iter().map(|id| acp::SessionId::new(*id)).collect(),
        }
    }

    fn thread(agent: &str, session_id: Option<&str>, updated_at: i64) -> ThreadMetadata {
        ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: session_id.map(acp::SessionId::new),
            agent_id: AgentId::new(agent),
            title: None,
            title_override: None,
            updated_at: at(updated_at),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[Path::new(
                "/project",
            )])),
            remote_connection: None,
            archived: false,
        }
    }

    fn inserted_ids(plan: &SyncPlan) -> Vec<&str> {
        plan.insert
            .iter()
            .map(|thread| thread.session_id.as_ref().unwrap().0.as_ref())
            .collect()
    }

    #[test]
    fn adds_sessions_that_changed_since_the_last_sync_to_the_main_list() {
        let known = thread("omp", Some("known"), 1);
        let plan = plan_sync(
            &listing(
                vec![
                    session("new", 5),
                    session("known", 6),
                    session("unchanged", 2),
                ],
                &[],
            ),
            &[known],
            at(3),
            at(10),
            |_| false,
        );

        // "unchanged" had its chance at the last sync, so a thread deleted since stays deleted.
        assert_eq!(inserted_ids(&plan), ["new"]);
        assert!(!plan.insert[0].archived);
        assert!(plan.delete.is_empty() && plan.archive.is_empty());
    }

    #[test]
    fn deletes_threads_whose_session_is_gone_and_leaves_the_rest() {
        let gone = thread("omp", Some("gone"), 1);
        let open = thread("omp", Some("open"), 1);
        let other_agent = thread("claude-acp", Some("theirs"), 1);
        let draft = thread("omp", None, 1);
        let touched_while_listing = thread("omp", Some("fresh"), 10);
        let plan = plan_sync(
            &listing(vec![session("kept", 1)], &[]),
            &[
                gone.clone(),
                open,
                other_agent,
                draft,
                touched_while_listing,
            ],
            at(5),
            at(10),
            |session_id| session_id.0.as_ref() == "open",
        );

        assert_eq!(plan.delete, vec![gone.thread_id]);
        assert!(plan.archive.is_empty());
    }

    #[test]
    fn archives_threads_whose_session_the_agent_archived() {
        let archived_by_agent = thread("omp", Some("gc-archived"), 1);
        let mut already_archived = thread("omp", Some("also-archived"), 1);
        already_archived.archived = true;
        let plan = plan_sync(
            &listing(vec![session("kept", 1)], &["gc-archived", "also-archived"]),
            &[archived_by_agent.clone(), already_archived],
            at(5),
            at(10),
            |_| false,
        );

        assert_eq!(plan.archive, vec![archived_by_agent.thread_id]);
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn moves_up_threads_whose_session_went_on_elsewhere() {
        let mut sent_in_zed = thread("omp", Some("sent-in-zed"), 1);
        sent_in_zed.interacted_at = Some(at(2));
        let mut archived = thread("omp", Some("archived"), 1);
        archived.archived = true;
        let mut idle = thread("omp", Some("idle"), 4);
        idle.title = Some("idle".into());
        let plan = plan_sync(
            &listing(
                vec![session("sent-in-zed", 6), session("archived", 7), session("idle", 3)],
                &[],
            ),
            &[sent_in_zed.clone(), archived.clone(), idle],
            at(5),
            at(10),
            |_| false,
        );

        let by_id = |id: &str| {
            plan.update
                .iter()
                .find(|thread| thread.session_id.as_ref().unwrap().0.as_ref() == id)
                .unwrap()
        };
        // The sidebar orders by `interacted_at` when a message was sent in Zed, so both times move.
        assert_eq!(by_id("sent-in-zed").interacted_at, Some(at(6)));
        assert_eq!(by_id("sent-in-zed").updated_at, at(6));
        assert_eq!(by_id("sent-in-zed").thread_id, sent_in_zed.thread_id);
        // Moving up keeps the archive state; the title the session gained comes along.
        assert!(by_id("archived").archived);
        assert_eq!(by_id("archived").title.as_deref(), Some("archived"));
        // An idle session with nothing new and a title the thread already shows changes nothing.
        assert_eq!(plan.update.len(), 2);
        assert!(plan.insert.is_empty() && plan.delete.is_empty());
    }

    #[test]
    fn an_empty_list_deletes_nothing() {
        let plan = plan_sync(
            &listing(Vec::new(), &[]),
            &[thread("omp", Some("s1"), 1)],
            at(5),
            at(10),
            |_| false,
        );

        assert_eq!(plan, SyncPlan::default());
    }
}

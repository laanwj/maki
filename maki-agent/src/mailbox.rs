use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use maki_providers::Message;
use maki_storage::id::MakiId;
use thiserror::Error;

const MAILBOX_CAPACITY: usize = 100;
const SUBAGENT_MAILBOX_CAPACITY: usize = 100;

static MAILBOXES: LazyLock<Mutex<HashMap<MakiId, Weak<Mutex<State>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SUBAGENT_MAILBOXES: LazyLock<Mutex<HashMap<Arc<str>, flume::Sender<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct State {
    pending: VecDeque<Message>,
    wake: bool,
}

#[derive(Debug, Error)]
#[error("session not live: {0}")]
pub struct MailboxError(MakiId);

#[derive(Clone)]
pub struct SessionMailbox {
    session_id: MakiId,
    state: Arc<Mutex<State>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionMailbox {
    pub fn register(session_id: MakiId) -> Self {
        let mut mailboxes = lock(&MAILBOXES);
        if let Some(state) = mailboxes.get(&session_id).and_then(Weak::upgrade) {
            return Self { session_id, state };
        }

        let state = Arc::new(Mutex::new(State::default()));
        mailboxes.insert(session_id, Arc::downgrade(&state));
        Self { session_id, state }
    }

    pub fn notify(session_id: MakiId, text: String, wake: bool) -> Result<(), MailboxError> {
        let mailbox = {
            let mut mailboxes = lock(&MAILBOXES);
            let Some(state) = mailboxes.get(&session_id).and_then(Weak::upgrade) else {
                mailboxes.remove(&session_id);
                return Err(MailboxError(session_id));
            };
            Self { session_id, state }
        };
        let mut state = lock(&mailbox.state);
        if state.pending.len() == MAILBOX_CAPACITY {
            state.pending.pop_front();
        }
        state.pending.push_back(Message::observation(text));
        state.wake |= wake;
        Ok(())
    }

    pub fn drain(&self) -> Vec<Message> {
        let mut state = lock(&self.state);
        state.wake = false;
        state.pending.drain(..).collect()
    }

    pub fn claim_wake(&self) -> Vec<Message> {
        let mut state = lock(&self.state);
        if !state.wake {
            return Vec::new();
        }
        state.wake = false;
        state.pending.drain(..).collect()
    }
}

impl Drop for SessionMailbox {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) != 1 {
            return;
        }
        let weak = Arc::downgrade(&self.state);
        let mut mailboxes = lock(&MAILBOXES);
        if Arc::strong_count(&self.state) == 1
            && mailboxes
                .get(&self.session_id)
                .is_some_and(|registered| registered.ptr_eq(&weak))
        {
            mailboxes.remove(&self.session_id);
        }
    }
}

#[derive(Debug, Error)]
pub enum SubagentMailboxError {
    #[error("subagent is no longer running: {0}")]
    NotLive(Arc<str>),
    #[error("subagent follow-up queue is full: {0}")]
    Full(Arc<str>),
}

/// User follow-ups for a live subagent, keyed by its `tool_use_id`.
/// The subagent registers on creation and deregisters on close.
pub struct SubagentMailbox {
    tool_use_id: Arc<str>,
    tx: flume::Sender<String>,
    rx: flume::Receiver<String>,
}

impl SubagentMailbox {
    pub fn register(tool_use_id: Arc<str>) -> Self {
        let (tx, rx) = flume::bounded(SUBAGENT_MAILBOX_CAPACITY);
        lock(&SUBAGENT_MAILBOXES).insert(Arc::clone(&tool_use_id), tx.clone());
        Self {
            tool_use_id,
            tx,
            rx,
        }
    }

    pub fn send(tool_use_id: &str, text: String) -> Result<(), SubagentMailboxError> {
        let id = || Arc::from(tool_use_id);
        let tx = lock(&SUBAGENT_MAILBOXES)
            .get(tool_use_id)
            .cloned()
            .ok_or_else(|| SubagentMailboxError::NotLive(id()))?;
        // The receiver can die between the lookup and the send when the
        // session closes concurrently; that is NotLive, not a full queue.
        tx.try_send(text).map_err(|e| match e {
            flume::TrySendError::Full(_) => SubagentMailboxError::Full(id()),
            flume::TrySendError::Disconnected(_) => SubagentMailboxError::NotLive(id()),
        })
    }

    /// Whether a live subagent session holds this id's mailbox. The UI uses it
    /// to keep routing and chat state for conversations that outlive the run
    /// that spawned them.
    pub fn is_live(tool_use_id: &str) -> bool {
        lock(&SUBAGENT_MAILBOXES).contains_key(tool_use_id)
    }

    pub async fn recv(&self) -> Option<String> {
        self.rx.recv_async().await.ok()
    }

    /// A clone of the receiving end, so a waiter can listen without holding
    /// the mailbox's owner lock.
    pub fn receiver(&self) -> flume::Receiver<String> {
        self.rx.clone()
    }
}

impl Drop for SubagentMailbox {
    fn drop(&mut self) {
        let mut mailboxes = lock(&SUBAGENT_MAILBOXES);
        if mailboxes
            .get(&self.tool_use_id)
            .is_some_and(|tx| tx.same_channel(&self.tx))
        {
            mailboxes.remove(&self.tool_use_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &Message) -> &str {
        message.user_text().unwrap()
    }

    #[test]
    fn notifications_drain_in_order_and_clear_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "first".into(), true).unwrap();
        SessionMailbox::notify(id, "second".into(), true).unwrap();

        let messages = mailbox.drain();
        assert_eq!(
            messages.iter().map(text).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(messages.iter().all(Message::is_observation));
        assert!(mailbox.claim_wake().is_empty());
    }

    #[test]
    fn quiet_notifications_do_not_claim_a_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "built".into(), false).unwrap();

        assert!(mailbox.claim_wake().is_empty());
        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn waking_notification_claims_all_pending_messages() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "quiet".into(), false).unwrap();
        SessionMailbox::notify(id, "wake".into(), true).unwrap();

        let messages = mailbox.claim_wake();
        assert_eq!(
            messages.iter().map(text).collect::<Vec<_>>(),
            ["quiet", "wake"]
        );
        assert!(mailbox.drain().is_empty());
    }

    #[test]
    fn notifications_drop_the_oldest_message_at_capacity() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        for index in 0..=MAILBOX_CAPACITY {
            SessionMailbox::notify(id, index.to_string(), false).unwrap();
        }

        let messages = mailbox.drain();
        assert_eq!(messages.len(), MAILBOX_CAPACITY);
        assert_eq!(text(&messages[0]), "1");
        assert_eq!(text(messages.last().unwrap()), MAILBOX_CAPACITY.to_string());
    }

    #[test]
    fn registrations_for_the_same_id_share_state() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);
        SessionMailbox::notify(id, "built".into(), false).unwrap();

        assert_eq!(second.drain().len(), 1);
        assert!(first.drain().is_empty());
    }

    #[test]
    fn dropping_the_last_registration_closes_the_mailbox() {
        let id = MakiId::generate();
        drop(SessionMailbox::register(id));

        assert!(!lock(&MAILBOXES).contains_key(&id));
        assert!(SessionMailbox::notify(id, "late".into(), false).is_err());
    }

    #[test]
    fn dropping_one_registration_keeps_the_shared_mailbox() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);

        drop(first);

        assert!(lock(&MAILBOXES).contains_key(&id));
        SessionMailbox::notify(id, "built".into(), false).unwrap();
        assert_eq!(second.drain().len(), 1);
    }

    #[test]
    fn stale_drop_does_not_remove_a_replacement() {
        let id = MakiId::generate();
        let stale = SessionMailbox::register(id);
        let replacement = SessionMailbox {
            session_id: id,
            state: Arc::new(Mutex::new(State::default())),
        };
        lock(&MAILBOXES).insert(id, Arc::downgrade(&replacement.state));

        drop(stale);
        SessionMailbox::notify(id, "built".into(), false).unwrap();

        assert_eq!(replacement.drain().len(), 1);
    }

    #[test]
    fn legacy_and_canonical_ids_address_the_same_mailbox() {
        let legacy: MakiId = "01965087-4c71-7f00-8000-000000000001".parse().unwrap();
        let canonical: MakiId = legacy.to_string().parse().unwrap();
        let mailbox = SessionMailbox::register(legacy);
        SessionMailbox::notify(canonical, "built".into(), false).unwrap();

        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn subagent_followups_arrive_in_order() {
        smol::block_on(async {
            let id: Arc<str> = Arc::from("toolu_sub");
            let mailbox = SubagentMailbox::register(Arc::clone(&id));
            SubagentMailbox::send(&id, "first".into()).unwrap();
            SubagentMailbox::send(&id, "second".into()).unwrap();

            assert_eq!(mailbox.recv().await.as_deref(), Some("first"));
            assert_eq!(mailbox.recv().await.as_deref(), Some("second"));
        });
    }

    #[test]
    fn subagent_send_fails_when_unregistered() {
        assert!(SubagentMailbox::send("toolu_missing", "late".into()).is_err());
    }

    #[test]
    fn dropping_subagent_mailbox_deregisters_it() {
        let id: Arc<str> = Arc::from("toolu_gone");
        drop(SubagentMailbox::register(Arc::clone(&id)));

        assert!(SubagentMailbox::send(&id, "late".into()).is_err());
        assert!(!lock(&SUBAGENT_MAILBOXES).contains_key(&id));
    }

    #[test]
    fn dropping_stale_subagent_mailbox_keeps_replacement() {
        let id: Arc<str> = Arc::from("toolu_stale");
        let stale = SubagentMailbox::register(Arc::clone(&id));
        let replacement = SubagentMailbox::register(Arc::clone(&id));

        drop(stale);

        SubagentMailbox::send(&id, "kept".into()).unwrap();
        assert_eq!(smol::block_on(replacement.recv()).as_deref(), Some("kept"));
    }
}

//! A task is a subagent chat, addressed by its `tool_use_id`. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `maki.session.live()` already reports that.
//!
//! Both `maki.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use std::sync::Arc;

use maki_agent::tools::MAIN_TASK_ID;
use serde::Serialize;

use crate::app::App;
use crate::components::DisplayRole;

pub(crate) const UNKNOWN_TASK_ERR: &str = "unknown task: ";

/// How a chat ended, from the vaguest to the most specific. `SubagentHistory`
/// only sees the transcript close, and the `ToolDone` carrying `is_error`
/// lands after it, so [`Self::Unknown`] holds the place until then.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TaskOutcome {
    /// Ended, and nobody said how. Reads as done unless a verdict follows.
    Unknown,
    Done,
    Error,
}

impl TaskOutcome {
    /// Only the placeholder gives way, so a late event cannot walk a finished
    /// task back to another ending.
    pub(crate) fn refines(self, previous: Self) -> bool {
        previous == Self::Unknown && self != Self::Unknown
    }

    /// The bubble that ends the transcript. Derived from the outcome rather
    /// than passed beside it, so a green marker cannot sit on a failed task.
    pub(crate) fn role(self) -> DisplayRole {
        match self {
            Self::Error => DisplayRole::Error,
            Self::Unknown | Self::Done => DisplayRole::Done,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskStatus {
    Working,
    Done,
    Error,
}

impl From<Option<TaskOutcome>> for TaskStatus {
    fn from(outcome: Option<TaskOutcome>) -> Self {
        match outcome {
            None => Self::Working,
            Some(TaskOutcome::Error) => Self::Error,
            Some(TaskOutcome::Unknown | TaskOutcome::Done) => Self::Done,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskState<'a> {
    pub(crate) id: &'a Arc<str>,
    pub(crate) name: &'a str,
    pub(crate) status: TaskStatus,
}

/// The wire shape of `maki.task.list()`. The main chat leaves `status` unset.
#[derive(Serialize)]
pub(crate) struct TaskInfo {
    id: Arc<str>,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<TaskStatus>,
    focused: bool,
}

impl App {
    /// Subagent chats only, in chat order.
    pub(crate) fn task_states(&self) -> impl Iterator<Item = TaskState<'_>> {
        self.chats.iter().filter_map(|chat| {
            Some(TaskState {
                id: chat.task_id()?,
                name: &chat.name,
                status: chat.task_status(),
            })
        })
    }

    /// The same walk widened to every chat, so the main chat at index 0 lands
    /// first on its own and no extra code path has to place it.
    pub(crate) fn tasks(&self) -> Vec<TaskInfo> {
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| TaskInfo {
                id: chat.task_id_or_main(),
                name: chat.name.clone(),
                status: chat.task_id().map(|_| chat.task_status()),
                focused: idx == self.active_chat,
            })
            .collect()
    }

    /// The task whose transcript is on screen, in the ids `maki.task` uses.
    pub(crate) fn active_task_id(&self) -> Arc<str> {
        self.chats[self.active_chat].task_id_or_main()
    }

    /// The only writer of `active_chat` outside the chat cycling keys. Tasks
    /// are looked up by id, never by position and never through `chat_index`,
    /// a routing cache wiped at the end of every turn.
    pub(crate) fn focus_task(&mut self, id: &str) -> Result<(), String> {
        let idx = if id == MAIN_TASK_ID {
            0
        } else {
            self.chats
                .iter()
                .position(|chat| chat.task_id().is_some_and(|task_id| &**task_id == id))
                .ok_or_else(|| format!("{UNKNOWN_TASK_ERR}{id}"))?
        };
        self.focus_chat(idx);
        Ok(())
    }
}

/// Fires when a known task changes status, or when a new one shows up working.
/// A task first seen already finished is recorded quietly, so a restored
/// session does not replay yesterday's tasks.
///
/// `previous` is keyed by id, because a session reset reuses positions.
pub(crate) fn diff_task_states<'a>(
    previous: &mut Vec<(Arc<str>, TaskStatus)>,
    current: impl Iterator<Item = TaskState<'a>>,
    mut emit: impl FnMut(TaskState<'a>),
) {
    let mut next = Vec::with_capacity(previous.len());
    for task in current {
        let announce = match previous.iter().find(|(id, _)| id == task.id) {
            Some((_, status)) => *status != task.status,
            None => task.status == TaskStatus::Working,
        };
        if announce {
            emit(task);
        }
        next.push((Arc::clone(task.id), task.status));
    }
    *previous = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Msg;
    use crate::app::tests::{
        RESEARCH_NAME, agent_msg_with_run_id, app_with_subagent_id, cancel_app,
        close_subagent_transcript, end_turn, error_app, finish_subagent, start_subagent,
        subagent_msg, subagent_msg_with_run_id, tool_start,
    };
    use crate::chat::{DONE_TEXT, ERROR_TEXT};
    use crate::components::DisplayRole;
    use crate::components::key;
    use crossterm::event::KeyCode;
    use maki_agent::{AgentEvent, ImageMediaType, ImageSource};
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";
    const OTHER_ID: &str = "toolu_02";
    const MISSING_ID: &str = "toolu_nope";
    const BUILD_NAME: &str = "build";
    const UNCHANGED_CHAT: usize = 2;

    fn app_with_two_subagents() -> App {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        app
    }

    /// Restores the two subagents in the reverse of the order their ids
    /// suggest, so a lookup guessing a position from an id lands on the wrong
    /// transcript instead of being right by accident.
    fn restored_app_with_two_subagents() -> App {
        let mut app = app_with_subagent_id(OTHER_ID);
        start_subagent(&mut app, TASK_ID, BUILD_NAME);
        // Only a subagent that handed over its transcript survives a reload,
        // so both have to close before the session is written.
        close_subagent_transcript(&mut app, OTHER_ID);
        close_subagent_transcript(&mut app, TASK_ID);
        app.reset_ui_chrome();
        app.restore_display();
        app
    }

    fn state(id: &Arc<str>, status: TaskStatus) -> TaskState<'_> {
        TaskState {
            id,
            name: "task",
            status,
        }
    }

    fn collect(
        previous: &mut Vec<(Arc<str>, TaskStatus)>,
        current: &[(Arc<str>, TaskStatus)],
    ) -> Vec<(String, TaskStatus)> {
        let mut fired = Vec::new();
        diff_task_states(
            previous,
            current.iter().map(|(id, status)| state(id, *status)),
            |task| fired.push((task.id.to_string(), task.status)),
        );
        fired
    }

    /// Identity lives on the chat, not in `chat_index`: that one is wiped at
    /// the end of every turn, while the picker keeps addressing tasks long
    /// after.
    #[test_case(MAIN_TASK_ID, Some(0) ; "main")]
    #[test_case(TASK_ID, Some(1)      ; "subagent")]
    #[test_case(MISSING_ID, None      ; "unknown_id")]
    fn focus_addresses_a_task_by_id_alone(id: &str, expected: Option<usize>) {
        let mut app = app_with_two_subagents();
        app.focus_task(OTHER_ID).unwrap();
        assert_eq!(app.active_chat, UNCHANGED_CHAT);
        app.chat_index.clear();

        let err = app.focus_task(id).err();

        assert_eq!(
            err,
            expected
                .is_none()
                .then(|| format!("{UNKNOWN_TASK_ERR}{id}"))
        );
        assert_eq!(app.active_chat, expected.unwrap_or(UNCHANGED_CHAT));
    }

    /// A reload rebuilds the chats from scratch, so the id is all the picker
    /// has left to aim with.
    #[test_case(OTHER_ID, RESEARCH_NAME, 1 ; "first_restored")]
    #[test_case(TASK_ID, BUILD_NAME, 2     ; "second_restored")]
    fn focus_addresses_a_restored_task_by_id(id: &str, name: &str, expected: usize) {
        let mut app = restored_app_with_two_subagents();
        app.focus_task(id).unwrap();

        assert_eq!(app.active_chat, expected);
        let chat = &app.chats[app.active_chat];
        assert_eq!(chat.task_id().map(|task_id| &**task_id), Some(id));
        assert_eq!(chat.name, name);
    }

    /// A subagent reaches disk when it spawns but its transcript only when it
    /// ends, so quitting mid-turn strands one with no messages. Restoring it
    /// would pin a task no agent backs at the top of the picker, running
    /// forever. The reload drops it instead.
    #[test]
    fn a_subagent_stranded_without_a_transcript_is_not_restored() {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        close_subagent_transcript(&mut app, OTHER_ID);

        app.reset_ui_chrome();
        app.restore_display();

        let restored: Vec<_> = app
            .task_states()
            .map(|task| (task.id.to_string(), task.status))
            .collect();
        assert_eq!(restored, vec![(OTHER_ID.to_owned(), TaskStatus::Done)]);
        let recorded: Vec<_> = app
            .state
            .session
            .subagents()
            .iter()
            .map(|sa| sa.tool_use_id.clone())
            .collect();
        assert_eq!(recorded, vec![OTHER_ID.to_owned()], "and stays dropped");
    }

    /// The `task` tool closes the subagent session before it reports a failure,
    /// so the `SubagentHistory` that only knows "it ended" always arrives before
    /// the `ToolDone` holding the verdict. Let the first one decide and every
    /// failure reads as done.
    #[test_case(
        |app: &mut App| close_subagent_transcript(app, TASK_ID),
        TaskStatus::Done, DONE_TEXT
        ; "a_close_nothing_follows_reads_as_done"
    )]
    #[test_case(
        |app| {
            close_subagent_transcript(app, TASK_ID);
            finish_subagent(app, TASK_ID, true);
        },
        TaskStatus::Error, ERROR_TEXT
        ; "a_late_verdict_corrects_the_close"
    )]
    #[test_case(
        |app| {
            finish_subagent(app, TASK_ID, true);
            close_subagent_transcript(app, TASK_ID);
        },
        TaskStatus::Error, ERROR_TEXT
        ; "a_close_never_clears_a_verdict"
    )]
    fn the_most_specific_ending_decides(end: fn(&mut App), status: TaskStatus, text: &str) {
        let mut app = app_with_subagent_id(TASK_ID);
        end(&mut app);
        assert_eq!(app.chats[1].task_status(), status);
        assert_eq!(app.chats[1].last_message_text(), text);
    }

    /// No way of ending a turn may leave a task `working`: nothing runs after
    /// to correct it, and the picker would spin on it forever. The finished
    /// task comes along to show the sweep leaves it alone.
    #[test_case(end_turn as fn(&mut App) ; "turn_end")]
    #[test_case(error_app                ; "parent_error")]
    #[test_case(cancel_app               ; "user_cancel")]
    fn a_turn_ending_terminalizes_every_unfinished_task(terminate: fn(&mut App)) {
        let mut app = app_with_two_subagents();
        finish_subagent(&mut app, TASK_ID, false);
        assert_eq!(app.chats[2].task_status(), TaskStatus::Working);

        terminate(&mut app);

        assert_eq!(
            app.task_states().map(|t| t.status).collect::<Vec<_>>(),
            vec![TaskStatus::Done, TaskStatus::Error]
        );
    }

    /// The picker remembers the focused id before it previews anything and
    /// goes back there on cancel. With two entries claiming focus, or none,
    /// the user is stranded on a task they were only peeking at.
    #[test_case(MAIN_TASK_ID, 0 ; "main")]
    #[test_case(TASK_ID, 1      ; "first_task")]
    #[test_case(OTHER_ID, 2     ; "second_task")]
    fn exactly_one_task_reports_focused(id: &str, expected: usize) {
        let mut app = app_with_two_subagents();
        app.focus_task(OTHER_ID).unwrap();
        app.focus_task(id).unwrap();

        let tasks = app.tasks();
        let focused: Vec<_> = tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.focused)
            .map(|(idx, task)| (idx, &*task.id))
            .collect();
        assert_eq!(focused, vec![(expected, id)]);
    }

    /// A task announces itself the moment it shows up working and on every
    /// change after, never twice for the same state. A session reset forgets
    /// it, so running the same task again reads as new.
    #[test]
    fn diff_announces_first_sight_and_every_change() {
        let id: Arc<str> = Arc::from(TASK_ID);
        let working = [(Arc::clone(&id), TaskStatus::Working)];
        let mut previous = Vec::new();

        assert_eq!(
            collect(&mut previous, &working),
            vec![(TASK_ID.to_owned(), TaskStatus::Working)]
        );
        assert!(collect(&mut previous, &working).is_empty());
        assert_eq!(
            collect(&mut previous, &[(Arc::clone(&id), TaskStatus::Done)]),
            vec![(TASK_ID.to_owned(), TaskStatus::Done)]
        );

        assert!(collect(&mut previous, &[]).is_empty());
        assert!(previous.is_empty());
        assert_eq!(
            collect(&mut previous, &working),
            vec![(TASK_ID.to_owned(), TaskStatus::Working)]
        );
    }

    /// A task first seen already finished stays quiet, so a reload does not
    /// replay old news. It is still recorded, or its next change would look
    /// like another first sight and stay quiet too.
    #[test]
    fn a_task_first_seen_finished_is_recorded_silently() {
        let one: Arc<str> = Arc::from(TASK_ID);
        let two: Arc<str> = Arc::from(OTHER_ID);
        let mut previous = Vec::new();

        assert!(collect(&mut previous, &[(Arc::clone(&one), TaskStatus::Done)]).is_empty());
        assert_eq!(
            collect(&mut previous, &[(one, TaskStatus::Error)]),
            vec![(TASK_ID.to_owned(), TaskStatus::Error)]
        );

        assert_eq!(
            collect(&mut previous, &[(Arc::clone(&two), TaskStatus::Working)]),
            vec![(OTHER_ID.to_owned(), TaskStatus::Working)]
        );
        assert_eq!(previous, vec![(two, TaskStatus::Working)]);
    }

    const FOLLOWUP: &str = "also check the tests";

    /// A subagent session stays open for follow-ups after its first result, so
    /// the parent's turn ending must leave its routing and status alone. Once
    /// the session really closes, the next turn end terminalizes the chat.
    #[test]
    fn a_live_subagent_survives_the_turn_end() {
        let mut app = app_with_subagent_id(TASK_ID);
        let mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));

        end_turn(&mut app);

        assert_eq!(app.chats[1].task_status(), TaskStatus::Working);
        assert!(app.chat_index.contains_key(TASK_ID));

        drop(mailbox);
        end_turn(&mut app);
        assert_eq!(app.chats[1].task_status(), TaskStatus::Error);
        assert!(!app.chat_index.contains_key(TASK_ID));
    }

    #[test]
    fn prompt_subagent_delivers_and_draws_the_message() {
        let mut app = app_with_subagent_id(TASK_ID);
        let mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));

        app.prompt_subagent(TASK_ID, FOLLOWUP.into()).unwrap();

        assert_eq!(smol::block_on(mailbox.recv()).as_deref(), Some(FOLLOWUP));
        assert_eq!(app.chats[1].last_message_text(), FOLLOWUP);
        assert_eq!(app.chats[1].last_message_role(), Some(&DisplayRole::User));
    }

    #[test]
    fn prompt_subagent_errors_when_not_live() {
        let mut app = app_with_subagent_id(TASK_ID);

        let err = app.prompt_subagent(TASK_ID, FOLLOWUP.into()).unwrap_err();

        assert_eq!(err, format!("subagent is no longer running: {TASK_ID}"));
        assert_ne!(app.chats[1].last_message_text(), FOLLOWUP);
    }

    #[test]
    fn prompt_subagent_errors_for_an_unknown_task() {
        let mut app = app_with_subagent_id(TASK_ID);

        let err = app
            .prompt_subagent(MISSING_ID, FOLLOWUP.into())
            .unwrap_err();

        assert_eq!(err, format!("{UNKNOWN_TASK_ERR}{MISSING_ID}"));
    }

    /// The focused subagent chat owns the input box: typing and submitting
    /// there must not touch the main chat's queue or start a run.
    #[test]
    fn submitting_in_a_subagent_chat_prompts_the_subagent() {
        let mut app = app_with_subagent_id(TASK_ID);
        let mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));
        app.focus_task(TASK_ID).unwrap();

        app.update(Msg::Key(key(KeyCode::Char('h'))));
        app.update(Msg::Key(key(KeyCode::Char('i'))));
        let actions = app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(actions.is_empty(), "no run starts from a subagent chat");
        assert_eq!(smol::block_on(mailbox.recv()).as_deref(), Some("hi"));
        assert_eq!(app.chats[1].last_message_text(), "hi");
        assert!(app.input_box.is_empty());
        assert!(app.main_chat().last_message_text() != "hi");
    }

    /// A finished subagent has no live session, so the input box flashes the
    /// error instead of pretending the follow-up went somewhere.
    #[test]
    fn submitting_to_a_finished_subagent_flashes_the_error() {
        let mut app = app_with_subagent_id(TASK_ID);
        close_subagent_transcript(&mut app, TASK_ID);
        app.focus_task(TASK_ID).unwrap();

        app.update(Msg::Key(key(KeyCode::Char('h'))));
        let actions = app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(actions.is_empty());
        assert_ne!(app.chats[1].last_message_text(), "h");
        assert_eq!(app.input_box.buffer.value(), "h");
    }

    /// Ctrl-C with a draft in the box discards the draft, the way the main
    /// chat does, instead of quitting the app from under the user.
    #[test]
    fn ctrl_c_in_a_subagent_chat_discards_the_draft() {
        let mut app = app_with_subagent_id(TASK_ID);
        let _mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));
        app.focus_task(TASK_ID).unwrap();
        app.update(Msg::Key(key(KeyCode::Char('h'))));

        let actions = app.update(Msg::Key(
            crate::components::keybindings::key::QUIT.to_key_event(),
        ));

        assert!(actions.is_empty());
        assert!(app.input_box.is_empty());
        assert_eq!(app.exit_request, crate::components::ExitRequest::None);
    }

    /// Events of a conversational subagent keep the run id they were born
    /// with; a new main run must not make them invisible.
    #[test]
    fn subagent_events_land_despite_a_stale_run_id() {
        let mut app = app_with_subagent_id(TASK_ID);
        let _mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));
        end_turn(&mut app);
        app.run_id += 1;

        app.update(subagent_msg_with_run_id(
            AgentEvent::TextDelta { text: "y".into() },
            TASK_ID,
            None,
            1,
        ));

        app.chats[1].flush();
        assert_eq!(app.chats[1].last_message_text(), "xy");
    }

    /// A main-chat event from a stale run is still dropped: only subagent
    /// traffic self-routes past the run id guard.
    #[test]
    fn main_chat_events_still_drop_on_a_stale_run_id() {
        let mut app = app_with_subagent_id(TASK_ID);
        end_turn(&mut app);
        app.run_id += 1;

        app.update(agent_msg_with_run_id(
            AgentEvent::TextDelta {
                text: "stale".into(),
            },
            1,
        ));

        assert_ne!(app.chats[0].last_message_text(), "stale");
    }

    /// The draft is per chat: a half-typed follow-up must not leak into the
    /// main chat's input box, and the main chat's draft survives the trip.
    #[test]
    fn switching_chats_swaps_input_drafts() {
        let mut app = app_with_subagent_id(TASK_ID);
        app.update(Msg::Key(key(KeyCode::Char('m'))));

        app.focus_task(TASK_ID).unwrap();
        assert!(app.input_box.is_empty(), "main draft must not follow");

        app.update(Msg::Key(key(KeyCode::Char('s'))));
        app.focus_task(MAIN_TASK_ID).unwrap();
        assert_eq!(app.input_box.buffer.value(), "m");

        app.focus_task(TASK_ID).unwrap();
        assert_eq!(app.input_box.buffer.value(), "s");
    }

    /// A follow-up turn mid-flight when the parent's turn ends is not an
    /// orphan: its in-progress tools keep spinning.
    #[test]
    fn turn_end_keeps_in_progress_tools_of_a_live_subagent() {
        let mut app = app_with_subagent_id(TASK_ID);
        let _mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));
        app.update(subagent_msg(tool_start("t1", "bash"), TASK_ID, None));
        assert_eq!(app.chats[1].in_progress_count(), 1);

        end_turn(&mut app);

        assert_eq!(app.chats[1].in_progress_count(), 1);

        // Once the session is gone, the same sweep terminalizes it.
        drop(_mailbox);
        end_turn(&mut app);
        assert_eq!(app.chats[1].in_progress_count(), 0);
    }

    /// Refusing images must not eat the follow-up: text and image go back
    /// into the input box so the user can strip the image and resend.
    #[test]
    fn submitting_an_image_to_a_subagent_restores_the_draft() {
        let mut app = app_with_subagent_id(TASK_ID);
        let mailbox = maki_agent::SubagentMailbox::register(Arc::from(TASK_ID));
        // Images stay pending in the box across chat switches: attach in the
        // main chat, then focus the subagent.
        app.input_box
            .attach_image(ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA==")));
        app.focus_task(TASK_ID).unwrap();
        app.update(Msg::Key(key(KeyCode::Char('h'))));

        let actions = app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(actions.is_empty(), "no run starts from a subagent chat");
        assert!(mailbox.receiver().try_recv().is_err(), "nothing was sent");
        assert_eq!(app.input_box.buffer.value(), "h");
        assert!(!app.input_box.is_empty(), "the image is restored too");
    }
}

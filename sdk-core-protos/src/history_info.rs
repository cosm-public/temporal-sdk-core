use crate::temporal::api::{
    common::v1::WorkflowType,
    enums::v1::{EventType, TaskQueueKind},
    history::v1::{history_event, History, HistoryEvent},
    taskqueue::v1::TaskQueue,
    workflowservice::v1::{GetWorkflowExecutionHistoryResponse, PollWorkflowTaskQueueResponse},
};
use anyhow::{anyhow, bail};
use rand::{thread_rng, Rng};

/// Contains information about a validated history. Used for replay and other testing.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryInfo {
    previous_started_event_id: i64,
    workflow_task_started_event_id: i64,
    // This needs to stay private so the struct can't be instantiated outside of the constructor,
    // which enforces some invariants regarding history structure that need to be upheld.
    events: Vec<HistoryEvent>,
    wf_task_count: usize,
    wf_type: String,
}

type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

impl HistoryInfo {
    /// Constructs a new instance, retaining only enough events to reach the provided workflow
    /// task number. If not provided, all events are retained.
    pub fn new_from_history(h: &History, to_wf_task_num: Option<usize>) -> Result<Self> {
        let events = &h.events;
        if events.is_empty() {
            bail!("History is empty!");
        }

        let is_all_hist = to_wf_task_num.is_none();
        let to_wf_task_num = to_wf_task_num.unwrap_or(usize::MAX);
        let mut workflow_task_started_event_id = 0;
        let mut wf_task_count = 0;
        let mut history = events.iter().peekable();

        let wf_type = match &events.get(0).unwrap().attributes {
            Some(history_event::Attributes::WorkflowExecutionStartedEventAttributes(attrs)) => {
                attrs
                    .workflow_type
                    .as_ref()
                    .ok_or(anyhow!(
                        "No workflow type defined in execution started attributes"
                    ))?
                    .name
                    .clone()
            }
            _ => bail!("First event in history was not workflow execution started!"),
        };

        let mut events = vec![];
        while let Some(event) = history.next() {
            events.push(event.clone());
            let next_event = history.peek();

            if event.event_type == EventType::WorkflowTaskStarted as i32 {
                let next_is_completed = next_event.map_or(false, |ne| {
                    ne.event_type == EventType::WorkflowTaskCompleted as i32
                });
                let next_is_failed_or_timeout = next_event.map_or(false, |ne| {
                    ne.event_type == EventType::WorkflowTaskFailed as i32
                        || ne.event_type == EventType::WorkflowTaskTimedOut as i32
                });

                if next_event.is_none() || next_is_completed {
                    let previous_started_event_id = workflow_task_started_event_id;
                    workflow_task_started_event_id = event.event_id;
                    if workflow_task_started_event_id == previous_started_event_id {
                        bail!(
                            "Latest wf started id {workflow_task_started_event_id} and previous \
                             one {previous_started_event_id} are equal!"
                        );
                    }
                    wf_task_count += 1;
                    if wf_task_count == to_wf_task_num || next_event.is_none() {
                        return Ok(Self {
                            previous_started_event_id,
                            workflow_task_started_event_id,
                            events,
                            wf_task_count,
                            wf_type,
                        });
                    }
                } else if next_event.is_some() && !next_is_failed_or_timeout {
                    bail!(
                        "Invalid history! Event {event:?} should be WFT \
                           completed, failed, or timed out"
                    );
                }
            }

            if next_event.is_none() {
                if event.is_final_wf_execution_event() || is_all_hist {
                    // Since this is the end of execution, we are pretending that the SDK is
                    // replaying *complete* history, which would mean the previously started ID is
                    // in fact the last task.
                    return Ok(Self {
                        previous_started_event_id: workflow_task_started_event_id,
                        workflow_task_started_event_id,
                        events,
                        wf_task_count,
                        wf_type,
                    });
                }
                // No more events
                if workflow_task_started_event_id != event.event_id {
                    bail!("History ends unexpectedly");
                }
            }
        }
        unreachable!()
    }

    /// Remove events from the beginning of this history such that it looks like what would've been
    /// delivered on a sticky queue where the previously started task was the one before the last
    /// task in this history.
    ///
    /// This is not *fully* accurate in that it will include commands that were part of the last
    /// WFT completion, which the server would typically not include, but it's good enough for
    /// testing.
    pub fn make_incremental(&mut self) {
        let last_complete_ix = self
            .events
            .iter()
            .rposition(|he| he.event_type() == EventType::WorkflowTaskCompleted)
            .expect("Must be a WFT completed event in history");
        self.events.drain(0..=last_complete_ix);
    }

    pub fn events(&self) -> &[HistoryEvent] {
        &self.events
    }

    /// Attempt to extract run id from internal events. If the first event is not workflow execution
    /// started, it will panic.
    pub fn orig_run_id(&self) -> &str {
        if let Some(history_event::Attributes::WorkflowExecutionStartedEventAttributes(wes)) =
            &self.events[0].attributes
        {
            &wes.original_execution_run_id
        } else {
            panic!("First event is wrong type")
        }
    }

    /// Return total workflow task count in this history
    pub const fn wf_task_count(&self) -> usize {
        self.wf_task_count
    }

    /// Create a workflow task polling response containing all the events in this history and a
    /// randomly generated task token. Caller should attach a meaningful `workflow_execution` if
    /// needed.
    pub fn as_poll_wft_response(&self, task_q: impl Into<String>) -> PollWorkflowTaskQueueResponse {
        let task_token: [u8; 16] = thread_rng().gen();
        PollWorkflowTaskQueueResponse {
            history: Some(History {
                events: self.events.clone(),
            }),
            task_token: task_token.to_vec(),
            workflow_type: Some(WorkflowType {
                name: self.wf_type.clone(),
            }),
            workflow_execution_task_queue: Some(TaskQueue {
                name: task_q.into(),
                kind: TaskQueueKind::Normal as i32,
            }),
            previous_started_event_id: self.previous_started_event_id,
            started_event_id: self.workflow_task_started_event_id,
            ..Default::default()
        }
    }

    /// Returns the last workflow task started event id
    pub fn previous_started_event_id(&self) -> i64 {
        self.previous_started_event_id
    }
}

impl From<HistoryInfo> for History {
    fn from(i: HistoryInfo) -> Self {
        Self { events: i.events }
    }
}

impl From<HistoryInfo> for GetWorkflowExecutionHistoryResponse {
    fn from(i: HistoryInfo) -> Self {
        Self {
            history: Some(i.into()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{temporal::api::enums::v1::EventType, TestHistoryBuilder};

    fn single_timer(timer_id: &str) -> TestHistoryBuilder {
        let mut t = TestHistoryBuilder::default();
        t.add_by_type(EventType::WorkflowExecutionStarted);
        t.add_full_wf_task();
        let timer_started_event_id = t.add_get_event_id(EventType::TimerStarted, None);
        t.add_timer_fired(timer_started_event_id, timer_id.to_string());
        t.add_workflow_task_scheduled_and_started();
        t
    }

    #[test]
    fn history_info_constructs_properly() {
        let t = single_timer("timer1");

        let history_info = t.get_history_info(1).unwrap();
        assert_eq!(3, history_info.events().len());
        let history_info = t.get_history_info(2).unwrap();
        assert_eq!(8, history_info.events().len());
    }

    #[test]
    fn incremental_works() {
        let t = single_timer("timer1");
        let hi = t.get_one_wft(2).unwrap();
        assert_eq!(hi.events().len(), 4);
        assert_eq!(hi.events()[0].event_id, 5);
    }
}
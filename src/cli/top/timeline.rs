use std::collections::VecDeque;

use anyhow::{bail, Context, Result};

use crate::core::events::{
    query::{EventBatch, EventCursor, EventGap, EventSource, PersistenceState},
    EventContext, LifecycleEvent,
};

pub(super) const LIMIT: usize = 1000;

pub(super) enum Entry {
    Event(Box<LifecycleEvent>),
    Gap(String),
}

pub(super) struct Timeline {
    pub entries: VecDeque<Entry>,
    pub cursor: EventCursor,
    pub offset: usize,
    pub gaps: u64,
    pub evicted: u64,
    pub latest_gap: Option<String>,
    pub persistence: Option<PersistenceState>,
    context: Option<EventContext>,
}

impl Timeline {
    pub fn new(run_id: String) -> Self {
        Self {
            entries: VecDeque::new(),
            cursor: EventCursor {
                run_id,
                next_sequence: 0,
            },
            offset: 0,
            gaps: 0,
            evicted: 0,
            latest_gap: None,
            persistence: None,
            context: None,
        }
    }

    pub fn ingest(&mut self, batch: EventBatch) -> Result<()> {
        let context = batch
            .context
            .context("event query has no runtime context")?;
        let cursor = batch.cursor.context("event query has no cursor")?;
        if batch.source != EventSource::Live
            || context.run_id != self.cursor.run_id
            || cursor.run_id != self.cursor.run_id
            || self.context.as_ref().is_some_and(|old| old != &context)
            || batch
                .gaps
                .iter()
                .any(|gap| matches!(gap, EventGap::DifferentRun { .. }))
        {
            bail!("supervisor run changed; reopen top to inspect the new run");
        }
        if cursor.next_sequence < self.cursor.next_sequence || batch.entries.len() > LIMIT {
            bail!("invalid event query window");
        }
        // Validate the entire batch before changing visible history. A cursor
        // is inclusive; accepting repeats or mixed runs would invent evidence.
        let mut next = self.cursor.next_sequence;
        for event in &batch.entries {
            if event.run_id != self.cursor.run_id
                || event.sequence < next
                || event.sequence >= cursor.next_sequence
                || event.cause.is_some_and(|cause| cause >= event.sequence)
                || event
                    .generation
                    .is_some_and(|generation| generation > event.sequence)
            {
                bail!("invalid event sequence or runtime identity");
            }
            next = event.sequence + 1;
        }
        for gap in batch.gaps {
            self.gap(serde_json::to_string(&gap)?);
        }
        if batch.omitted_gaps > 0 {
            self.gap(format!(
                "{} additional gap diagnostics omitted",
                batch.omitted_gaps
            ));
        }
        for event in batch.entries {
            self.push(Entry::Event(Box::new(event)));
        }
        self.context = Some(context);
        self.cursor = cursor;
        self.persistence = batch.persistence;
        Ok(())
    }

    fn gap(&mut self, text: String) {
        self.gaps = self.gaps.saturating_add(1);
        self.latest_gap = Some(text.clone());
        self.push(Entry::Gap(text));
    }

    fn push(&mut self, entry: Entry) {
        if self.entries.len() == LIMIT {
            self.entries.pop_front();
            self.evicted = self.evicted.saturating_add(1);
        }
        self.entries.push_back(entry);
        if self.offset > 0 {
            self.offset = self
                .offset
                .saturating_add(1)
                .min(self.entries.len().saturating_sub(1));
        }
    }

    pub fn line(&self, entry: &Entry) -> String {
        let event = match entry {
            Entry::Event(event) => event,
            Entry::Gap(text) => return format!("GAP: {}", super::safe_text(text)),
        };
        let cause = event.cause.map_or_else(
            || "-".into(),
            |cause| {
                let retained = self
                    .entries
                    .iter()
                    .any(|entry| matches!(entry, Entry::Event(e) if e.sequence == cause));
                format!("#{cause}{}", if retained { "" } else { "(unavailable)" })
            },
        );
        let generation = event
            .generation
            .map_or_else(|| "-".into(), |id| id.to_string());
        // Keep the explicit cause near the front even when payloads are long.
        // JSON preserves structured observations without inventing an explanation.
        let data =
            serde_json::to_string(&event.data).unwrap_or_else(|_| "[unavailable payload]".into());
        super::safe_text(&format!(
            "#{} g={} [{}] cause={} {} {}{}",
            event.sequence,
            generation,
            event.service.as_deref().unwrap_or("supervisor"),
            cause,
            event.timestamp.format("%H:%M:%SZ"),
            data,
            if event.truncated { " [truncated]" } else { "" },
        ))
    }

    pub fn summary(&self) -> String {
        format!(
            "Gaps: {} | local rows evicted: {} | disk: {} | cause=recorded link; -=unrecorded",
            self.gaps,
            self.evicted,
            match self.persistence {
                Some(PersistenceState::Disabled) => "off",
                Some(PersistenceState::Recording) => "recording",
                Some(PersistenceState::Failed) => "FAILED (memory only)",
                None => "unknown",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::query::{EventFilter, EventQuery};
    use crate::core::events::{EventData, EventRecorder, EVENT_CAPACITY};

    fn batch(recorder: &EventRecorder, timeline: &Timeline) -> EventBatch {
        EventQuery {
            filter: EventFilter::default(),
            tail: LIMIT,
            cursor: Some(timeline.cursor.clone()),
        }
        .select(recorder.history().snapshot())
        .unwrap()
    }

    #[test]
    fn test_timeline_gaps_are_visible_and_history_is_bounded() {
        let recorder = EventRecorder::new("state.json".into(), None);
        let mut timeline = Timeline::new(recorder.run_id().into());
        for _ in 0..EVENT_CAPACITY + 8 {
            recorder.record(None, None, None, EventData::SupervisorStarted);
        }
        let mut window = batch(&recorder, &timeline);
        window.persistence = Some(PersistenceState::Failed);
        timeline.ingest(window).unwrap();
        assert_eq!(timeline.entries.len(), LIMIT);
        assert_eq!(timeline.gaps, 2); // retention and tail limit
        assert_eq!(timeline.evicted, 2); // gap rows still described by the banner
        assert!(timeline
            .latest_gap
            .as_ref()
            .unwrap()
            .contains("tail-limited"));
        assert!(timeline.summary().contains("FAILED (memory only)"));
        let Entry::Event(first) = &timeline.entries[0] else {
            panic!()
        };
        assert_eq!(first.sequence, 32);
        timeline.offset = 10;
        recorder.record(None, None, None, EventData::SupervisorCancelled);
        timeline.ingest(batch(&recorder, &timeline)).unwrap();
        assert_eq!(timeline.offset, 11);
        assert_eq!(timeline.evicted, 3);
        assert_eq!(timeline.entries.len(), LIMIT);
        timeline.ingest(batch(&recorder, &timeline)).unwrap();
        assert_eq!(
            timeline.offset, 11,
            "empty polls must not move the viewport"
        );
        assert_eq!(timeline.gaps, 2, "cursor polling must not repeat old gaps");
    }

    #[test]
    fn test_timeline_preserves_order_generations_and_only_recorded_causes() {
        let recorder = EventRecorder::new("state.json".into(), None);
        recorder.record(None, None, None, EventData::SupervisorStarted);
        recorder.record(
            Some("api\x1b[2J"),
            Some(1),
            None,
            EventData::GenerationPending,
        );
        recorder.record(Some("api"), Some(1), Some(1), EventData::Starting);
        let mut timeline = Timeline::new(recorder.run_id().into());
        timeline.ingest(batch(&recorder, &timeline)).unwrap();
        let first = timeline.line(&timeline.entries[0]);
        let second = timeline.line(&timeline.entries[1]);
        let third = timeline.line(&timeline.entries[2]);
        assert!(first.starts_with("#0 g=- [supervisor] cause=-"));
        assert!(second.starts_with("#1 g=1 [api\\u{1b}[2J] cause=-"));
        assert!(third.starts_with("#2 g=1 [api] cause=#1 "));
        timeline.entries.pop_front();
        timeline.entries.pop_front();
        assert!(timeline
            .line(&timeline.entries[0])
            .contains("cause=#1(unavailable)"));
        assert!(!second.contains('\x1b'));
    }

    #[test]
    fn test_timeline_rejects_invalid_batches_without_partial_updates() {
        let recorder = EventRecorder::new("state.json".into(), None);
        recorder.record(None, None, None, EventData::SupervisorStarted);
        recorder.record(None, None, Some(0), EventData::SupervisorCancelled);
        let mut timeline = Timeline::new(recorder.run_id().into());
        let valid = batch(&recorder, &timeline);
        for change in 0..7 {
            let mut invalid = valid.clone();
            match change {
                0 => invalid.context.as_mut().unwrap().run_id = "other".into(),
                1 => invalid.entries[1].run_id = "other".into(),
                2 => invalid.entries.reverse(),
                3 => invalid.entries[1].cause = Some(1),
                4 => invalid.cursor = None,
                5 => invalid.cursor.as_mut().unwrap().next_sequence = 1,
                _ => invalid.source = EventSource::Stored,
            }
            assert!(timeline.ingest(invalid).is_err(), "case {change}");
            assert!(timeline.entries.is_empty());
            assert_eq!(timeline.cursor.next_sequence, 0);
        }
        timeline.ingest(valid.clone()).unwrap();
        assert!(
            timeline.ingest(valid).is_err(),
            "replayed batches must be rejected"
        );
        assert_eq!(timeline.entries.len(), 2);
    }
}

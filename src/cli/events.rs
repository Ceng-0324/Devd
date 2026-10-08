use super::{
    protocol::{self, Request, Response},
    transport::Stream,
};
use crate::core::events::{
    query::{
        EventBatch, EventCursor, EventFilter, EventGap, EventKind, EventQuery, PersistenceState,
    },
    EventHistory,
};
use anyhow::{bail, Context, Result};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(clap::Args)]
pub(super) struct Args {
    service: Option<String>,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
    tail: u16,
    /// Match any of these event types; repeat --type to select several.
    #[arg(long = "type", value_enum)]
    kinds: Vec<EventKind>,
    #[arg(long, value_parser = crate::config::parse_duration)]
    since: Option<Duration>,
    /// Resume at RUN_UUID:NEXT_SEQUENCE, inclusive.
    #[arg(long)]
    cursor: Option<EventCursor>,
    /// Print versioned JSON batches (one per line when following).
    #[arg(long)]
    json: bool,
    #[arg(short, long)]
    follow: bool,
    /// Read retained disk history after shutdown, without loading YAML.
    #[arg(long, conflicts_with = "follow")]
    stored: bool,
}

pub(super) async fn run(args: Args, socket: &Path, state_dir: &Path) -> Result<()> {
    let query = EventQuery {
        filter: EventFilter {
            service: args.service,
            kinds: args.kinds,
            since: super::log_filter(None, args.since, None)?.since,
        },
        tail: args.tail.into(),
        cursor: args.cursor,
    };
    query.validate().map_err(anyhow::Error::msg)?;
    if args.stored {
        let directory = state_dir.join("events");
        let batch = tokio::task::spawn_blocking(move || {
            crate::core::events::storage::read_stored(&directory, &query)
        })
        .await?
        .context(
            "cannot read stored events; enable persistence at startup and query after shutdown",
        )?;
        return super::output(&render(&batch, args.json)?);
    }
    if !args.follow {
        let Response::Events(batch) = protocol::request(socket, Request::Events { query }).await?
        else {
            bail!("unexpected events response");
        };
        return super::output(&render(&batch, args.json)?);
    }
    let mut stream = protocol::connect(socket).await?;
    protocol::write(&mut stream, &Request::FollowEvents { query }).await?;
    let mut stdout = super::stdout::Stdout::new()?;
    let mut first = true;
    loop {
        let response = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            response = async {
                if first { tokio::time::timeout(protocol::IO_TIMEOUT, protocol::next_response(&mut stream)).await.context("event stream did not start")? }
                else { protocol::next_response(&mut stream).await }
            } => response?,
        };
        match response {
            Some(Response::Events(batch)) => {
                first = false;
                let text = render(&batch, args.json)?;
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    result = async {
                        stdout.write_all(text.as_bytes()).await?;
                        stdout.flush().await
                    } => result?,
                }
            }
            Some(Response::Error(error)) => bail!("{error}"),
            None if !first => break,
            _ => bail!("unexpected event stream response"),
        }
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        result = stdout.flush() => result?,
    }
    Ok(())
}

fn render(batch: &EventBatch, json: bool) -> Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string(batch)?));
    }
    let mut text = format!(
        "{:?} events; cursor={}; persistence={:?}\n",
        batch.source,
        batch
            .cursor
            .as_ref()
            .map_or_else(|| "none".into(), ToString::to_string),
        batch.persistence
    );
    for gap in &batch.gaps {
        text.push_str(&format!("GAP {}\n", serde_json::to_string(gap)?));
    }
    if batch.omitted_gaps > 0 {
        text.push_str(&format!(
            "GAP {} older gap diagnostics omitted\n",
            batch.omitted_gaps
        ));
    }
    for event in &batch.entries {
        // JSON encoding keeps untrusted service names/evidence on one safe line.
        text.push_str(&format!(
            "{} {}:{} {} generation={} {}\n",
            event.timestamp.to_rfc3339(),
            event.run_id,
            event.sequence,
            serde_json::to_string(event.service.as_deref().unwrap_or("devd"))?,
            event
                .generation
                .map_or_else(|| "-".into(), |value| value.to_string()),
            serde_json::to_string(&event.data)?
        ));
    }
    Ok(text)
}

pub(super) async fn follow(
    stream: &mut Stream,
    history: EventHistory,
    query: EventQuery,
    mut persistence: tokio::sync::watch::Receiver<PersistenceState>,
) -> Result<()> {
    let (window, mut live) = history.subscribe_with_snapshot();
    let mut batch = query.select(window).map_err(anyhow::Error::msg)?;
    batch.persistence = Some(*persistence.borrow_and_update());
    protocol::write(stream, &Response::Events(batch.clone())).await?;
    batch.entries.clear();
    batch.gaps.clear();
    batch.first_available = None;
    loop {
        let mut unexpected = [0];
        tokio::select! {
            input = stream.read(&mut unexpected) => {
                if input? == 0 { return Ok(()); }
                bail!("unexpected input after event subscription");
            }
            changed = persistence.changed() => {
                if changed.is_err() { return Ok(()); }
                batch.persistence = Some(*persistence.borrow_and_update());
            }
            event = live.recv() => match event {
                Ok(event) => {
                    batch.cursor.as_mut().unwrap().next_sequence = event.sequence + 1;
                    if query.filter.matches(&event) { batch.entries.push((*event).clone()); }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    batch.gaps.push(EventGap::SubscriberLag { run_id: batch.context.as_ref().unwrap().run_id.clone(), skipped });
                    // Keep the last inspected cursor; a reconnect can recover
                    // retained events. No silent advance over uninspected data.
                    protocol::write(stream, &Response::Events(batch)).await?;
                    bail!("event stream lagged; reconnect using the last cursor");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
        protocol::write(stream, &Response::Events(batch.clone())).await?;
        batch.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{EventData, EventRecorder};

    #[tokio::test]
    async fn test_event_follow_reports_lag_before_disconnect_and_preserves_cursor() {
        let recorder = EventRecorder::new("state".into(), None);
        let query = EventQuery {
            filter: EventFilter::default(),
            tail: 100,
            cursor: None,
        };
        let (server, mut client) = tokio::io::duplex(4096);
        let mut server: Stream = Box::new(server);
        let (_status, state) = tokio::sync::watch::channel(PersistenceState::Disabled);
        let history = recorder.history();
        let task = tokio::spawn(async move { follow(&mut server, history, query, state).await });
        let Some(Response::Events(first)) = protocol::next_response(&mut client).await.unwrap()
        else {
            panic!("missing first batch")
        };
        assert_eq!(first.cursor.unwrap().next_sequence, 0);
        // No await: this single-thread runtime cannot consume between publishes.
        for _ in 0..300 {
            recorder.record(None, None, None, EventData::Starting);
        }
        let Some(Response::Events(gap)) = protocol::next_response(&mut client).await.unwrap()
        else {
            panic!("missing gap batch")
        };
        assert_eq!(
            gap.gaps,
            [EventGap::SubscriberLag {
                run_id: recorder.run_id().into(),
                skipped: 44
            }]
        );
        assert_eq!(gap.cursor.unwrap().next_sequence, 0);
        assert!(task
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("reconnect"));
    }

    #[tokio::test]
    async fn test_filtered_follow_advances_watermark_and_reports_persistence_failure() {
        let recorder = EventRecorder::new("state".into(), None);
        let query = EventQuery {
            filter: EventFilter {
                service: Some("api".into()),
                ..Default::default()
            },
            tail: 100,
            cursor: None,
        };
        let (server, mut client) = tokio::io::duplex(4096);
        let mut server: Stream = Box::new(server);
        let (status, state) = tokio::sync::watch::channel(PersistenceState::Recording);
        let history = recorder.history();
        let task = tokio::spawn(async move { follow(&mut server, history, query, state).await });
        protocol::next_response(&mut client).await.unwrap();
        recorder.record(Some("other"), None, None, EventData::Starting);
        let Some(Response::Events(batch)) = protocol::next_response(&mut client).await.unwrap()
        else {
            panic!("missing batch")
        };
        assert!(batch.entries.is_empty());
        assert_eq!(batch.cursor.unwrap().next_sequence, 1);
        status.send_replace(PersistenceState::Failed);
        let Some(Response::Events(batch)) = protocol::next_response(&mut client).await.unwrap()
        else {
            panic!("missing failure status")
        };
        assert_eq!(batch.persistence, Some(PersistenceState::Failed));
        drop(client);
        task.await.unwrap().unwrap();
    }

    #[test]
    fn test_event_text_escapes_terminal_controls_and_json_rejects_new_schema() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(Some("api\u{1b}[2J\n"), None, None, EventData::Starting);
        let query = EventQuery {
            filter: EventFilter::default(),
            tail: 100,
            cursor: None,
        };
        let batch = query.select(recorder.history().snapshot()).unwrap();
        let text = render(&batch, false).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert_eq!(text.lines().count(), 2);
        let json = render(&batch, true).unwrap();
        assert!(serde_json::from_str::<EventBatch>(&json).is_ok());
        assert!(serde_json::from_str::<EventBatch>(&json.replacen(
            "\"schema_version\":1",
            "\"schema_version\":2",
            1
        ))
        .is_err());
    }
}

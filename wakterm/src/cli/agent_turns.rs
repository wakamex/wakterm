//! `wakterm agent turns`: the agent turns that finished since a time or
//! since a named reader last looked, grouped by agent.

use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone, Utc};
use clap::Parser;
use mux::agent_event::{AgentEvent, AgentEventKind, AgentEventPage, AgentEventStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use wakterm_client::client::Client;

/// Lines and characters of a final message that a summary shows.
const PREVIEW_LINES: usize = 2;
const PREVIEW_CHARS: usize = 200;
/// How far before `--since` the search for its first event starts, since
/// events are stored in sequence order but carry their provider's time.
const SINCE_MARGIN_MINUTES: i64 = 15;
/// How far back a reader's first look goes without `--since`.
const FIRST_READ_HOURS: i64 = 24;
const PAGE: u32 = 1000;

#[derive(Debug, Parser, Clone)]
pub struct AgentTurnsCommand {
    /// List turns that finished since this time: a duration ago such as
    /// 90s, 30m, 2h or 1d; a clock time such as 5am, 5:30pm or 17:00, its
    /// most recent occurrence; or an RFC 3339 timestamp
    #[arg(long, value_parser = parse_since)]
    since: Option<DateTime<Utc>>,

    /// List turns after this reader's saved position and save the new one,
    /// so that readers with different names never take each other's place.
    /// A reader's first look covers the last 24 hours unless --since is given
    #[arg(long, value_parser = parse_reader)]
    reader: Option<String>,

    /// With --reader, list without saving the new position
    #[arg(long, requires = "reader")]
    peek: bool,

    /// Only these agents, by name or agent ID; repeatable
    #[arg(long = "agent")]
    agents: Vec<String>,

    /// Every turn's full final message instead of the latest one's opening
    #[arg(long)]
    full: bool,

    /// Output format: "table" or "json"
    #[arg(long, default_value = "table", value_parser = ["table", "json"])]
    format: String,
}

impl AgentTurnsCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let names = client
            .list_agent_api_catalog(codec::ListAgentApiCatalog {})
            .await?
            .catalog
            .agents
            .into_iter()
            .map(|agent| (agent.agent_id, (agent.name, agent.harness)))
            .collect::<BTreeMap<_, _>>();
        let read_page = |after_sequence, limit| {
            let client = client.clone();
            async move {
                client
                    .read_agent_events(codec::ReadAgentEvents {
                        after_sequence,
                        limit,
                        wait_ms: 0,
                    })
                    .await
                    .map(|response| response.page)
            }
        };
        let checkpoint = self
            .reader
            .as_deref()
            .map(load_checkpoint)
            .transpose()?
            .flatten();
        let since = match (self.since, &checkpoint, &self.reader) {
            (Some(since), _, _) => Some(since),
            (None, None, Some(_)) => Some(Utc::now() - Duration::hours(FIRST_READ_HOURS)),
            _ => None,
        };
        anyhow::ensure!(
            since.is_some() || checkpoint.is_some(),
            "give --since, --reader, or both"
        );
        let read = read_turns(read_page, checkpoint.as_ref().map(|c| c.sequence), since).await?;
        let report = Report::new(
            &read,
            &names,
            &self.agents,
            self.full,
            since,
            self.reader.clone(),
        );
        match self.format.as_str() {
            "json" => {
                serde_json::to_writer_pretty(std::io::stdout().lock(), &report)?;
                println!();
            }
            _ => print!("{}", report.table()),
        }
        if let (Some(reader), false) = (&self.reader, self.peek) {
            save_checkpoint(
                reader,
                &Checkpoint {
                    sequence: read.through_sequence,
                    saved_at: Utc::now(),
                },
            )?;
        }
        Ok(())
    }
}

fn parse_reader(name: &str) -> Result<String, String> {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
    {
        Ok(name.to_string())
    } else {
        Err("a reader name is letters, digits, '-', '_' and '.'".to_string())
    }
}

fn parse_since(text: &str) -> Result<DateTime<Utc>, String> {
    parse_since_at(text, Local::now())
}

/// Parses `--since` relative to `now`.
fn parse_since_at<Tz: TimeZone>(text: &str, now: DateTime<Tz>) -> Result<DateTime<Utc>, String> {
    let text = text.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&Utc));
    }
    let lower = text.to_ascii_lowercase();
    if let Some(split) = lower.find(|c: char| !c.is_ascii_digit()) {
        let (number, unit) = lower.split_at(split);
        if let (Ok(number), Some(seconds)) = (
            number.parse::<i64>(),
            match unit {
                "s" => Some(1),
                "m" => Some(60),
                "h" => Some(3600),
                "d" => Some(86400),
                _ => None,
            },
        ) {
            return Ok((now - Duration::seconds(number * seconds)).with_timezone(&Utc));
        }
    }
    let (clock, pm) = match (lower.strip_suffix("am"), lower.strip_suffix("pm")) {
        (Some(clock), _) => (clock.trim(), Some(false)),
        (_, Some(clock)) => (clock.trim(), Some(true)),
        _ => (lower.as_str(), None),
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>(), minute.parse::<u32>()),
        None => (clock.parse::<u32>(), Ok(0)),
    };
    let (Ok(mut hour), Ok(minute)) = (hour, minute) else {
        return Err(format!(
            "{text:?} is not a duration such as 2h, a clock time such as 5am or 17:00, or an RFC 3339 time"
        ));
    };
    match pm {
        Some(pm) if (1..=12).contains(&hour) => hour = hour % 12 + if pm { 12 } else { 0 },
        Some(_) => return Err(format!("{text:?} has an hour outside 1 to 12")),
        None => {}
    }
    let time = NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| format!("{text:?} is not a time of day"))?;
    let today = now.date_naive().and_time(time);
    let at = now
        .timezone()
        .from_local_datetime(&today)
        .earliest()
        .ok_or_else(|| format!("{text:?} does not exist today in the local time zone"))?;
    // A clock time later than now means that time yesterday.
    Ok(if at > now { at - Duration::days(1) } else { at }.with_timezone(&Utc))
}

#[derive(Debug, Serialize, Deserialize)]
struct Checkpoint {
    /// The last event sequence the reader has seen.
    sequence: u64,
    saved_at: DateTime<Utc>,
}

fn checkpoint_path(reader: &str) -> PathBuf {
    config::DATA_DIR
        .join("agent-turn-readers")
        .join(format!("{reader}.json"))
}

fn load_checkpoint(reader: &str) -> anyhow::Result<Option<Checkpoint>> {
    match std::fs::read(checkpoint_path(reader)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn save_checkpoint(reader: &str, checkpoint: &Checkpoint) -> anyhow::Result<()> {
    let path = checkpoint_path(reader);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec_pretty(checkpoint)?)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

/// The finished turns read from the event log.
#[derive(Debug, Default)]
struct ReadTurns {
    finals: Vec<AgentEvent>,
    /// The sequences read: after `after_sequence`, through `through_sequence`.
    after_sequence: u64,
    through_sequence: u64,
    /// Whether events after the reader's position are no longer retained.
    gap: bool,
}

/// Reads the turn finals after `after` and, with `since`, observed since
/// then. Without `after`, the first sequence read is found by binary search
/// on event times, starting `SINCE_MARGIN_MINUTES` early.
async fn read_turns<ReadPage, ReadPageFut>(
    mut read_page: ReadPage,
    after: Option<u64>,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<ReadTurns>
where
    ReadPage: FnMut(u64, u32) -> ReadPageFut,
    ReadPageFut: Future<Output = anyhow::Result<AgentEventPage>>,
{
    // Any page reports the log's bounds, even one too old to serve.
    let head = read_page(0, 1).await?;
    let (oldest, latest) = (head.oldest_available_sequence, head.latest_sequence);
    let mut read = ReadTurns {
        through_sequence: latest,
        ..Default::default()
    };
    let start = match after {
        Some(after) => {
            read.gap = after + 1 < oldest;
            after.max(oldest.saturating_sub(1))
        }
        None => {
            let since = since.expect("since without a reader position");
            let target = since - Duration::minutes(SINCE_MARGIN_MINUTES);
            // The last sequence whose event is older than the target.
            let (mut low, mut high) = (oldest.saturating_sub(1), latest);
            while low < high {
                let mid = low + (high - low + 1) / 2;
                let page = read_page(mid - 1, 1).await?;
                match page.events.first() {
                    Some(event) if event.observed_at < target => low = event.sequence.max(mid),
                    Some(_) => high = mid - 1,
                    None => high = mid - 1,
                }
            }
            low
        }
    };
    read.after_sequence = start;
    let mut cursor = start;
    while cursor < latest {
        let page = read_page(cursor, PAGE).await?;
        if matches!(page.status, AgentEventStatus::CursorTooOld) {
            read.gap = true;
            cursor = page.oldest_available_sequence.saturating_sub(1);
            continue;
        }
        let Some(next) = page.next_after_sequence.filter(|next| *next > cursor) else {
            break;
        };
        read.finals.extend(page.events.into_iter().filter(|event| {
            event.sequence <= latest
                && event.kind == AgentEventKind::TurnFinal
                && since.map_or(true, |since| event.observed_at >= since)
        }));
        cursor = next;
    }
    Ok(read)
}

#[derive(Debug, Serialize)]
struct Report {
    since: Option<DateTime<Utc>>,
    reader: Option<String>,
    after_sequence: u64,
    through_sequence: u64,
    /// Events after the reader's position were dropped from the log before
    /// it read them, so turns may be missing.
    gap: bool,
    turns: usize,
    agents: Vec<AgentTurns>,
}

#[derive(Debug, Serialize)]
struct AgentTurns {
    agent_id: String,
    name: Option<String>,
    harness: Option<String>,
    turns: usize,
    outcomes: BTreeMap<String, usize>,
    latest_at: DateTime<Utc>,
    latest: Turn,
    /// Every turn, oldest first, with --full.
    #[serde(skip_serializing_if = "Option::is_none")]
    all: Option<Vec<Turn>>,
}

#[derive(Debug, Serialize)]
struct Turn {
    turn_id: Option<String>,
    at: DateTime<Utc>,
    outcome: Option<String>,
    sequence: u64,
    /// The final message, its opening unless --full.
    text: Option<String>,
}

/// The opening of a message: its first lines, cut to a length.
fn preview(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(PREVIEW_LINES)
        .collect();
    let joined = lines.join(" ");
    let more = joined.chars().count() > PREVIEW_CHARS
        || text.lines().filter(|line| !line.trim().is_empty()).count() > PREVIEW_LINES;
    let mut cut: String = joined.chars().take(PREVIEW_CHARS).collect();
    if more {
        cut.push('…');
    }
    cut
}

impl Report {
    fn new(
        read: &ReadTurns,
        names: &BTreeMap<String, (String, String)>,
        only: &[String],
        full: bool,
        since: Option<DateTime<Utc>>,
        reader: Option<String>,
    ) -> Self {
        let mut by_agent: BTreeMap<&str, Vec<&AgentEvent>> = BTreeMap::new();
        for event in &read.finals {
            let name = names.get(&event.agent_id).map(|(name, _)| name.as_str());
            if only.is_empty()
                || only
                    .iter()
                    .any(|wanted| wanted == &event.agent_id || Some(wanted.as_str()) == name)
            {
                by_agent.entry(&event.agent_id).or_default().push(event);
            }
        }
        let turn = |event: &AgentEvent, full: bool| Turn {
            turn_id: event.turn_id.clone(),
            at: event.observed_at,
            outcome: event.outcome.clone(),
            sequence: event.sequence,
            text: event.text.as_deref().map(|text| {
                if full {
                    text.to_string()
                } else {
                    preview(text)
                }
            }),
        };
        let mut agents: Vec<AgentTurns> = by_agent
            .into_iter()
            .map(|(agent_id, mut events)| {
                events.sort_by_key(|event| (event.observed_at, event.sequence));
                let latest = events.last().expect("an agent with turns");
                let mut outcomes = BTreeMap::new();
                for event in &events {
                    *outcomes
                        .entry(
                            event
                                .outcome
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        )
                        .or_insert(0) += 1;
                }
                let (name, harness) = names.get(agent_id).cloned().unzip();
                AgentTurns {
                    agent_id: agent_id.to_string(),
                    name,
                    harness,
                    turns: events.len(),
                    outcomes,
                    latest_at: latest.observed_at,
                    latest: turn(latest, full),
                    all: full.then(|| events.iter().map(|event| turn(event, true)).collect()),
                }
            })
            .collect();
        agents.sort_by(|a, b| b.latest_at.cmp(&a.latest_at));
        Self {
            since,
            reader,
            after_sequence: read.after_sequence,
            through_sequence: read.through_sequence,
            gap: read.gap,
            turns: agents.iter().map(|agent| agent.turns).sum(),
            agents,
        }
    }

    fn table(&self) -> String {
        let mut out = String::new();
        let local = |at: &DateTime<Utc>| at.with_timezone(&Local).format("%m-%d %H:%M").to_string();
        out.push_str(&format!(
            "{} turn{} by {} agent{}{}{}\n",
            self.turns,
            if self.turns == 1 { "" } else { "s" },
            self.agents.len(),
            if self.agents.len() == 1 { "" } else { "s" },
            self.since
                .map(|since| format!(" since {}", local(&since)))
                .unwrap_or_default(),
            self.reader
                .as_deref()
                .map(|reader| format!(" for reader {reader}"))
                .unwrap_or_default()
        ));
        if self.gap {
            out.push_str("Some events after the reader's position are no longer kept, so turns may be missing.\n");
        }
        for agent in &self.agents {
            let outcomes = agent
                .outcomes
                .iter()
                .map(|(outcome, count)| format!("{count} {outcome}"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "\n{}  {} turn{} ({outcomes}), latest {}\n",
                agent.name.as_deref().unwrap_or(&agent.agent_id),
                agent.turns,
                if agent.turns == 1 { "" } else { "s" },
                local(&agent.latest_at)
            ));
            match &agent.all {
                Some(all) => {
                    for turn in all {
                        out.push_str(&format!(
                            "  {} {}\n",
                            local(&turn.at),
                            turn.outcome.as_deref().unwrap_or("unknown")
                        ));
                        for line in turn.text.as_deref().unwrap_or("").lines() {
                            out.push_str(&format!("    {line}\n"));
                        }
                    }
                }
                None => {
                    if let Some(text) = agent.latest.text.as_deref().filter(|text| !text.is_empty())
                    {
                        out.push_str(&format!("  {text}\n"));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn since_takes_durations_clock_times_and_timestamps() {
        let zone = FixedOffset::west_opt(4 * 3600).unwrap();
        let now = zone.with_ymd_and_hms(2026, 10, 28, 11, 30, 0).unwrap();
        let utc = |h, m, d| Utc.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap();
        assert_eq!(parse_since_at("2h", now).unwrap(), utc(13, 30, 28));
        assert_eq!(
            parse_since_at("90s", now).unwrap(),
            utc(15, 28, 28) + Duration::seconds(30)
        );
        assert_eq!(parse_since_at("1d", now).unwrap(), utc(15, 30, 27));
        assert_eq!(parse_since_at("5am", now).unwrap(), utc(9, 0, 28));
        assert_eq!(parse_since_at("5:30pm", now).unwrap(), utc(21, 30, 27));
        assert_eq!(parse_since_at("12am", now).unwrap(), utc(4, 0, 28));
        assert_eq!(parse_since_at("17:00", now).unwrap(), utc(21, 0, 27));
        assert_eq!(
            parse_since_at("2026-10-28T05:00:00-04:00", now).unwrap(),
            utc(9, 0, 28)
        );
        assert!(parse_since_at("13pm", now).is_err());
        assert!(parse_since_at("soon", now).is_err());
    }

    #[test]
    fn reader_names_are_plain_file_names() {
        assert!(parse_reader("voice-agent.1").is_ok());
        assert!(parse_reader("../x").is_err());
        assert!(parse_reader(".hidden").is_err());
        assert!(parse_reader("").is_err());
    }

    #[test]
    fn preview_is_the_first_lines_cut_to_a_length() {
        assert_eq!(preview("Done.\n\nAll tests pass."), "Done. All tests pass.");
        assert_eq!(preview("one\ntwo\nthree"), "one two…");
        assert_eq!(preview(&"x".repeat(300)).chars().count(), PREVIEW_CHARS + 1);
    }

    fn event(sequence: u64, minute: u32, kind: AgentEventKind, agent: &str) -> AgentEvent {
        AgentEvent {
            sequence,
            event_id: format!("e{sequence}"),
            kind,
            agent_id: agent.to_string(),
            incarnation_id: "i".to_string(),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 28, 12, minute, 0).unwrap(),
            turn_id: Some(format!("t{sequence}")),
            lifecycle: None,
            reason: None,
            turn_state: None,
            text: Some(format!("reply {sequence}")),
            outcome: Some("completed".to_string()),
            recoverable: None,
            detail: None,
            approval: None,
            input_sha256: None,
        }
    }

    /// A log of 60 events, one a minute, every third a turn final, with the
    /// oldest `dropped` no longer kept.
    fn log(dropped: u64) -> Vec<AgentEvent> {
        (dropped + 1..=60)
            .map(|sequence| {
                let kind = if sequence % 3 == 0 {
                    AgentEventKind::TurnFinal
                } else {
                    AgentEventKind::AssistantMessage
                };
                let agent = if sequence % 2 == 0 { "a" } else { "b" };
                event(sequence, (sequence - 1) as u32, kind, agent)
            })
            .collect()
    }

    fn pages(
        events: Vec<AgentEvent>,
    ) -> impl FnMut(u64, u32) -> std::future::Ready<anyhow::Result<AgentEventPage>> {
        move |after, limit| {
            let oldest = events.first().map_or(1, |event| event.sequence);
            let latest = events.last().map_or(0, |event| event.sequence);
            let too_old = after + 1 < oldest;
            let page: Vec<AgentEvent> = if too_old {
                vec![]
            } else {
                events
                    .iter()
                    .filter(|event| event.sequence > after)
                    .take(limit as usize)
                    .cloned()
                    .collect()
            };
            std::future::ready(Ok(AgentEventPage {
                schema: String::new(),
                status: if too_old {
                    AgentEventStatus::CursorTooOld
                } else {
                    AgentEventStatus::Ok
                },
                requested_after_sequence: after,
                oldest_available_sequence: oldest,
                latest_sequence: latest,
                next_after_sequence: page.last().map(|event| event.sequence),
                events: page,
                recovery: None,
            }))
        }
    }

    fn run<F: Future>(future: F) -> F::Output {
        smol::block_on(future)
    }

    #[test]
    fn since_reads_the_turns_observed_since_then() {
        let since = Utc.with_ymd_and_hms(2026, 10, 28, 12, 30, 0).unwrap();
        let read = run(read_turns(pages(log(0)), None, Some(since))).unwrap();
        // Finals are every third sequence; sequence 31 is minute 30.
        assert_eq!(
            read.finals.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [33, 36, 39, 42, 45, 48, 51, 54, 57, 60]
        );
        assert_eq!(read.through_sequence, 60);
        // The search started within the margin before `since`.
        assert!(
            read.after_sequence >= 15 && read.after_sequence <= 31,
            "{}",
            read.after_sequence
        );
    }

    #[test]
    fn a_reader_reads_after_its_position_and_notices_a_gap() {
        let read = run(read_turns(pages(log(0)), Some(54), None)).unwrap();
        assert_eq!(
            read.finals.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [57, 60]
        );
        assert!(!read.gap);
        let read = run(read_turns(pages(log(40)), Some(10), None)).unwrap();
        assert_eq!(read.finals.first().map(|e| e.sequence), Some(42));
        assert!(read.gap);
    }

    #[test]
    fn report_groups_turns_by_agent_newest_first() {
        let read = run(read_turns(pages(log(0)), Some(45), None)).unwrap();
        let names =
            BTreeMap::from([("a".to_string(), ("alpha".to_string(), "claude".to_string()))]);
        let report = Report::new(&read, &names, &[], false, None, Some("me".to_string()));
        assert_eq!(report.turns, 5);
        let summary: Vec<_> = report
            .agents
            .iter()
            .map(|agent| {
                (
                    agent.name.clone(),
                    agent.agent_id.clone(),
                    agent.turns,
                    agent.latest.sequence,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (Some("alpha".to_string()), "a".to_string(), 3, 60),
                (None, "b".to_string(), 2, 57)
            ]
        );
        assert!(report.table().contains("alpha  3 turns (3 completed)"));
        let only_b = Report::new(&read, &names, &["b".to_string()], true, None, None);
        assert_eq!(only_b.agents.len(), 1);
        assert_eq!(only_b.agents[0].all.as_ref().unwrap().len(), 2);
    }
}

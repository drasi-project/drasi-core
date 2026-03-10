// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! HERE Traffic real-time dashboard.
//!
//! A terminal UI that displays live traffic congestion and incidents from the
//! HERE Traffic API, powered by Drasi continuous queries.
//!
//! # Authentication
//! - **API key**: set `HERE_API_KEY`
//! - **OAuth 2.0**: set `HERE_ACCESS_KEY_ID` and `HERE_ACCESS_KEY_SECRET`
//!
//! # Configuration
//! - `HERE_BBOX`: bounding box as `lat1,lon1,lat2,lon2` (default: Berlin)

use std::collections::VecDeque;
use std::env;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tokio::sync::Mutex;

use drasi_bootstrap_here_traffic::HereTrafficBootstrapProvider;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_here_traffic::{Endpoint, HereTrafficSource};

const MAX_LOG: usize = 100;

// ─── Dashboard State ─────────────────────────────────────────────────────

struct App {
    bbox: String,
    segments: Vec<SegmentRow>,
    incidents: Vec<IncidentRow>,
    log: VecDeque<LogEntry>,
    adds: usize,
    updates: usize,
    deletes: usize,
    last_update: Option<DateTime<Utc>>,
}

#[derive(Clone)]
struct SegmentRow {
    id: String,
    road: String,
    jam: f64,
    speed: f64,
    free_flow: f64,
    confidence: f64,
}

#[derive(Clone)]
struct IncidentRow {
    id: String,
    kind: String,
    severity: String,
    description: String,
}

struct LogEntry {
    time: DateTime<Local>,
    kind: ChangeKind,
    message: String,
}

#[derive(Clone, Copy)]
enum ChangeKind {
    Add,
    Update,
    Delete,
}

impl App {
    fn new(bbox: String) -> Self {
        Self {
            bbox,
            segments: Vec::new(),
            incidents: Vec::new(),
            log: VecDeque::new(),
            adds: 0,
            updates: 0,
            deletes: 0,
            last_update: None,
        }
    }

    fn push_log(&mut self, kind: ChangeKind, message: String) {
        self.log.push_front(LogEntry {
            time: Local::now(),
            kind,
            message,
        });
        if self.log.len() > MAX_LOG {
            self.log.pop_back();
        }
    }

    fn process(&mut self, result: QueryResult) {
        self.last_update = Some(result.timestamp);
        for diff in &result.results {
            match diff {
                ResultDiff::Add { data } => {
                    self.adds += 1;
                    if result.query_id == "traffic-segments" {
                        self.add_segment(data);
                    } else {
                        self.add_incident(data);
                    }
                }
                ResultDiff::Update {
                    before, after, ..
                } => {
                    self.updates += 1;
                    if result.query_id == "traffic-segments" {
                        self.update_segment(before, after);
                    } else {
                        self.update_incident(after);
                    }
                }
                ResultDiff::Delete { data } => {
                    self.deletes += 1;
                    if result.query_id == "traffic-segments" {
                        self.delete_segment(data);
                    } else {
                        self.delete_incident(data);
                    }
                }
                _ => {}
            }
        }
        // Keep segments sorted by jam factor (worst first)
        self.segments
            .sort_by(|a, b| b.jam.partial_cmp(&a.jam).unwrap_or(std::cmp::Ordering::Equal));
    }

    fn add_segment(&mut self, data: &serde_json::Value) {
        let road = jstr(data, "road");
        let jam = jf64(data, "jam");
        let speed = jf64(data, "speed");
        self.segments.push(SegmentRow {
            id: jstr(data, "id"),
            road: road.clone(),
            jam,
            speed,
            free_flow: jf64(data, "free_flow"),
            confidence: jf64(data, "confidence"),
        });
        self.push_log(
            ChangeKind::Add,
            format!("{road}  jam={jam:.1}  speed={speed:.0} km/h"),
        );
    }

    fn update_segment(&mut self, before: &serde_json::Value, after: &serde_json::Value) {
        let id = jstr(after, "id");
        let road = jstr(after, "road");
        let jam_before = jf64(before, "jam");
        let jam_after = jf64(after, "jam");
        if let Some(seg) = self.segments.iter_mut().find(|s| s.id == id) {
            seg.road = road.clone();
            seg.jam = jam_after;
            seg.speed = jf64(after, "speed");
            seg.free_flow = jf64(after, "free_flow");
            seg.confidence = jf64(after, "confidence");
        }
        self.push_log(
            ChangeKind::Update,
            format!("{road}  jam {jam_before:.1} -> {jam_after:.1}"),
        );
    }

    fn delete_segment(&mut self, data: &serde_json::Value) {
        let id = jstr(data, "id");
        let road = jstr(data, "road");
        self.segments.retain(|s| s.id != id);
        self.push_log(ChangeKind::Delete, format!("{road} cleared"));
    }

    fn add_incident(&mut self, data: &serde_json::Value) {
        let id = jstr(data, "id");
        let kind = jstr(data, "type");
        let desc = jstr(data, "description");
        self.incidents.push(IncidentRow {
            id: id.clone(),
            kind: kind.clone(),
            severity: jstr(data, "severity"),
            description: desc.clone(),
        });
        self.push_log(ChangeKind::Add, format!("Incident {kind}: {desc}"));
    }

    fn update_incident(&mut self, after: &serde_json::Value) {
        let id = jstr(after, "id");
        if let Some(inc) = self.incidents.iter_mut().find(|i| i.id == id) {
            inc.kind = jstr(after, "type");
            inc.severity = jstr(after, "severity");
            inc.description = jstr(after, "description");
        }
        self.push_log(
            ChangeKind::Update,
            format!("Incident {} updated", jstr(after, "type")),
        );
    }

    fn delete_incident(&mut self, data: &serde_json::Value) {
        let id = jstr(data, "id");
        self.incidents.retain(|i| i.id != id);
        self.push_log(ChangeKind::Delete, format!("Incident {id} resolved"));
    }
}

fn jf64(v: &serde_json::Value, key: &str) -> f64 {
    v[key].as_f64().unwrap_or(0.0)
}

fn jstr(v: &serde_json::Value, key: &str) -> String {
    match &v[key] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ─── TUI Rendering ───────────────────────────────────────────────────────

fn ui(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(6),    // segments
            Constraint::Length(8), // incidents
            Constraint::Length(9), // change log
        ])
        .split(area);

    render_header(frame, app, chunks[0]);
    render_segments(frame, app, chunks[1]);
    render_incidents(frame, app, chunks[2]);
    render_log(frame, app, chunks[3]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let elapsed = app
        .last_update
        .map(|t| {
            let secs = (Utc::now() - t).num_seconds();
            if secs < 60 {
                format!("{secs}s ago")
            } else {
                format!("{}m ago", secs / 60)
            }
        })
        .unwrap_or_else(|| "waiting...".into());

    let line = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(&app.bbox, Style::default().fg(Color::Cyan)),
        Span::raw("   "),
        Span::styled(
            format!("Segments: {}", app.segments.len()),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Incidents: {}", app.incidents.len()),
            Style::default().fg(Color::LightRed),
        ),
        Span::raw("   "),
        Span::styled(
            format!("+{}  ~{}  -{}", app.adds, app.updates, app.deletes),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled(elapsed, Style::default().fg(Color::DarkGray)),
    ]);

    let block = Block::default()
        .title(
            " HERE Traffic Dashboard "
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .title_bottom(" [q] quit ".fg(Color::DarkGray))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(line).block(block);
    frame.render_widget(paragraph, area);
}

fn render_segments(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Congested Segments ".fg(Color::Red).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    if app.segments.is_empty() {
        let msg = if app.last_update.is_some() {
            "No congested segments in this area"
        } else {
            "Waiting for first poll..."
        };
        let p = Paragraph::new(msg)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(p, area);
        return;
    }

    let header = Row::new(vec!["Road", "Jam Factor", "Speed", "Free Flow", "Conf"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(0);

    let rows: Vec<Row> = app
        .segments
        .iter()
        .map(|s| {
            let color = jam_color(s.jam);
            Row::new(vec![
                Cell::from(truncate(&s.road, 28)),
                Cell::from(jam_bar(s.jam)).style(Style::default().fg(color)),
                Cell::from(format!("{:>5.0} km/h", s.speed)),
                Cell::from(format!("{:>5.0} km/h", s.free_flow)),
                Cell::from(format!("{:>4.0}%", s.confidence)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(16),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(6),
    ];

    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_incidents(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(
            " Active Incidents "
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    if app.incidents.is_empty() {
        let msg = if app.last_update.is_some() {
            "No active incidents"
        } else {
            "Waiting for first poll..."
        };
        let p = Paragraph::new(msg)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(p, area);
        return;
    }

    let header = Row::new(vec!["Type", "Sev", "Description"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .incidents
        .iter()
        .map(|i| {
            Row::new(vec![
                Cell::from(truncate(&i.kind, 14)),
                Cell::from(format!(" {} ", i.severity)).style(Style::default().fg(Color::Red)),
                Cell::from(truncate(&i.description, 60)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(5),
        Constraint::Min(30),
    ];

    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_log(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(
            " Recent Changes "
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    if app.log.is_empty() {
        let p = Paragraph::new("No changes yet")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(p, area);
        return;
    }

    let max_visible = (area.height.saturating_sub(2)) as usize;
    let items: Vec<ListItem> = app
        .log
        .iter()
        .take(max_visible)
        .map(|entry| {
            let (symbol, color) = match entry.kind {
                ChangeKind::Add => ("+", Color::Green),
                ChangeKind::Update => ("~", Color::Yellow),
                ChangeKind::Delete => ("-", Color::Red),
            };
            let time = entry.time.format("%H:%M:%S").to_string();
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {time} "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("[{symbol}] "),
                    Style::default()
                        .fg(color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.message.clone(), Style::default().fg(color)),
            ]))
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn jam_bar(jam: f64) -> String {
    let filled = (jam.clamp(0.0, 10.0) as usize).min(10);
    let empty = 10 - filled;
    format!(
        "{}{} {:>4.1}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        jam
    )
}

fn jam_color(jam: f64) -> Color {
    if jam >= 8.0 {
        Color::Red
    } else if jam >= 6.0 {
        Color::LightRed
    } else if jam >= 4.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

// ─── Source Setup ─────────────────────────────────────────────────────────

fn build_source(
    api_key: &Option<String>,
    access_key_id: &Option<String>,
    access_key_secret: &Option<String>,
    bbox: &str,
) -> Result<drasi_source_here_traffic::HereTrafficSource> {
    match (api_key, access_key_id, access_key_secret) {
        (Some(key), None, None) => {
            let bp = HereTrafficBootstrapProvider::builder()
                .with_source_id("traffic")
                .with_api_key(key)
                .with_bounding_box(bbox)
                .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
                .build()?;

            Ok(HereTrafficSource::builder("traffic", key, bbox.to_string())
                .with_polling_interval(Duration::from_secs(60))
                .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
                .with_bootstrap_provider(bp)
                .build()?)
        }
        (None, Some(kid), Some(ks)) => {
            let bp = HereTrafficBootstrapProvider::builder()
                .with_source_id("traffic")
                .with_oauth(kid, ks)
                .with_bounding_box(bbox)
                .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
                .build()?;

            Ok(
                HereTrafficSource::builder_oauth("traffic", kid, ks, bbox.to_string())
                    .with_polling_interval(Duration::from_secs(60))
                    .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
                    .with_bootstrap_provider(bp)
                    .build()?,
            )
        }
        _ => Err(anyhow::anyhow!(
            "Set HERE_API_KEY or (HERE_ACCESS_KEY_ID + HERE_ACCESS_KEY_SECRET)"
        )),
    }
}

// ─── Main ─────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let bbox = env::var("HERE_BBOX").unwrap_or_else(|_| "47.5,-122.4,47.7,-122.2".to_string());
    let api_key = env::var("HERE_API_KEY").ok();
    let access_key_id = env::var("HERE_ACCESS_KEY_ID").ok();
    let access_key_secret = env::var("HERE_ACCESS_KEY_SECRET").ok();

    eprintln!("Starting HERE Traffic Dashboard...");

    let source = build_source(&api_key, &access_key_id, &access_key_secret, &bbox)?;

    let segments_query = Query::cypher("traffic-segments")
        .query(
            r#"
            MATCH (s:TrafficSegment)
            WHERE s.jam_factor > 2.0
            RETURN s.id AS id, s.road_name AS road, s.jam_factor AS jam,
                   s.current_speed AS speed, s.free_flow_speed AS free_flow,
                   s.confidence AS confidence
            "#,
        )
        .from_source("traffic")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let incidents_query = Query::cypher("traffic-incidents")
        .query(
            r#"
            MATCH (i:TrafficIncident)
            RETURN i.id AS id, i.type AS type, i.severity AS severity,
                   i.description AS description
            "#,
        )
        .from_source("traffic")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("dashboard")
        .with_query("traffic-segments")
        .with_query("traffic-incidents")
        .with_auto_start(true)
        .build();

    let core = DrasiLib::builder()
        .with_id("here-traffic-dashboard")
        .with_source(source)
        .with_query(segments_query)
        .with_query(incidents_query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;

    let mut subscription = handle.subscribe_with_options(Default::default()).await?;

    let state = Arc::new(Mutex::new(App::new(bbox)));

    // Receiver task: feed query results into app state
    let state_rx = state.clone();
    let receiver = tokio::spawn(async move {
        while let Some(result) = subscription.recv().await {
            state_rx.lock().await.process(result);
        }
    });

    // Install panic hook so terminal is restored on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    // Enter TUI
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Render loop
    loop {
        {
            let app = state.lock().await;
            terminal.draw(|frame| ui(frame, &app))?;
        }

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                    break;
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    receiver.abort();
    core.stop().await?;

    eprintln!("Dashboard stopped.");
    Ok(())
}

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

use anyhow::Result;
use axum::{
    extract::{Path, State},
    response::Html,
    routing::get,
    Json, Router,
};
use drasi_bootstrap_cloudflare_radar::CloudflareRadarBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_cloudflare_radar::{CloudflareRadarSource, StartBehavior};
use std::env;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let api_token = env::var("CF_RADAR_TOKEN")
        .map_err(|_| anyhow::anyhow!("CF_RADAR_TOKEN is required"))?;
    let api_base_url = env::var("CF_RADAR_API_BASE_URL")
        .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".to_string());
    let poll_interval_secs = env::var("CF_RADAR_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(300);

    let bootstrap_provider = CloudflareRadarBootstrapProvider::builder()
        .with_api_token(&api_token)
        .with_api_base_url(&api_base_url)
        .with_category("outages", true)
        .with_category("bgp_hijacks", true)
        .with_category("domain_rankings", true)
        .build()?;

    let source = CloudflareRadarSource::builder("radar")
        .with_api_token(api_token)
        .with_api_base_url(api_base_url)
        .with_poll_interval_secs(poll_interval_secs)
        .with_start_behavior(StartBehavior::StartFromBeginning)
        .with_category("outages", true)
        .with_category("bgp_hijacks", true)
        .with_category("domain_rankings", true)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let outages_query = Query::cypher("active-outages")
        .query(
            r#"
            MATCH (o:Outage)-[:AFFECTS_LOCATION]->(l:Location)
            WHERE o.endDate IS NULL
            RETURN o.scope AS scope,
                   o.outageCause AS cause,
                   o.startDate AS started,
                   l.code AS country
        "#,
        )
        .from_source("radar")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let hijack_query = Query::cypher("hijacks")
        .query(
            r#"
            MATCH (h:BgpHijack)-[:HIJACKED_BY]->(hijacker:AutonomousSystem)
            MATCH (h)-[:VICTIM_ASN]->(victim:AutonomousSystem)
            RETURN h.eventId AS eventId,
                   hijacker.asn AS hijackerAsn,
                   victim.asn AS victimAsn,
                   h.confidenceScore AS confidence
        "#,
        )
        .from_source("radar")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let ranking_query = Query::cypher("top-domains")
        .query(
            r#"
            MATCH (d:Domain)-[:RANKED_AS]->(r:DomainRanking)
            RETURN d.name AS domain, r.rank AS rank
        "#,
        )
        .from_source("radar")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let log_reaction = LogReaction::builder("radar-logger")
        .from_query("active-outages")
        .from_query("hijacks")
        .from_query("top-domains")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("[{{query_name}}] + {{after}}")),
            updated: Some(TemplateSpec::new("[{{query_name}}] ~ {{before}} -> {{after}}")),
            deleted: Some(TemplateSpec::new("[{{query_name}}] - {{before}}")),
        })
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("radar-dashboard")
            .with_source(source)
            .with_query(outages_query)
            .with_query(hijack_query)
            .with_query(ranking_query)
            .with_reaction(log_reaction)
            .build()
            .await?,
    );

    core.start().await?;

    let api_core = core.clone();
    let results_api = Router::new()
        .route("/", get(serve_dashboard))
        .route("/queries/:id/results", get(get_query_results))
        .with_state(api_core);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("Cloudflare Radar dashboard running.");
    println!("Dashboard:   http://localhost:8080/");
    println!("Results API: http://localhost:8080/queries/<query-id>/results");

    tokio::signal::ctrl_c().await?;
    api_handle.abort();
    core.stop().await?;
    Ok(())
}

async fn get_query_results(
    State(core): State<Arc<DrasiLib>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, (axum::http::StatusCode, String)> {
    core.get_query_results(&id)
        .await
        .map(Json)
        .map_err(|e| (axum::http::StatusCode::NOT_FOUND, e.to_string()))
}

async fn serve_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Cloudflare Radar Dashboard</title>
<style>
  :root {
    --bg: #0f1117;
    --card: #1a1d27;
    --border: #2a2d3a;
    --text: #e1e4eb;
    --muted: #8b8fa3;
    --accent: #f6821f;
    --green: #34d399;
    --red: #f87171;
    --yellow: #fbbf24;
    --blue: #60a5fa;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
    background: var(--bg);
    color: var(--text);
    padding: 24px;
  }
  header {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 32px;
    padding-bottom: 16px;
    border-bottom: 1px solid var(--border);
  }
  header svg { flex-shrink: 0; }
  header h1 { font-size: 24px; font-weight: 600; }
  header .status {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--muted);
  }
  header .dot {
    width: 8px; height: 8px;
    border-radius: 50%;
    background: var(--green);
    animation: pulse 2s infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.4; }
  }
  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 24px;
  }
  @media (max-width: 900px) { .grid { grid-template-columns: 1fr; } }
  .card {
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: 12px;
    overflow: hidden;
  }
  .card.full { grid-column: 1 / -1; }
  .card-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
  }
  .card-header h2 { font-size: 15px; font-weight: 600; }
  .card-header .badge {
    font-size: 12px;
    padding: 2px 10px;
    border-radius: 99px;
    font-weight: 600;
  }
  .badge-red { background: rgba(248,113,113,0.15); color: var(--red); }
  .badge-yellow { background: rgba(251,191,36,0.15); color: var(--yellow); }
  .badge-blue { background: rgba(96,165,250,0.15); color: var(--blue); }
  .badge-green { background: rgba(52,211,153,0.15); color: var(--green); }
  .card-body { padding: 0; }
  .card-body.pad { padding: 20px; }
  table { width: 100%; border-collapse: collapse; }
  th {
    text-align: left;
    padding: 10px 20px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    border-bottom: 1px solid var(--border);
  }
  td {
    padding: 12px 20px;
    font-size: 13px;
    border-bottom: 1px solid var(--border);
  }
  tr:last-child td { border-bottom: none; }
  tr:hover { background: rgba(255,255,255,0.02); }
  .confidence {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 4px;
    font-size: 12px;
    font-weight: 600;
  }
  .conf-high { background: rgba(248,113,113,0.15); color: var(--red); }
  .conf-med { background: rgba(251,191,36,0.15); color: var(--yellow); }
  .conf-low { background: rgba(52,211,153,0.15); color: var(--green); }
  .rank-num {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px; height: 24px;
    border-radius: 4px;
    font-size: 12px;
    font-weight: 700;
    background: rgba(246,130,31,0.12);
    color: var(--accent);
  }
  .empty {
    padding: 40px 20px;
    text-align: center;
    color: var(--muted);
    font-size: 14px;
  }
  .updated {
    font-size: 12px;
    color: var(--muted);
    text-align: right;
    padding: 12px 20px;
    border-top: 1px solid var(--border);
  }
  .country-flag { margin-right: 6px; }
</style>
</head>
<body>

<header>
  <svg width="32" height="32" viewBox="0 0 32 32" fill="none">
    <circle cx="16" cy="16" r="15" stroke="#f6821f" stroke-width="2"/>
    <path d="M16 6 L16 16 L24 16" stroke="#f6821f" stroke-width="2" stroke-linecap="round"/>
    <circle cx="16" cy="16" r="3" fill="#f6821f"/>
  </svg>
  <h1>Cloudflare Radar Dashboard</h1>
  <div class="status">
    <div class="dot"></div>
    <span id="status-text">Connecting…</span>
  </div>
</header>

<div class="grid">
  <!-- Outages -->
  <div class="card">
    <div class="card-header">
      <h2>🌐 Active Outages</h2>
      <span class="badge badge-red" id="outage-count">—</span>
    </div>
    <div class="card-body" id="outages-body">
      <div class="empty">Loading…</div>
    </div>
    <div class="updated" id="outages-updated"></div>
  </div>

  <!-- BGP Hijacks -->
  <div class="card">
    <div class="card-header">
      <h2>🛡️ BGP Hijacks</h2>
      <span class="badge badge-yellow" id="hijack-count">—</span>
    </div>
    <div class="card-body" id="hijacks-body">
      <div class="empty">Loading…</div>
    </div>
    <div class="updated" id="hijacks-updated"></div>
  </div>

  <!-- Top Domains -->
  <div class="card full">
    <div class="card-header">
      <h2>📊 Top Domain Rankings</h2>
      <span class="badge badge-blue" id="domain-count">—</span>
    </div>
    <div class="card-body" id="domains-body">
      <div class="empty">Loading…</div>
    </div>
    <div class="updated" id="domains-updated"></div>
  </div>
</div>

<script>
const REFRESH_MS = 10000;

function countryFlag(code) {
  if (!code || code.length !== 2) return '';
  const offset = 0x1F1E6;
  const a = code.toUpperCase().charCodeAt(0) - 65 + offset;
  const b = code.toUpperCase().charCodeAt(1) - 65 + offset;
  return String.fromCodePoint(a) + String.fromCodePoint(b);
}

function timeAgo(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d)) return iso;
  const s = Math.floor((Date.now() - d) / 1000);
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  if (s < 86400) return Math.floor(s / 3600) + 'h ago';
  return Math.floor(s / 86400) + 'd ago';
}

function confClass(v) {
  const n = Number(v);
  if (n >= 80) return 'conf-high';
  if (n >= 50) return 'conf-med';
  return 'conf-low';
}

function renderOutages(data) {
  const el = document.getElementById('outages-body');
  const badge = document.getElementById('outage-count');
  badge.textContent = data.length;
  if (!data.length) {
    el.innerHTML = '<div class="empty">No active outages — all clear ✅</div>';
    badge.className = 'badge badge-green';
    return;
  }
  badge.className = 'badge badge-red';
  let html = '<table><tr><th>Location</th><th>Scope</th><th>Cause</th><th>Started</th></tr>';
  for (const r of data) {
    html += '<tr>';
    html += '<td><span class="country-flag">' + countryFlag(r.country) + '</span>' + (r.country || '—') + '</td>';
    html += '<td>' + (r.scope || '—') + '</td>';
    html += '<td>' + (r.cause || '—') + '</td>';
    html += '<td>' + timeAgo(r.started) + '</td>';
    html += '</tr>';
  }
  html += '</table>';
  el.innerHTML = html;
}

function renderHijacks(data) {
  const el = document.getElementById('hijacks-body');
  const badge = document.getElementById('hijack-count');
  badge.textContent = data.length;
  if (!data.length) {
    el.innerHTML = '<div class="empty">No BGP hijacks detected ✅</div>';
    badge.className = 'badge badge-green';
    return;
  }
  badge.className = 'badge badge-yellow';
  let html = '<table><tr><th>Event</th><th>Hijacker ASN</th><th>Victim ASN</th><th>Confidence</th></tr>';
  for (const r of data) {
    const cls = confClass(r.confidence);
    html += '<tr>';
    html += '<td style="font-family:monospace;font-size:12px">' + (r.eventId || '—') + '</td>';
    html += '<td>AS' + (r.hijackerAsn || '?') + '</td>';
    html += '<td>AS' + (r.victimAsn || '?') + '</td>';
    html += '<td><span class="confidence ' + cls + '">' + (r.confidence || '?') + '%</span></td>';
    html += '</tr>';
  }
  html += '</table>';
  el.innerHTML = html;
}

function renderDomains(data) {
  const el = document.getElementById('domains-body');
  const badge = document.getElementById('domain-count');
  const sorted = data.slice().sort((a, b) => Number(a.rank) - Number(b.rank));
  badge.textContent = sorted.length;
  if (!sorted.length) {
    el.innerHTML = '<div class="empty">No domain rankings loaded yet…</div>';
    return;
  }
  let html = '<table><tr><th>Rank</th><th>Domain</th></tr>';
  for (const r of sorted.slice(0, 50)) {
    html += '<tr>';
    html += '<td><span class="rank-num">' + r.rank + '</span></td>';
    html += '<td>' + (r.domain || '—') + '</td>';
    html += '</tr>';
  }
  if (sorted.length > 50) {
    html += '<tr><td colspan="2" style="text-align:center;color:var(--muted);padding:12px">… and ' + (sorted.length - 50) + ' more</td></tr>';
  }
  html += '</table>';
  el.innerHTML = html;
}

function stamp(id) {
  document.getElementById(id).textContent = 'Updated ' + new Date().toLocaleTimeString();
}

async function refresh() {
  try {
    const [outages, hijacks, domains] = await Promise.all([
      fetch('/queries/active-outages/results').then(r => r.ok ? r.json() : []),
      fetch('/queries/hijacks/results').then(r => r.ok ? r.json() : []),
      fetch('/queries/top-domains/results').then(r => r.ok ? r.json() : []),
    ]);
    renderOutages(outages);
    renderHijacks(hijacks);
    renderDomains(domains);
    stamp('outages-updated');
    stamp('hijacks-updated');
    stamp('domains-updated');
    document.getElementById('status-text').textContent = 'Live';
  } catch (e) {
    document.getElementById('status-text').textContent = 'Error: ' + e.message;
  }
}

refresh();
setInterval(refresh, REFRESH_MS);
</script>
</body>
</html>
"##;

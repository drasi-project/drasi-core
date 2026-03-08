let map;
let routeChart;
const markers = new Map();
const seenAlerts = new Set();

function initMap() {
  map = L.map('vehicle-map', { preferCanvas: true }).setView([39.7392, -104.9903], 11);
  L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png', {
    maxZoom: 19,
    attribution: '&copy; OpenStreetMap contributors'
  }).addTo(map);
}

function markerColor(delayColor) {
  switch ((delayColor || '').toLowerCase()) {
    case 'red':
      return '#ef4444';
    case 'yellow':
      return '#f59e0b';
    default:
      return '#10b981';
  }
}

function updateMap(vehicles) {
  const nextIds = new Set();

  vehicles.forEach((vehicle) => {
    if (typeof vehicle.latitude !== 'number' || typeof vehicle.longitude !== 'number') {
      return;
    }

    nextIds.add(vehicle.entityId);
    const color = markerColor(vehicle.delayColor);
    const popup = `
      <strong>${vehicle.vehicleLabel || vehicle.vehicleId}</strong><br/>
      Vehicle: ${vehicle.vehicleId}<br/>
      Route: ${vehicle.routeId || 'N/A'}<br/>
      Delay: ${Math.round((vehicle.delaySeconds || 0) / 60)} min<br/>
      Speed: ${vehicle.speed != null ? `${vehicle.speed.toFixed(1)} m/s` : 'N/A'}
    `;

    const existing = markers.get(vehicle.entityId);
    if (existing) {
      existing.setLatLng([vehicle.latitude, vehicle.longitude]);
      existing.setStyle({ color, fillColor: color });
      existing.setPopupContent(popup);
    } else {
      const marker = L.circleMarker([vehicle.latitude, vehicle.longitude], {
        radius: 7,
        color,
        fillColor: color,
        fillOpacity: 0.75,
        weight: 2
      }).addTo(map);
      marker.bindPopup(popup);
      markers.set(vehicle.entityId, marker);
    }
  });

  for (const [entityId, marker] of markers.entries()) {
    if (!nextIds.has(entityId)) {
      map.removeLayer(marker);
      markers.delete(entityId);
    }
  }

  document.getElementById('vehicle-count').textContent = `${vehicles.length} vehicles`;
}

function updateLeaderboard(entries) {
  const tbody = document.querySelector('#delay-table tbody');
  tbody.innerHTML = '';

  if (!entries.length) {
    tbody.innerHTML = '<tr><td colspan="4" class="empty">No delayed trips currently detected.</td></tr>';
    return;
  }

  entries.forEach((entry) => {
    const row = document.createElement('tr');
    if (entry.delaySeconds > 600) {
      row.classList.add('high-delay');
    }

    const tripCell = document.createElement('td');
    tripCell.textContent = entry.tripId || entry.entityId;

    const routeCell = document.createElement('td');
    routeCell.textContent = entry.routeId || 'N/A';

    const vehicleCell = document.createElement('td');
    vehicleCell.textContent = entry.vehicleLabel || 'Unknown';

    const delayCell = document.createElement('td');
    delayCell.textContent = `${entry.delayMinutes.toFixed(1)} min`;

    row.appendChild(tripCell);
    row.appendChild(routeCell);
    row.appendChild(vehicleCell);
    row.appendChild(delayCell);
    tbody.appendChild(row);
  });
}

function severityClass(severity) {
  const value = (severity || 'unknown').toLowerCase();
  if (value.includes('severe')) return 'severity-severe';
  if (value.includes('warning')) return 'severity-warning';
  if (value.includes('info')) return 'severity-info';
  return 'severity-unknown';
}

function updateAlerts(alerts) {
  const container = document.getElementById('alerts-container');
  container.innerHTML = '';
  document.getElementById('alert-count').textContent = `${alerts.length} alerts`;

  if (!alerts.length) {
    container.innerHTML = '<div class="empty">No active alerts.</div>';
    return;
  }

  alerts.forEach((alert) => {
    const isNew = !seenAlerts.has(alert.alertId);
    seenAlerts.add(alert.alertId);

    const card = document.createElement('article');
    card.className = `alert-card ${severityClass(alert.severityLevel)} ${isNew ? 'alert-new' : ''}`;

    const severityBadge = document.createElement('span');
    severityBadge.className = 'alert-badge';
    severityBadge.textContent = alert.severityLevel || 'UNKNOWN';

    const header = document.createElement('strong');
    header.textContent = alert.headerText || 'Service alert';

    const description = document.createElement('p');
    description.className = 'alert-text';
    description.textContent = alert.descriptionText || 'No description provided.';

    const meta = document.createElement('div');
    meta.className = 'alert-meta';
    meta.textContent = `Cause: ${alert.cause || 'N/A'} · Effect: ${alert.effect || 'N/A'}`;

    const routesContainer = document.createElement('div');
    routesContainer.className = 'badge-list';
    const routeIds = alert.routeIds || [];
    if (routeIds.length) {
      routeIds.forEach((route) => {
        const routeBadge = document.createElement('span');
        routeBadge.className = 'badge';
        routeBadge.textContent = `Route ${route}`;
        routesContainer.appendChild(routeBadge);
      });
    } else {
      const noRouteBadge = document.createElement('span');
      noRouteBadge.className = 'badge';
      noRouteBadge.textContent = 'No route scope';
      routesContainer.appendChild(noRouteBadge);
    }

    const stopsContainer = document.createElement('div');
    stopsContainer.className = 'badge-list';
    const stopIds = alert.stopIds || [];
    if (stopIds.length) {
      stopIds.forEach((stop) => {
        const stopBadge = document.createElement('span');
        stopBadge.className = 'badge';
        stopBadge.textContent = `Stop ${stop}`;
        stopsContainer.appendChild(stopBadge);
      });
    } else {
      const noStopBadge = document.createElement('span');
      noStopBadge.className = 'badge';
      noStopBadge.textContent = 'No stop scope';
      stopsContainer.appendChild(noStopBadge);
    }

    card.appendChild(severityBadge);
    card.appendChild(header);
    card.appendChild(description);
    card.appendChild(meta);
    card.appendChild(routesContainer);
    card.appendChild(stopsContainer);

    container.appendChild(card);
  });
}

function updateRouteChart(stats) {
  const labels = stats.map((item) => item.routeId);
  const values = stats.map((item) => item.vehicleCount);

  if (!routeChart) {
    const ctx = document.getElementById('route-chart').getContext('2d');
    routeChart = new Chart(ctx, {
      type: 'bar',
      data: {
        labels,
        datasets: [
          {
            label: 'Vehicle Count',
            data: values,
            backgroundColor: 'rgba(56, 189, 248, 0.45)',
            borderColor: 'rgba(56, 189, 248, 1)',
            borderWidth: 1
          }
        ]
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        scales: {
          x: { ticks: { color: '#cbd5e1' }, grid: { color: 'rgba(51,65,85,0.4)' } },
          y: {
            beginAtZero: true,
            ticks: { color: '#cbd5e1', precision: 0 },
            grid: { color: 'rgba(51,65,85,0.4)' }
          }
        },
        plugins: {
          legend: { labels: { color: '#cbd5e1' } }
        }
      }
    });
    return;
  }

  routeChart.data.labels = labels;
  routeChart.data.datasets[0].data = values;
  routeChart.update();
}

function setConnectionState(state) {
  const pill = document.getElementById('connection-pill');
  pill.classList.remove('pill-ok', 'pill-warn', 'pill-error');

  if (state === 'ok') {
    pill.classList.add('pill-ok');
    pill.textContent = 'Live';
  } else if (state === 'error') {
    pill.classList.add('pill-error');
    pill.textContent = 'Disconnected';
  } else {
    pill.classList.add('pill-warn');
    pill.textContent = 'Connecting...';
  }
}

function renderDashboard(payload) {
  if (!payload) return;

  updateMap(payload.vehicles || []);
  updateLeaderboard(payload.delayLeaderboard || []);
  updateAlerts(payload.alerts || []);
  updateRouteChart(payload.routeStatistics || []);

  const timestamp = payload.generatedAt ? new Date(payload.generatedAt).toLocaleTimeString() : 'N/A';
  document.getElementById('generated-at').textContent = `Updated ${timestamp}`;
}

async function loadInitialState() {
  try {
    const response = await fetch('/api/dashboard/state', { cache: 'no-store' });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const payload = await response.json();
    renderDashboard(payload);
    setConnectionState('warn');
  } catch (error) {
    console.error('Failed to load initial dashboard state', error);
    setConnectionState('error');
  }
}

function connectEventStream() {
  const stream = new EventSource('/api/dashboard/events');

  stream.addEventListener('open', () => {
    setConnectionState('ok');
  });

  stream.addEventListener('dashboard', (event) => {
    try {
      const payload = JSON.parse(event.data);
      renderDashboard(payload);
      setConnectionState('ok');
    } catch (error) {
      console.error('Failed to parse dashboard event', error);
    }
  });

  stream.onerror = () => {
    setConnectionState('error');
  };
}

document.addEventListener('DOMContentLoaded', async () => {
  initMap();
  await loadInitialState();
  connectEventStream();
});

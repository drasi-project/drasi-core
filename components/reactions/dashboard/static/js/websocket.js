export class DashboardSocket {
  constructor(path = "/ws") {
    this.path = path;
    this.socket = null;
    this.subscriptions = new Set();
    this.queryHandlers = [];
    this.snapshotHandlers = [];
    this.statusHandlers = [];
    this.pendingSnapshots = new Set();
    this.reconnectDelayMs = 1000;
    this.reconnectTimer = null;
    this.stopped = true;
  }

  start() {
    if (!this.stopped && this.socket) return;
    this.stopped = false;
    this.connect();
  }

  stop() {
    this.stopped = true;
    window.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    const socket = this.socket;
    this.socket = null;
    this.pendingSnapshots.clear();
    if (socket) socket.close();
    this.notifyStatus("Disconnected");
  }

  onQueryResult(handler) {
    this.queryHandlers.push(handler);
  }

  onQuerySnapshot(handler) {
    this.snapshotHandlers.push(handler);
  }

  onStatus(handler) {
    this.statusHandlers.push(handler);
  }

  subscribe(queryIds) {
    const requested = new Set();
    for (const queryId of queryIds) {
      if (queryId && queryId.trim().length > 0) {
        this.subscriptions.add(queryId);
        requested.add(queryId);
      }
    }

    if (this.socket?.readyState === WebSocket.OPEN && requested.size > 0) {
      for (const queryId of requested) this.pendingSnapshots.add(queryId);
      this.notifyStatus("Synchronizing");
      this.send({ type: "subscribe", query_ids: Array.from(requested) });
    }
  }

  unsubscribe(queryIds) {
    for (const queryId of queryIds) {
      this.subscriptions.delete(queryId);
      this.pendingSnapshots.delete(queryId);
    }

    this.send({
      type: "unsubscribe",
      query_ids: queryIds,
    });
    if (this.socket?.readyState === WebSocket.OPEN && this.pendingSnapshots.size === 0) {
      this.notifyStatus("Connected");
    }
  }

  connect() {
    if (this.stopped) return;
    window.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    const protocol = window.location.protocol === "https:" ? "wss" : "ws";
    const url = `${protocol}://${window.location.host}${this.path}`;

    const socket = new WebSocket(url);
    this.socket = socket;
    const isCurrent = () => !this.stopped && this.socket === socket;
    this.notifyStatus("Connecting");

    socket.onopen = () => {
      if (!isCurrent()) return;
      this.reconnectDelayMs = 1000;
      this.pendingSnapshots.clear();
      if (this.subscriptions.size > 0) {
        this.subscribe(Array.from(this.subscriptions));
      } else {
        this.notifyStatus("Connected");
      }
    };

    socket.onmessage = (event) => {
      if (!isCurrent()) return;
      let payload = null;
      try {
        payload = JSON.parse(event.data);
      } catch (error) {
        console.warn("Ignoring invalid websocket payload", error);
        return;
      }

      if (payload.type === "query_snapshot") {
        for (const handler of this.snapshotHandlers) handler(payload);
        this.pendingSnapshots.delete(payload.query_id);
        if (this.pendingSnapshots.size === 0) this.notifyStatus("Connected");
      } else if (payload.type === "query_result") {
        for (const handler of this.queryHandlers) {
          handler(payload);
        }
      }
    };

    socket.onclose = () => {
      if (!isCurrent()) return;
      this.socket = null;
      this.notifyStatus("Disconnected");
      const timer = window.setTimeout(() => {
        if (this.reconnectTimer !== timer || this.stopped || this.socket) return;
        this.reconnectTimer = null;
        this.connect();
      }, this.reconnectDelayMs);
      this.reconnectTimer = timer;
      this.reconnectDelayMs = Math.min(this.reconnectDelayMs * 2, 30000);
    };

    socket.onerror = () => {
      if (!isCurrent()) return;
      this.notifyStatus("Error");
    };
  }

  send(payload) {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
      return;
    }

    this.socket.send(JSON.stringify(payload));
  }

  notifyStatus(status) {
    for (const handler of this.statusHandlers) {
      handler(status);
    }
  }
}

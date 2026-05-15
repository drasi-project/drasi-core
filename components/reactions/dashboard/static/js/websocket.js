export class DashboardSocket {
  constructor(path = "/ws") {
    this.path = path;
    this.socket = null;
    this.subscriptions = new Set();
    this.queryHandlers = [];
    this.statusHandlers = [];
    this.reconnectDelayMs = 1000;
    this.stopped = false;
  }

  start() {
    this.stopped = false;
    this.connect();
  }

  stop() {
    this.stopped = true;
    if (this.socket) {
      this.socket.close();
      this.socket = null;
    }
  }

  onQueryResult(handler) {
    this.queryHandlers.push(handler);
  }

  onStatus(handler) {
    this.statusHandlers.push(handler);
  }

  subscribe(queryIds) {
    for (const queryId of queryIds) {
      if (queryId && queryId.trim().length > 0) {
        this.subscriptions.add(queryId);
      }
    }

    this.send({
      type: "subscribe",
      query_ids: Array.from(this.subscriptions),
    });
  }

  unsubscribe(queryIds) {
    for (const queryId of queryIds) {
      this.subscriptions.delete(queryId);
    }

    this.send({
      type: "unsubscribe",
      query_ids: queryIds,
    });
  }

  connect() {
    const protocol = window.location.protocol === "https:" ? "wss" : "ws";
    const url = `${protocol}://${window.location.host}${this.path}`;

    this.socket = new WebSocket(url);
    this.notifyStatus("Connecting");

    this.socket.onopen = () => {
      this.reconnectDelayMs = 1000;
      this.notifyStatus("Connected");
      if (this.subscriptions.size > 0) {
        this.send({
          type: "subscribe",
          query_ids: Array.from(this.subscriptions),
        });
      }
    };

    this.socket.onmessage = (event) => {
      let payload = null;
      try {
        payload = JSON.parse(event.data);
      } catch (error) {
        console.warn("Ignoring invalid websocket payload", error);
        return;
      }

      if (payload.type === "query_result") {
        for (const handler of this.queryHandlers) {
          handler(payload);
        }
      }
    };

    this.socket.onclose = () => {
      this.notifyStatus("Disconnected");
      if (!this.stopped) {
        window.setTimeout(() => this.connect(), this.reconnectDelayMs);
        this.reconnectDelayMs = Math.min(this.reconnectDelayMs * 2, 30000);
      }
    };

    this.socket.onerror = () => {
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

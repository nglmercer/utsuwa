// Utsuwa native-host JS bridge.
//
// Installed as a wry initialization script, so it exists before any page
// script runs. Minimal surface:
//   window.utsuwa.invoke(method, params, options) -> Promise  (typed IPC)
//   window.utsuwa.bridgeVersion                   (protocol version, mirrors Rust)
//   window.utsuwa.markReady()                     (drain buffered host events)
//   window.utsuwa.isReady()                       (whether markReady ran)
//   window 'utsuwa-host-event' CustomEvent        (host -> frontend push)
//
// Event buffering: host events emitted before the page registers listeners
// (notably the startup `app.ready`) are buffered, not lost. Events emitted
// even before this script runs are stashed by Rust in
// `window.__utsuwaEarlyEvents` and drained here on load. The frontend
// registers its `utsuwa-host-event` listeners, completes the
// `host.frontend_ready` handshake, then calls `markReady()` to replay
// buffered events in order; later events dispatch immediately.
//
// invoke() rejects on timeout (default 60s, override per call) so a dead
// transport can never leave a Promise pending forever. Long-running tools
// stream progress over host events; the request itself resolves promptly.
//
// Outside the native host (plain browser dev), `window.ipc` is absent and
// invoke() rejects instead of silently failing.
(function () {
  if (window.utsuwa) return;

  var BRIDGE_VERSION = 1;
  var DEFAULT_INVOKE_TIMEOUT_MS = 60000;
  var MAX_BUFFERED_EVENTS = 500;

  var pending = new Map();
  var nextSeq = 0;
  var eventBuffer = [];
  var ready = false;

  function post(msg) {
    if (window.ipc && window.ipc.postMessage) {
      window.ipc.postMessage(JSON.stringify(msg));
    } else {
      throw new Error('no native host: window.ipc is unavailable');
    }
  }

  function newId() {
    if (window.crypto && window.crypto.randomUUID) return window.crypto.randomUUID();
    nextSeq += 1;
    return 'id-' + Date.now() + '-' + nextSeq;
  }

  function dispatch(event, data) {
    window.dispatchEvent(
      new CustomEvent('utsuwa-host-event', { detail: { event: event, data: data || {} } })
    );
  }

  // Drain events stashed before this script ran (see emit_script in Rust).
  var early = window.__utsuwaEarlyEvents;
  if (Array.isArray(early) && early.length > 0) {
    for (var i = 0; i < early.length; i++) {
      var item = early[i];
      if (item && typeof item.event === 'string') {
        eventBuffer.push({ event: item.event, data: item.data || {} });
      }
    }
  }
  try {
    delete window.__utsuwaEarlyEvents;
  } catch (e) {
    window.__utsuwaEarlyEvents = undefined;
  }

  window.utsuwa = {
    bridgeVersion: BRIDGE_VERSION,
    invoke: function (method, params, options) {
      var id = newId();
      var timeoutMs =
        options && typeof options.timeoutMs === 'number' && options.timeoutMs > 0
          ? Math.floor(options.timeoutMs)
          : DEFAULT_INVOKE_TIMEOUT_MS;
      return new Promise(function (resolve, reject) {
        var timer = setTimeout(function () {
          if (!pending.has(id)) return;
          pending.delete(id);
          var timeout = new Error('native host request timed out: ' + method);
          timeout.code = 'timeout';
          reject(timeout);
        }, timeoutMs);
        // Node test runners only (browsers return a number): don't let a
        // long default timeout hold the process open.
        if (timer && typeof timer.unref === 'function') timer.unref();
        pending.set(id, { resolve: resolve, reject: reject, timer: timer });
        try {
          post({ id: id, method: method, params: params || {} });
        } catch (e) {
          clearTimeout(timer);
          pending.delete(id);
          reject(e);
        }
      });
    },
    // Called by Rust with the dispatched response. Never called by pages.
    __resolve: function (id, ok, payload) {
      var p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      if (p.timer) clearTimeout(p.timer);
      if (ok) {
        p.resolve(payload);
      } else {
        var e = new Error((payload && payload.message) || 'ipc error');
        e.code = payload && payload.code;
        p.reject(e);
      }
    },
    // Called by Rust to push HostEvents. Buffered until markReady().
    __emit: function (event, data) {
      if (ready) {
        dispatch(event, data);
      } else {
        eventBuffer.push({ event: event, data: data || {} });
        // Bound the buffer: a chatty host must not grow memory without
        // limit when the page never drains (drop oldest, keep newest).
        if (eventBuffer.length > MAX_BUFFERED_EVENTS) eventBuffer.shift();
      }
    },
    // Called by the frontend after registering listeners + handshake.
    // Replays buffered events in order, then dispatches live. Returns the
    // replayed count. Idempotent: later calls replay nothing.
    markReady: function () {
      ready = true;
      var buffered = eventBuffer;
      eventBuffer = [];
      for (var j = 0; j < buffered.length; j++) {
        dispatch(buffered[j].event, buffered[j].data);
      }
      return buffered.length;
    },
    isReady: function () {
      return ready;
    },
    // Counts only (no payloads) for tests and diagnostics.
    __pendingCount: function () {
      return pending.size;
    },
    __bufferedCount: function () {
      return eventBuffer.length;
    }
  };
})();

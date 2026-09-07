// Utsuwa native-host JS bridge (Task 6).
//
// Installed as a wry initialization script, so it exists before any page
// script runs. Minimal surface:
//   window.utsuwa.invoke(method, params) -> Promise  (typed IPC requests)
//   window 'utsuwa-host-event' CustomEvent           (host -> frontend push)
//
// Outside the native host (plain browser dev), `window.ipc` is absent and
// invoke() rejects instead of silently failing.
(function () {
  if (window.utsuwa) return;
  var pending = new Map();
  function post(msg) {
    if (window.ipc && window.ipc.postMessage) {
      window.ipc.postMessage(JSON.stringify(msg));
    } else {
      throw new Error('no native host: window.ipc is unavailable');
    }
  }
  function newId() {
    if (window.crypto && window.crypto.randomUUID) return window.crypto.randomUUID();
    return 'id-' + Date.now() + '-' + pending.size;
  }
  window.utsuwa = {
    invoke: function (method, params) {
      var id = newId();
      return new Promise(function (resolve, reject) {
        pending.set(id, { resolve: resolve, reject: reject });
        try {
          post({ id: id, method: method, params: params || {} });
        } catch (e) {
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
      if (ok) {
        p.resolve(payload);
      } else {
        var e = new Error((payload && payload.message) || 'ipc error');
        e.code = payload && payload.code;
        p.reject(e);
      }
    },
    // Called by Rust to push HostEvents. Never called by pages.
    __emit: function (event, data) {
      window.dispatchEvent(
        new CustomEvent('utsuwa-host-event', { detail: { event: event, data: data || {} } })
      );
    }
  };
})();

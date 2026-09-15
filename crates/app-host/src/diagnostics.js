// Utsuwa native-host diagnostics script.
//
// Installed as a wry initialization script immediately after bridge.js, so it
// runs before any page script. Forwards sanitized page diagnostics to Rust
// over the typed `diagnostics.report` IPC method (fire-and-forget):
//   window 'error' event       -> { kind: 'window.error', ... }
//   'unhandledrejection'       -> { kind: 'unhandledrejection', ... }
//   console.error              -> { kind: 'console.error', ... }
//   DOMContentLoaded           -> { kind: 'domcontentloaded' }
//   window 'load'              -> { kind: 'load' }
//
// Debug-oriented only: Rust truncates, throttles, and log-gates these.
// Never throws into page code; every hook is wrapped so diagnostics can't
// break the app. Like all wry initialization scripts this bypasses the page
// CSP (user scripts are not subject to it).
(function () {
  if (window.__utsuwaDiagnosticsInstalled) return;
  window.__utsuwaDiagnosticsInstalled = true;

  function safeString(value, max) {
    try {
      if (value === undefined || value === null) return '';
      var s = typeof value === 'string' ? value : String(value);
      return s.length > max ? s.slice(0, max) : s;
    } catch (e) {
      return '?';
    }
  }

  function report(kind, fields) {
    try {
      var bridge = window.utsuwa;
      if (!bridge || typeof bridge.invoke !== 'function') return;
      var params = { kind: kind };
      if (fields) {
        for (var key in fields) {
          if (Object.prototype.hasOwnProperty.call(fields, key)) {
            params[key] = fields[key];
          }
        }
      }
      // Failures are swallowed: reporting must never break page code paths.
      bridge.invoke('diagnostics.report', params, { timeoutMs: 15000 }).catch(function () {});
    } catch (e) {
      /* never throw into page code */
    }
  }

  window.addEventListener('error', function (event) {
    var err = event && event.error;
    report('window.error', {
      message: safeString(event && event.message, 2000),
      url: safeString(event && event.filename, 500),
      line: (event && event.lineno) || 0,
      stack: safeString(err && err.stack, 2000)
    });
  });

  window.addEventListener('unhandledrejection', function (event) {
    var reason = event && event.reason;
    var message = reason instanceof Error ? reason.message : reason;
    var stack = reason instanceof Error ? reason.stack : undefined;
    report('unhandledrejection', {
      message: safeString(message, 2000),
      stack: safeString(stack, 2000)
    });
  });

  // Wrap console.error: keep original behavior, additionally forward.
  try {
    var originalError = console.error.bind(console);
    console.error = function () {
      try {
        var parts = [];
        for (var i = 0; i < arguments.length; i++) {
          var arg = arguments[i];
          if (arg instanceof Error) {
            parts.push(arg.stack || arg.message);
          } else if (typeof arg === 'string') {
            parts.push(arg);
          } else {
            try {
              parts.push(JSON.stringify(arg));
            } catch (e) {
              parts.push(String(arg));
            }
          }
        }
        report('console.error', { message: safeString(parts.join(' '), 2000) });
      } catch (e) {
        /* fall through to original */
      }
      return originalError.apply(null, arguments);
    };
  } catch (e) {
    /* console unavailable (very early frame states); skip wrapping */
  }

  try {
    document.addEventListener('DOMContentLoaded', function () {
      report('domcontentloaded', {
        url: safeString(window.location && window.location.href, 500)
      });
    });
    window.addEventListener('load', function () {
      report('load', { url: safeString(window.location && window.location.href, 500) });
    });
  } catch (e) {
    /* non-DOM context (tests); lifecycle markers unavailable */
  }
})();

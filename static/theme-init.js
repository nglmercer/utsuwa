// Theme pre-hydration init: applies the `dark` class before first paint so
// dark-mode users never see a light flash. External file (not inline) so the
// desktop CSP (`script-src 'self'` + hashes, no `unsafe-inline`) allows it in
// both the WebView (`companion://app/theme-init.js`) and the browser.
(function () {
	try {
		var colorMode = null;
		try {
			colorMode = localStorage.getItem('colorMode') || 'system';
		} catch (e) {
			colorMode = 'system';
		}
		var shouldBeDark;
		if (colorMode === 'system') {
			shouldBeDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
		} else {
			shouldBeDark = colorMode === 'dark';
		}
		if (shouldBeDark) {
			document.documentElement.classList.add('dark');
		}
	} catch (e) {
		/* theme is cosmetic: never break boot */
	}
})();

/** The three supported companion transports. */
export type CompanionTransport = 'native-agent' | 'direct' | 'server';

export interface CompanionTransportOptions {
	/** A usable `window.utsuwa.invoke` bridge was detected. */
	nativeHostAvailable: boolean;
	/** The frontend was built as a packaged native desktop build. */
	nativeBuildExpected: boolean;
	/** The selected provider is a browser-local provider. */
	localProvider: boolean;
}

/**
 * Select the transport before any provider request is made. A packaged build
 * with a missing bridge is an infrastructure error, never a reason to send a
 * tool-less request through the web transports.
 */
export function selectCompanionTransport(options: CompanionTransportOptions): CompanionTransport {
	if (options.nativeHostAvailable) return 'native-agent';
	if (options.nativeBuildExpected) {
		throw new Error(
			'Native host runtime was expected but the Utsuwa IPC bridge is unavailable.'
		);
	}
	return options.localProvider ? 'direct' : 'server';
}

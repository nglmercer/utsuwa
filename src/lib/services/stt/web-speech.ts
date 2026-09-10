import type { RecordedSttDiagnostics, RecordedSttSessionResult } from './recorded-stt.ts';

export interface SpeechRecognitionCallbacks {
	onResult: (transcript: string, isFinal: boolean) => void;
	onEnd: (result?: RecordedSttSessionResult) => void;
	onError: (error: string) => void;
	onAudioLevel?: (level: number) => void;
	onTranscriptionStart?: () => void;
	onDiagnostics?: (diagnostics: RecordedSttDiagnostics) => void;
	onSessionResult?: (result: RecordedSttSessionResult) => void;
}

// Type declarations for Web Speech API
interface SpeechRecognitionEvent extends Event {
	results: SpeechRecognitionResultList;
	resultIndex: number;
}

interface SpeechRecognitionResultList {
	length: number;
	item(index: number): SpeechRecognitionResult;
	[index: number]: SpeechRecognitionResult;
}

interface SpeechRecognitionResult {
	length: number;
	item(index: number): SpeechRecognitionAlternative;
	[index: number]: SpeechRecognitionAlternative;
	isFinal: boolean;
}

interface SpeechRecognitionAlternative {
	transcript: string;
	confidence: number;
}

interface SpeechRecognitionErrorEvent extends Event {
	error: string;
	message: string;
}

interface SpeechRecognition extends EventTarget {
	continuous: boolean;
	interimResults: boolean;
	lang: string;
	maxAlternatives: number;
	onresult: ((event: SpeechRecognitionEvent) => void) | null;
	onend: (() => void) | null;
	onerror: ((event: SpeechRecognitionErrorEvent) => void) | null;
	onstart: (() => void) | null;
	onaudiostart: (() => void) | null;
	onspeechstart: (() => void) | null;
	onspeechend: (() => void) | null;
	start(): void;
	stop(): void;
	abort(): void;
}

declare global {
	interface Window {
		SpeechRecognition?: new () => SpeechRecognition;
		webkitSpeechRecognition?: new () => SpeechRecognition;
	}
}

class WebSpeechService {
	private recognition: SpeechRecognition | null = null;
	private callbacks: SpeechRecognitionCallbacks | null = null;
	private isListening = false;
	private sessionId = 0;

	isSupported(): boolean {
		return !!(window.SpeechRecognition || window.webkitSpeechRecognition);
	}

	startListening(callbacks: SpeechRecognitionCallbacks): boolean {
		if (!this.isSupported()) {
			callbacks.onError('Speech recognition not supported in this browser');
			return false;
		}

		if (this.isListening) {
			return true;
		}

		const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
		const recognition = new SpeechRecognition!();
		const sessionId = ++this.sessionId;
		this.recognition = recognition;
		this.callbacks = callbacks;

		// Treat each activation as one utterance so Web Speech has the same
		// speak → pause → complete behavior as recorded STT.
		recognition.continuous = false;
		recognition.interimResults = true;
		recognition.lang = 'en-US';
		recognition.maxAlternatives = 1;

		recognition.onstart = () => {
			if (sessionId !== this.sessionId) return;
			this.isListening = true;
		};

		recognition.onresult = (event: SpeechRecognitionEvent) => {
			if (sessionId !== this.sessionId) return;
			let finalTranscript = '';
			let interimTranscript = '';

			for (let i = event.resultIndex; i < event.results.length; i++) {
				const result = event.results[i];
				const transcript = result[0].transcript;
				if (result.isFinal) {
					finalTranscript += transcript;
				} else {
					interimTranscript += transcript;
				}
			}

			if (finalTranscript) {
				this.callbacks?.onResult(finalTranscript, true);
			} else if (interimTranscript) {
				this.callbacks?.onResult(interimTranscript, false);
			}
		};

		recognition.onend = () => {
			this.finish(sessionId);
		};

		recognition.onspeechend = () => {
			if (sessionId !== this.sessionId) return;
			// `onend` remains the single completion path; stopping here asks the
			// browser to close this one-utterance recognition session.
			if (this.isListening) recognition.stop();
		};

		recognition.onerror = (event: SpeechRecognitionErrorEvent) => {
			if (sessionId !== this.sessionId) return;
			this.isListening = false;
			// Silently ignore these common non-error cases
			if (event.error === 'aborted' || event.error === 'no-speech') {
				this.finish(sessionId);
				return;
			}
			// Map error codes to user-friendly messages
			const errorMessages: Record<string, string> = {
				'not-allowed': 'Microphone access denied',
				'audio-capture': 'No microphone found',
				'network': 'Network error occurred',
				'service-not-allowed': 'Speech service not allowed'
			};
			const currentCallbacks = this.callbacks;
			this.callbacks = null;
			this.recognition = null;
			currentCallbacks?.onError(errorMessages[event.error] || `Speech error: ${event.error}`);
		};

		try {
			recognition.start();
			return true;
		} catch (e) {
			if (sessionId === this.sessionId) {
				this.callbacks = null;
				this.recognition = null;
				this.isListening = false;
			}
			callbacks.onError('Failed to start speech recognition');
			return false;
		}
	}

	stopListening(): void {
		if (this.recognition && this.isListening) {
			this.recognition.stop();
		}
	}

	abort(): void {
		const recognition = this.recognition;
		++this.sessionId;
		this.recognition = null;
		this.callbacks = null;
		this.isListening = false;
		recognition?.abort();
	}

	getIsListening(): boolean {
		return this.isListening;
	}

	private finish(sessionId: number): void {
		if (sessionId !== this.sessionId) return;
		this.isListening = false;
		const callbacks = this.callbacks;
		this.callbacks = null;
		this.recognition = null;
		callbacks?.onEnd();
	}
}

export const webSpeechService = new WebSpeechService();

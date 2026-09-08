<script lang="ts">
	import { onMount } from 'svelte';
	import { onHotkeyEvent } from '$lib/services/platform';
	import { overlayStore } from '$lib/stores/overlay.svelte';
	import { sttStore } from '$lib/stores/stt.svelte';

	interface Props {
		onSendMessage?: (text: string) => void;
	}

	let { onSendMessage }: Props = $props();

	onMount(() => {
		// Global shortcuts lived in the removed Tauri backend; the only
		// hotkey source left is the in-app event bus. In-app emitters can
		// still drive these handlers without any backend.

		// Handle push-to-talk
		const unsubPTTStart = onHotkeyEvent('ptt:start', () => {
			if (sttStore.isSupported()) {
				sttStore.startListening((text) => {
					onSendMessage?.(text);
				});
			}
		});

		const unsubPTTStop = onHotkeyEvent('ptt:stop', () => {
			// STT will automatically send on stop if there's a transcript
			sttStore.stopListening();
		});

		// Handle focus chat
		const unsubFocus = onHotkeyEvent('chat:focus', () => {
			overlayStore.setChatExpanded(true);
			overlayStore.activate();
		});

		return () => {
			unsubPTTStart();
			unsubPTTStop();
			unsubFocus();
		};
	});
</script>

<script lang="ts">
	import { Icon } from '$lib/components/ui';

	interface Props {
		onUpload: (file: File) => void;
	}

	let { onUpload }: Props = $props();
	let isDragging = $state(false);
	let fileInput: HTMLInputElement;

	function handleDragOver(e: DragEvent) {
		e.preventDefault();
		isDragging = true;
	}

	function handleDragLeave() {
		isDragging = false;
	}

	function handleDrop(e: DragEvent) {
		e.preventDefault();
		isDragging = false;

		const file = e.dataTransfer?.files[0];
		if (file && file.name.endsWith('.vrm')) {
			onUpload(file);
		}
	}

	function handleFileSelect(e: Event) {
		const input = e.target as HTMLInputElement;
		const file = input.files?.[0];
		if (file && file.name.endsWith('.vrm')) {
			onUpload(file);
		}
		// Reset input
		input.value = '';
	}

	function handleClick() {
		fileInput?.click();
	}
</script>

<div
	class="uploader"
	class:dragging={isDragging}
	ondragover={handleDragOver}
	ondragleave={handleDragLeave}
	ondrop={handleDrop}
	role="button"
	tabindex="0"
	onclick={handleClick}
	onkeydown={(e) => e.key === 'Enter' && handleClick()}
>
	<input
		type="file"
		accept=".vrm"
		bind:this={fileInput}
		onchange={handleFileSelect}
		style="display: none;"
	/>

	<div class="icon">
		<Icon name="upload" size={32} strokeWidth={1.5} />
	</div>
	<span class="label">Upload VRM</span>
	<span class="hint">Drag & drop or click to browse</span>
</div>

<style>
	.uploader {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 0.75rem;
		aspect-ratio: 1;
		background: var(--bg-secondary);
		border: 1px dashed var(--border-light);
		border-radius: var(--radius-lg);
		cursor: pointer;
		transition: border-color 0.15s, background 0.15s, box-shadow 0.15s;
	}

	.uploader:hover {
		border-color: var(--accent);
		background: var(--accent-subtle);
	}

	.uploader.dragging {
		border-color: var(--accent);
		background: var(--accent-subtle);
		box-shadow: var(--shadow-glow);
	}

	.icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 56px;
		height: 56px;
		background: var(--bg-tertiary);
		border-radius: var(--radius-full);
		color: var(--text-tertiary);
		transition: background 0.15s, color 0.15s;
	}

	.uploader:hover .icon,
	.uploader.dragging .icon {
		background: var(--accent);
		color: #fff;
	}

	.label {
		font-size: 0.9rem;
		font-weight: 600;
		color: var(--text-primary);
	}

	.hint {
		font-size: 0.75rem;
		color: var(--text-tertiary);
		transition: color 0.15s;
	}

	.uploader:hover .hint,
	.uploader.dragging .hint {
		color: var(--accent);
	}
</style>

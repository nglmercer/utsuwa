<script lang="ts">
	import { settingsStore } from '$lib/stores/settings.svelte';
	import { Icon } from '$lib/components/ui';
	import { MIN_PASSPHRASE_LENGTH, VaultError } from '$lib/services/security/vault';
	import './ai-services-settings.css';

	let passphrase = $state('');
	let confirm = $state('');
	let current = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);

	function resetForm() {
		passphrase = '';
		confirm = '';
		current = '';
		error = null;
	}

	async function run(action: () => Promise<boolean | void>, done: string) {
		if (busy) return;
		busy = true;
		error = null;
		notice = null;
		try {
			const ok = await action();
			if (ok === false) {
				error = 'Wrong passphrase.';
				return;
			}
			notice = done;
			resetForm();
		} catch (e) {
			error = e instanceof VaultError ? e.message : 'Vault operation failed.';
		} finally {
			busy = false;
		}
	}

	function unlock() {
		return run(() => settingsStore.unlockVault(passphrase), 'Vault unlocked.');
	}

	function setup() {
		if (passphrase !== confirm) {
			error = 'Passphrases do not match.';
			return Promise.resolve();
		}
		return run(() => settingsStore.setVaultPassphrase(passphrase).then(() => true), 'Vault enabled — stored keys are now encrypted.');
	}

	function change() {
		if (passphrase !== confirm) {
			error = 'New passphrases do not match.';
			return Promise.resolve();
		}
		return run(
			() => settingsStore.changeVaultPassphrase(current, passphrase),
			'Passphrase changed.'
		);
	}

	function remove() {
		if (!window.confirm('Remove the vault passphrase? Stored keys will be saved in plaintext again.')) {
			return;
		}
		if (!settingsStore.removeVaultPassphrase()) {
			error = 'Unlock the vault first.';
			return;
		}
		notice = 'Vault removed — keys are stored in plaintext.';
		resetForm();
	}

	function lock() {
		settingsStore.lockVault();
		resetForm();
		notice = null;
	}
</script>

<div class="service-group">
	<div class="service-header">
		<Icon name="lock" size={14} />
		<span>Key Vault</span>
	</div>

	{#if !settingsStore.vaultSet}
		<p class="provider-note provider-warning">
			<Icon name="alert-circle" size={14} />
			Stored API keys and MCP tokens are saved in plaintext in this browser. Set a vault
			passphrase to encrypt them (AES-256-GCM). You will unlock once per session.
		</p>
		<div class="vault-rows">
			<input
				type="password"
				class="api-key-input"
				placeholder={`New passphrase (min ${MIN_PASSPHRASE_LENGTH} characters)`}
				bind:value={passphrase}
				autocomplete="new-password"
			/>
			<input
				type="password"
				class="api-key-input"
				placeholder="Confirm passphrase"
				bind:value={confirm}
				autocomplete="new-password"
			/>
			<div class="server-actions">
				<button class="mini-btn" onclick={setup} disabled={busy || !passphrase || !confirm}>
					{busy ? 'Working…' : 'Enable vault'}
				</button>
			</div>
		</div>
	{:else if settingsStore.vaultLocked}
		<p class="provider-note provider-warning">
			<Icon name="lock" size={14} />
			The vault is locked — providers and MCP tools are unavailable until you unlock.
			There is no recovery: forgetting the passphrase means re-entering your keys.
		</p>
		{#if settingsStore.vaultPendingEdits}
			<p class="provider-note provider-warning">
				<Icon name="alert-circle" size={14} />
				Settings changed while the vault was locked. Unlock to merge and save them.
			</p>
		{/if}
		<div class="vault-rows">
			<input
				type="password"
				class="api-key-input"
				placeholder="Vault passphrase"
				bind:value={passphrase}
				autocomplete="current-password"
			/>
			<div class="server-actions">
				<button class="mini-btn" onclick={unlock} disabled={busy || !passphrase}>
					{busy ? 'Unlocking…' : 'Unlock'}
				</button>
			</div>
		</div>
	{:else}
		<p class="provider-note provider-info">
			<Icon name="check" size={14} />
			Vault unlocked — stored keys are encrypted at rest for this session.
		</p>
		<div class="vault-rows">
			<input
				type="password"
				class="api-key-input"
				placeholder="Current passphrase"
				bind:value={current}
				autocomplete="current-password"
			/>
			<input
				type="password"
				class="api-key-input"
				placeholder="New passphrase"
				bind:value={passphrase}
				autocomplete="new-password"
			/>
			<input
				type="password"
				class="api-key-input"
				placeholder="Confirm new passphrase"
				bind:value={confirm}
				autocomplete="new-password"
			/>
			<div class="server-actions">
				<button class="mini-btn" onclick={change} disabled={busy || !current || !passphrase}>
					{busy ? 'Working…' : 'Change passphrase'}
				</button>
				<button class="mini-btn" onclick={lock} disabled={busy}>Lock now</button>
				<button class="mini-btn danger" onclick={remove} disabled={busy}>Remove vault</button>
			</div>
		</div>
	{/if}

	{#if error}
		<p class="provider-note error">
			<Icon name="alert-circle" size={14} />
			{error}
		</p>
	{/if}
	{#if notice}
		<p class="provider-note provider-info">
			<Icon name="check" size={14} />
			{notice}
		</p>
	{/if}
</div>

<style>
	.vault-rows {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		margin-top: 0.5rem;
	}
	.mini-btn.danger {
		color: var(--color-danger, #f87171);
	}
</style>

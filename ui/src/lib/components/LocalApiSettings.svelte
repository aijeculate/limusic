<script lang="ts">
	// The Data tab's "Local API" section: the opt-in loopback control API for launchers
	// (src-tauri/src/local_api). The backend starts, stops and rebinds the server on each change,
	// and answers with its real state, so a taken port shows up here instead of only in the log.
	import { onMount } from 'svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import { copyText } from '$lib/clipboard';
	import * as api from '$lib/api';
	import { toast } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';

	const LABEL =
		'mb-2 px-1 text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground';
	const CARD = 'divide-y divide-border/60 overflow-hidden rounded-xl border bg-card';

	let status = $state<api.LocalApiStatus | null>(null);
	let portText = $state('');
	let busy = $state(false);
	let copied = $state(false);

	onMount(async () => {
		try {
			status = await api.localApiStatus();
			portText = String(status.port);
		} catch (e) {
			toast.error(String(e));
		}
	});

	const portValue = $derived(Number(portText));
	const portValid = $derived(Number.isInteger(portValue) && portValue >= 1024 && portValue <= 65535);
	const portDirty = $derived(!!status && portValid && portValue !== status.port);

	async function run(op: () => Promise<api.LocalApiStatus>) {
		busy = true;
		try {
			status = await op();
			portText = String(status.port);
			if (status.error) toast.error(status.error);
		} catch (e) {
			toast.error(String(e));
		} finally {
			busy = false;
		}
	}

	const setEnabled = (on: boolean) =>
		run(() => api.localApiConfigure(on, portValid ? portValue : (status?.port ?? 26538)));
	const applyPort = () => run(() => api.localApiConfigure(status?.enabled ?? false, portValue));
	const regenerate = () => run(api.localApiRegenerateToken);

	async function copyToken() {
		if (!status?.token) return;
		await copyText(status.token);
		copied = true;
		setTimeout(() => (copied = false), 1500);
	}
</script>

{#if status}
	<section class="mb-7 last:mb-1">
		<h3 class={LABEL}>{t('settings.sections.local_api')}</h3>
		<div class={CARD}>
			<div class="flex items-center justify-between gap-6 px-4 py-3.5">
				<div class="min-w-0">
					<span class="text-sm font-medium">{t('settings.local_api.enable')}</span>
					<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
						{t('settings.local_api.enable_hint')}
					</p>
					{#if status.running}
						<p class="mt-1 text-xs text-primary">
							{t('settings.local_api.running', { port: status.port })}
						</p>
					{:else if status.error}
						<p class="mt-1 text-xs text-destructive">{status.error}</p>
					{/if}
				</div>
				<Switch checked={status.enabled} disabled={busy} onCheckedChange={setEnabled} />
			</div>

			{#if status.enabled}
				<div class="flex items-center justify-between gap-6 px-4 py-3.5">
					<div class="min-w-0">
						<span class="text-sm font-medium">{t('settings.local_api.port')}</span>
						<p class="mt-1 text-xs text-muted-foreground">{t('settings.local_api.port_hint')}</p>
					</div>
					<div class="flex items-center gap-2">
						<Input
							class="h-8 w-24 font-mono"
							inputmode="numeric"
							bind:value={portText}
							aria-invalid={!portValid}
						/>
						<Button size="sm" variant="secondary" disabled={busy || !portDirty} onclick={applyPort}>
							{t('settings.local_api.apply')}
						</Button>
					</div>
				</div>

				<div class="px-4 py-3.5">
					<span class="text-sm font-medium">{t('settings.local_api.token')}</span>
					<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
						{t('settings.local_api.token_hint')}
					</p>
					<div class="mt-2.5 flex items-center gap-2">
						<Input class="h-8 min-w-0 flex-1 font-mono text-xs" readonly value={status.token} />
						<Button size="sm" variant="secondary" onclick={copyToken}>
							{copied ? t('settings.local_api.copied') : t('settings.local_api.copy')}
						</Button>
						<Button size="sm" variant="secondary" disabled={busy} onclick={regenerate}>
							{t('settings.local_api.regenerate')}
						</Button>
					</div>
				</div>
			{/if}
		</div>
	</section>
{/if}

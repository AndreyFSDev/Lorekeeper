<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from './lib/api';
  import { t } from './lib/i18n.svelte';
  import Icon from './lib/Icon.svelte';
  import type { GuideEntry, GuideSection } from './lib/types';


  let sections: GuideSection[] = $state([]);
  let loading = $state(true);
  let error: string | null = $state(null);
  let query = $state('');
  let collapsed = $state<Set<string>>(new Set());

  onMount(async () => {
    try {
      sections = await api.guideData();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  });

  function matches(entry: GuideEntry, q: string): boolean {
    const hay = [entry.title, entry.location, entry.usage, entry.note ?? '', entry.since ?? '']
      .join('\n')
      .toLowerCase();
    return hay.includes(q);
  }

  // The filter hides only non-matching entries and sections; collapsed sections
  // expand themselves while searching — otherwise a match stays invisible.
  let visible = $derived(
    query.trim()
      ? sections
          .map((s) => ({ ...s, entries: s.entries.filter((e) => matches(e, query.trim().toLowerCase())) }))
          .filter((s) => s.entries.length > 0)
      : sections,
  );

  function sectionKey(s: GuideSection): string {
    return s.version || 'basics';
  }

  function sectionTitle(s: GuideSection): string {
    return s.version ? `v${s.version}` : t('gd.basics');
  }

  function isCollapsed(s: GuideSection): boolean {
    return !query.trim() && collapsed.has(sectionKey(s));
  }

  function toggle(s: GuideSection) {
    const key = sectionKey(s);
    const next = new Set(collapsed);
    if (next.has(key)) {
      next.delete(key);
    } else {
      next.add(key);
    }
    collapsed = next;
  }
</script>

<div class="guide-root">
  <header class="guide-header" data-tauri-drag-region>
    <div class="title-group" data-tauri-drag-region>
      <div class="title-row">
        <span class="title-icon"><Icon name="cap" size={22} /></span>
        <h1>{t('gd.title')}</h1>
      </div>
      <p class="subtitle">
        {t('gd.subtitle')}
      </p>
    </div>
  </header>

  <div class="guide-search">
    <input
      type="search"
      placeholder={t('gd.search')}
      bind:value={query}
    />
    {#if query.trim()}
      <span class="search-count">
        {t('gd.matches', { count: visible.reduce((n, s) => n + s.entries.length, 0) })}
      </span>
    {/if}
  </div>

  <main class="guide-content">
    {#if loading}
      <p class="dim">{t('gd.loading')}</p>
    {:else if error}
      <p class="err">{t('gd.error', { error })}</p>
    {:else if visible.length === 0}
      <p class="dim">{t('gd.empty')}</p>
    {:else}
      {#each visible as section (sectionKey(section))}
        <section class="guide-section">
          <button class="section-header" onclick={() => toggle(section)}>
            <span class="chevron {isCollapsed(section) ? 'closed' : ''}">▾</span>
            <span class="section-title">{sectionTitle(section)}</span>
            {#if section.date}
              <span class="section-date">{section.date}</span>
            {/if}
            <span class="section-count">{section.entries.length}</span>
          </button>

          {#if !isCollapsed(section)}
            <div class="section-body">
              {#each section.entries as entry (entry.title)}
                <article class="entry-card">
                  <div class="entry-head">
                    <span class="entry-icon">{entry.icon}</span>
                    <b class="entry-title">{entry.title}</b>
                    {#if entry.since}
                      <span class="since-badge">{t('gd.since', { version: entry.since })}</span>
                    {/if}
                  </div>
                  <div class="entry-block">
                    <span class="entry-label">{t('gd.where')}</span>
                    <p>{entry.location}</p>
                  </div>
                  <div class="entry-block">
                    <span class="entry-label">{t('gd.how')}</span>
                    <p>{entry.usage}</p>
                  </div>
                  {#if entry.note}
                    <div class="entry-note">⚠ {entry.note}</div>
                  {/if}
                </article>
              {/each}
            </div>
          {/if}
        </section>
      {/each}
    {/if}
  </main>
</div>

<style>
  .guide-root {
    display: flex;
    flex-direction: column;
    /* The content has its own scroll area: html/body in app.css is pinned to the
       window height, and without an inner scrollport the version list hit the
       window bottom with no scrolling. Same scheme as .settings-content in Settings.svelte. */
    height: 100vh;
    overflow: hidden;
    background: transparent;
  }

  .guide-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
    padding: 18px 152px 12px 24px;
    background: var(--lg-fill-inset);
    backdrop-filter: none;
    border-bottom: 1px solid var(--lg-edge-dim);
  }

  .title-group {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .title-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .title-icon {
    font-size: 1.35rem;
  }

  h1 {
    margin: 0;
    font-size: 1.15rem;
    font-weight: 700;
    color: var(--text);
  }

  .subtitle {
    margin: 0;
    font-size: 0.82rem;
    color: var(--text-dim);
  }

  .guide-search {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 24px;
  }

  .guide-search input {
    flex: 1;
    padding: 9px 14px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--lg-fill-inset);
    color: var(--text);
    font-size: 0.88rem;
  }

  .guide-search input:focus {
    outline: none;
    box-shadow: none;
  }

  .search-count {
    font-size: 0.78rem;
    color: var(--text-dim);
    white-space: nowrap;
  }

  .guide-content {
    flex: 1;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 4px 24px 32px;
  }

  .dim {
    color: var(--text-dim);
  }

  .err {
    color: var(--err);
  }

  .guide-section {
    /* Without flex-shrink: 0 the sections shrank into 4–22px strips: these are
       flex children of the content column, and their height killed scrolling
       (overflow: hidden clipped the cards). A section takes the full content
       height; scrolling belongs to .guide-content. */
    flex-shrink: 0;
    border: 1px solid var(--border);
    border-radius: var(--lg-radius-inner);
    overflow: hidden;
  }

  .section-header {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 10px 16px;
    background: var(--lg-fill-inset);
    border: none;
    color: var(--text);
    cursor: pointer;
    font-size: 0.92rem;
    text-align: left;
    transition: background 0.15s;
  }

  .section-header:hover {
    background: var(--accent-soft);
  }

  .chevron {
    color: var(--text-dim);
    transition: transform 0.15s;
  }

  .chevron.closed {
    transform: rotate(-90deg);
  }

  .section-title {
    font-weight: 700;
  }

  .section-date {
    color: var(--text-dim);
    font-size: 0.8rem;
  }

  .section-count {
    margin-left: auto;
    font-size: 0.75rem;
    color: var(--text-dim);
    background: var(--accent-soft);
    border-radius: 999px;
    padding: 1px 9px;
  }

  .section-body {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 12px;
  }

  .entry-card {
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px;
    border: 1px solid var(--border);
    border-radius: var(--lg-radius-inner);
    background: var(--lg-fill-soft);
  }

  .entry-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .entry-icon {
    font-size: 1.05rem;
  }

  .entry-title {
    font-size: 0.92rem;
    color: var(--text);
  }

  .since-badge {
    font-size: 0.72rem;
    /* Light blue: the background under the cards is dark (luminance ~0.07), and
       #4e99a3 kept the badge contrast at 3.1–3.6:1 (small-text norm is 4.5:1). */
    color: var(--text);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid var(--accent-glow);
    border-radius: 999px;
    padding: 1px 9px;
  }

  .entry-block {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .entry-label {
    font-size: 0.72rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    /* Contrast against the dark card background: #4e99a3 gave 3.0–3.9:1. */
    color: var(--text);
  }

  .entry-block p {
    margin: 0;
    font-size: 0.85rem;
    line-height: 1.5;
    color: var(--text);
  }

  .entry-note {
    font-size: 0.8rem;
    line-height: 1.45;
    /* --text-dim on this background held 3.3–3.6:1; lighter — with margin up to 4.5. */
    color: var(--text-dim);
    border-left: 3px solid var(--accent-glow);
    padding-left: 10px;
  }
</style>

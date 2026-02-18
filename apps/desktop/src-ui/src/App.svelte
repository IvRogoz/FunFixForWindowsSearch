<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";

  type SearchItem = {
    file_id: number;
    display_name: string;
    full_path: string;
    size: number;
    mtime_unix_ms: number;
    score: number;
  };

  let query = "";
  let selected = 0;
  let searchInput: HTMLInputElement | null = null;
  let items: SearchItem[] = [
    {
      file_id: 1,
      display_name: "example.txt",
      full_path: "C:\\example\\example.txt",
      size: 1234,
      mtime_unix_ms: Date.now(),
      score: 1
    }
  ];

  function onKeyDown(event: KeyboardEvent) {
    if (event.key === "ArrowDown") {
      selected = Math.min(items.length - 1, selected + 1);
      event.preventDefault();
    } else if (event.key === "ArrowUp") {
      selected = Math.max(0, selected - 1);
      event.preventDefault();
    } else if (event.key === "Escape") {
      invoke("toggle_panel");
    }
  }

  onMount(() => {
    searchInput?.focus();
    window.addEventListener("keydown", onKeyDown);

    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  });
</script>

<main class="panel">
  <div class="prompt-row">
    <span class="prompt">&gt;</span>
    <input bind:this={searchInput} bind:value={query} placeholder="Type to search files..." />
  </div>

  <div class="status-row">SORT: relevance | RESULTS: {items.length}</div>

  <ul class="results" role="listbox" aria-label="Search results">
    {#each items as item, index (item.file_id)}
      <li class:selected={index === selected}>
        <span class="name">{item.display_name}</span>
        <span class="path">{item.full_path}</span>
      </li>
    {/each}
  </ul>

  <div class="hint-row">Enter open | Alt+Enter reveal | Esc hide</div>
</main>

# App Boundary Contract (draft)

All commands are invoked from the Iced UI layer to app service (`wizd`).

## Commands

- `toggle_panel() -> void`
- `search_start(request: SearchRequest) -> void`
- `search_cancel(request_id: string) -> void`
- `open_item(file_id: number) -> void`
- `reveal_item(file_id: number) -> void`
- `get_settings() -> Settings`
- `set_settings(partial: PartialSettings) -> Settings`
- `reindex_now() -> void`

## Events

- `index_progress`: `{ phase: string, scanned: number, total_estimate: number }`
- `search_chunk`: `{ request_id: string, items: SearchItem[] }`
- `search_done`: `{ request_id: string, total: number, took_ms: number }`
- `watch_status`: `{ healthy: boolean, mode: "usn" | "poll" }`

## SearchRequest

```ts
type SearchRequest = {
  request_id: string
  query: string
  sort: "relevance" | "name" | "path" | "date" | "size"
  limit: number
}
```

## SearchItem

```ts
type SearchItem = {
  file_id: number
  display_name: string
  full_path: string
  size: number
  mtime_unix_ms: number
  score: number
}
```

# Character pack

Cloufam ships no characters: neither this repository nor the releases contain
any. To add your own, create a `characters` folder next to `Cloufam.exe` and
put SillyTavern character cards (`*.png`) there. On every start each card is
copied into SillyTavern's library; cards that already exist there are never
overwritten.

An optional `tags.json` next to the cards adds a tag catalog and folder layout:

```json
{
  "default_character": "My_Character.png",
  "tags": [{ "id": "t-brave", "name": "brave", "folder_type": "NONE" }],
  "tag_map": { "My_Character.png": ["t-brave"] }
}
```

Without it Cloufam uses the built-in catalog in `src-tauri/default_tags.json`
(SillyTavern's stock cards only) and opens `default_Seraphina.png` first.
Cards and their art keep their own rights.


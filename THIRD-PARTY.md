# Third-party components

Cloufam is distributed under **AGPL-3.0-or-later**. It orchestrates several
independent projects, each with its own license. This file is a summary, not a
replacement for the license text shipped with each component.

| Component | License | How it is used |
|---|---|---|
| [SillyTavern](https://github.com/SillyTavern/SillyTavern) | AGPL-3.0 | Downloaded from upstream and patched at runtime (theme, locale, extensions, gateway hooks). The patched files remain AGPL-3.0. |
| [ComfyUI](https://github.com/comfyanonymous/ComfyUI) | GPL-3.0 | Optional image-generation backend; installed and run as a separate process. |
| [Ollama](https://github.com/ollama/ollama) | MIT | Portable runtime for local chat, vision and embedding models. |
| [Node.js](https://nodejs.org/) | MIT | Portable runtime used by SillyTavern. |
| [Tauri](https://tauri.app/) | MIT / Apache-2.0 | Desktop shell. |
| [Svelte](https://svelte.dev/) | MIT | UI framework. |
| [Moonlit Echoes](https://github.com/RivelleDays/SillyTavern-MoonlitEchoesTheme) | See `src-tauri/moonlit/LICENSE` | Bundled SillyTavern theme/extension; its own license applies. |

## Models

Models are **not** part of this repository. They are downloaded from Ollama,
Hugging Face or other registries under their own licenses. Check each model card
before use or redistribution.

## Character art and cards

Character images, character cards and third-party character descriptions are
**not** part of this repository. They are optional content and remain the
property of their respective rights holders.


# BIBIIWIKI

BIBIIWIKI is a Rust port of the `invest_loopx` investment `llm_wiki`, plus an
OpenAI Responses-compatible gateway for Codex. It preserves sources, converts
documents to Markdown with Firecrawl AnyDoc, turns them into linked bilingual knowledge, searches and
queries that memory, repairs links, and proposes new factors.

The semantic stages use the real Codex CLI. Codex connects to an ephemeral
local BIBIIWIKI gateway, and that gateway routes a LiteLLM-style model alias to
the configured provider. Local storage, search, linting, and indexing remain
deterministic.

## Implemented workflow

- `wiki ingest`: automatically create `purpose.md`, `schema.md`, component
  indexes, and the application-owned index when the selected root is blank;
  preserve immutable raw inputs; convert Word, PowerPoint, Excel,
  OpenDocument, RTF, EPUB, CSV, and PDF with AnyDoc (passing Markdown/text
  through); then analyze `wiki/sources` and materialize concept, entity,
  methodology, and factor pages.
- `wiki search`: run the Python-compatible local ranker and optionally save a
  bilingual search-memory page.
- `wiki query`: retrieve bounded evidence, answer locally or with Codex, and
  optionally save the grounded answer.
- `wiki update`: create missing-link stubs, canonicalize links, rebuild the
  index, lint the result, and append maintenance history.
- `wiki lint`: report broken links, unindexed pages, invalid frontmatter,
  invalid bilingual naming, and invalid headings.
- `wiki propose`: ask Codex for one structured factor proposal grounded in the
  highest-ranked wiki evidence.
- `wiki prompts`: list all 46 embedded Jinja templates (45 copied from Python
  plus the AnyDoc source-page renderer); the copied templates remain intact and
  all templates compile with strict undefined-variable behavior.

Agent writes cannot replace the body of an `active` page. Source backups are
SHA-256-addressed and never silently replace different bytes.
Generated dates use UTC+08:00.

## Quick start

### Windows release

Download `bibiiwiki-windows-x86_64.zip` from the
[GitHub Releases page](https://github.com/bibiparrot/bibiiwiki/releases),
extract it, and run `bibiiwiki.exe`. The archive includes the example
configuration, README, and GPL-3.0 license. The default LLM workflow also
requires a running Ollama server with the configured model and the Codex CLI.
Every push and pull request runs the Windows tests and release build in GitHub
Actions; a `v*` tag publishes the tested archive and its SHA-256 checksum.

The checked-in default uses the local Ollama model `ornith-1.5:9b`. On first
launch BIBIIWIKI creates the unified configuration at
`$HOME/.bibiiwik/bibiiwiki.yaml`
(`%USERPROFILE%\.bibiiwik\bibiiwiki.yaml` on Windows). An existing
`$HOME/bibiiwiki.yaml` is moved there automatically without overwriting a
configuration already present at the new path. Pull the
model if needed, then validate that per-user configuration:

```powershell
ollama pull ornith-1.5:9b
cargo run -- check
```

The default path is Codex → BIBIIWIKI at `http://127.0.0.1:4000/v1` → Ollama's
OpenAI-compatible endpoint at `http://127.0.0.1:11434/v1`. Use
`bibiiwiki.example.yaml` as the annotated provider-switching reference.

### Desktop UI

Open the native egui workspace with an initial wiki root:

```powershell
cargo run -- ui --wiki-root wiki_root
```

The iconless 36 px activity rail uses 28 px square text controls for workspace,
search, AI query, task, LLM, tool, and prompt views. Configuration inspectors
remain mutually exclusive center docks. While one is open it covers the Markdown center;
closing it, or clicking its selected rail icon again, restores the previous
Markdown preview/editor and its unsaved state. AI Query is an independent,
resizable right-side dock, matching Search's side-dock behavior without taking
over the center. Its green header triangle, activity-rail button, View menu
entry, and compact green restore triangle hide or restore it independently.
Opening a Markdown note from the workspace
tree also closes the covering inspector automatically and brings the editor to
the foreground. The workspace and search docks collapse
independently with their header triangles;
floating blue, orange, and green triangles restore workspace, search, and AI
query views respectively. **Add wiki root** (also available from the File menu
and toolbar) opens the operating system's directory picker; selecting a folder
registers it immediately, while cancelling or a picker error leaves the UI
open and unchanged. Exactly one wiki root is current: it is expanded with a
green open-folder icon, while every inactive root is collapsed with a black
closed-folder icon in the default light theme (and a readable neutral icon in
dark mode). The first root is the safe fallback selection. The workspace panel
can order roots by name, newest addition, or latest file change without changing
which root is current. Browse the current root's Markdown files in the
`egui`/`egui_extras` explorer, where every subdirectory switches between the
Font Awesome closed- and open-folder icons as it collapses or expands. Its
compact 20 px rows use blue directory labels/icons, subtle blue connector
lines, and restrained hover/selection fills in both themes. Click or double-click
a note to open the native CommonMark editor in
the central pane. The `egui_commonmark` editor provides Source, rendered
Preview, and side-by-side Split modes, safe in-workspace saves, and `Ctrl+S`;
the source view colors frontmatter, headings, list markers, links, inline code,
and fenced code without changing the saved bytes. Preview treats leading YAML
frontmatter as note metadata and renders the Markdown body with readable
headings and ordered or unordered list markers in both themes. File errors are
reported in the status area without closing the UI. AI question
input, rendered answers, and selected search evidence use the same Markdown
component and mode controls.
Search-result tiles use explicit normal, hovered, and selected colors rather
than inheriting egui's selection text. Light and dark palettes are stored in
`$HOME/.bibiiwik/color_theme.yaml`; edit the `#RRGGBB` values and choose
**View → Reload color theme** to apply them. Missing files are created from
accessible defaults, while malformed files are reported in the UI and never
prevent startup.
The LLM configuration dock edits the same per-user
`.bibiiwik/bibiiwiki.yaml` with
`yaml-edit` lossless parsing and saving, so
comments, whitespace, key order, and scalar formatting survive the edit cycle;
BIBIIWIKI's typed validation runs before any file is written. A guided current
model section appears above the YAML editor and configures the LiteLLM route in
this order: interface type (`CHAT`, `RESP`, `MSG`, `COMP`, `LOCAL`, or
`NATIVE`), provider, protocol endpoint and API base URL, API-key source, then
model name. API keys may be stored as a masked direct value or as an
`os.environ/VARIABLE` reference; unresolved references can be saved and are
required only when the route is actually loaded. Applying the wizard updates
the first `model_list` deployment and `codex.model` while preserving unrelated
YAML and comments. The complete generated LiteLLM YAML remains visible below
the wizard for review before saving. Its inline
**Test LLM connection** first calls the configured deployment through
the Rust LLM adapter, then sends the same prompt through a temporary loopback
OpenAI Responses proxy. A spinner identifies the active stage; successful
stages report **Configured LLM works** and **LLM proxy works** with their replies
inside the panel, without opening another window. Before a test starts, both
status rows and their empty reply boxes remain visible. A square status marker
also precedes **Testing LLM connection**, and the resolved route (for example,
`ornith-1.5:9b -> ollama/ornith-1.5:9b`) is shown independently of the model's
self-description. The blue circular progress indicator requests continuous
repaint frames while either network check is waiting.
The Tools dock keeps its quick path controls above a complete TOML editor backed
by the `toml` crate. Reload, validate, apply, or safely save ingest, Codex,
search, and query values; the TOML editor remains the editing surface, while
the parsed values are stored under `local:` in the unified per-user YAML.
Invalid TOML is never written or applied. Workspace roots, current selection,
sorting, theme, dock visibility, and all other durable UI defaults are stored
in that same section. Existing eframe state is migrated once; the application
no longer creates or updates `bibiiwiki.tools.toml`. The Prompt dock uses a
lossless Jinja2/Markdown syntax
highlighter for expressions, control statements, comments, filters, strings,
numbers, headings, and fenced blocks in both light and dark themes.
The **Language** menu uses `rust-i18n` TOML catalogs for English, Simplified
Chinese, Japanese, Latin, Korean, Russian, French, and Spanish. The default
`local.locale: system` follows the operating-system BCP 47 locale and falls
back to English when it is unsupported; an explicit selection is saved in the
same unified YAML. Platform CJK fonts are registered as fallbacks so translated
labels render without missing glyphs.
Double-click a workspace, or use its right-click menu, to open that Obsidian
vault. BIBIIWIKI resolves the exact vault ID from Obsidian's local registry. On
Windows, adding a wiki root immediately creates its encoded Obsidian URI,
initializes the root's `.obsidian` metadata directory, merges its vault ID into
`obsidian.json`, and creates the matching per-vault `<id>.json` index before any
open URI is launched. Opening also repairs missing artifacts from older
registrations. Existing vaults and unrelated Obsidian settings are preserved,
and malformed registry data is reported without being overwritten. A registry
failure is shown in status without removing the workspace from BIBIIWIKI. Choosing a
workspace's **Remove from list** action now opens a confirmation dialog that
shows the exact root and states that no folder or wiki file will be deleted.
The final workspace can also be removed; the app then shows an empty workspace
state and keeps **Add wiki root** available.
Double-click a search result to load that note in the central Markdown editor;
this also dismisses any covering Tasks/LLM/Tools/Prompts inspector and brings
the editor to the foreground. The result's right-click menu retains the
separate **Open note in Obsidian tab** action. Ingest, lint, update, and
AI-query jobs run through the same CLI pipeline in the background and report to
the resizable output dock. Clicking **Ingest** opens the Tasks inspector and an
inline **Ingest sources** tab directly below the task buttons; it does not open
an egui popup window. The tab has one **Select files or directories…** control.
Its **Files…** mode accepts multiple PDF, Markdown, text, CSV, or Excel files,
while **Directories…** accepts multiple folders and scans them recursively.
Each mode opens the corresponding native operating-system picker from the same
control. Cancellation or picker errors remain inline and never start an ingest
job, and the tab can be closed
independently. The **View** menu restores a hidden toolbar or output
dock and switches between the Light and Dark themes. New installations default
to Light; the selected theme is persisted with the other UI preferences.
On Windows, the executable is linked as a GUI-subsystem application, so bare and
explicit `ui` launches create no Command Prompt or Windows Terminal window.

Workspace and `wiki search` queries include every `.md` and `.markdown` file
below the selected wiki root, including root-level files such as `purpose.md`.
Ripgrep 15.2.0's reusable `ignore`, `grep-searcher`, and `grep-regex` components
provide the automatically sized parallel discovery and recovery path. The scan
feeds a persistent Tantivy 0.26 index under
`$HOME/.bibiiwik/search-indexes/`; unchanged indexes are reused, while added,
changed, or removed Markdown files trigger a refresh. Tantivy selects indexed
English and CJK candidates, then the deterministic Python-compatible ranker
preserves the established scores and ordering. A missing, locked, unwritable,
or invalid index never blocks search: ripgrep serves the request directly and
the GUI status identifies the active backend. Results use paths relative to
the wiki root, so preview, editor, and Obsidian actions resolve the same file.
CLI subcommands attach to an existing parent terminal when one is available and
retain their normal console output.

Desktop controls use compile-time Font Awesome SVGs from `pictogram` and
`pictogram-icons-font-awesome`. Font Awesome icons retain their upstream
CC BY 4.0 license; the Rust integration crates are MIT/Apache-2.0.

Ingest a project; a blank wiki root is initialized automatically:

```powershell
cargo run -- wiki ingest `
  --source-dir wiki_sources `
  --wiki-root wiki_root `
  --workspace .

# Explicit files and directories override the configured source folder.
cargo run -- wiki ingest `
  --input wiki_sources\paper.pdf `
  --input wiki_sources\factor_data.xlsx `
  --wiki-root wiki_root

# Continue the latest failed/interrupted session, or deliberately redo it.
cargo run -- wiki ingest --wiki-root wiki_root --resume
cargo run -- wiki ingest --wiki-root wiki_root --redo
```

Search and query it:

```powershell
# Fully offline; does not load bibiiwiki.yaml.
cargo run -- wiki search momentum --wiki-root wiki_root --no-save
cargo run -- wiki query "How is momentum defined?" `
  --wiki-root wiki_root --no-llm --no-save

# Codex synthesis and persistent bilingual query memory.
cargo run -- wiki query "How is momentum defined?" --wiki-root wiki_root

cargo run -- wiki update --wiki-root wiki_root
cargo run -- wiki lint --wiki-root wiki_root
cargo run -- wiki propose "A robust medium-horizon momentum signal" `
  --wiki-root wiki_root
```

By default, semantic sessions are persisted below `.bibiiwiki/Agent/`.
Source-analysis JSON is content-addressed below that state directory, so an
unchanged source and analyzer contract do not consume another model call.
Large extracted Markdown documents are split with `text_splitter::MarkdownSplitter`,
which prefers CommonMark headings, blocks, lists, tables, and sentences before
falling back to Unicode-safe character boundaries. The active deployment's
`chunking.model_max_characters` limit controls each semantic call, and the
structured chunk results are merged deterministically. Before any wiki-component
extraction begins, ingest runs a three-part **Check LLM availability** preflight:
the configured provider is called directly, the same request is sent through the
temporary LiteLLM-compatible Responses proxy, and the Codex agent is tested through
that proxy. Each substep is shown and persisted independently; a failure stops the
run before semantic extraction and prints the failing route's complete error chain.
If Codex or the provider is
temporarily unavailable, ingest still preserves the complete extracted text,
writes searchable `pending-analysis` pages and manifests before stopping the
semantic stage. Each wiki root atomically persists the current source
snapshot, configuration fingerprint, stage metrics, and failure under
`.bibiiwiki/ingest-status.json`. The GUI exposes **Continue ingest** after a
failure or interrupted process: it reuses converted Markdown and successful
content-addressed chunk analyses before retrying the failed work. It rejects a
continue request if a source, selected model, or chunk limit changed. **Redo
ingest** deliberately creates a new session from the same source snapshot and
bypasses both extraction and semantic-analysis caches.
Likewise, a failed interactive LLM query returns formula-aware grounded local
evidence as Markdown instead of leaving the GUI job failed or stuck.

## Wiki layout

```text
wiki_root/
  purpose.md
  schema.md
  raw/
    sources/       byte-for-byte document backups
    extracted/     Markdown conversion cache manifests
  wiki/
    sources/       AnyDoc-converted Markdown and LLM source analysis
    concepts/
    entities/
    factors/
    methodology/
    experience/
    queries/
    index.md
    log.md
```

Pages use YAML frontmatter and `[[path/to/page|Label]]` links. Addressable
knowledge pages follow `English_Chinese.md`; titles are Chinese-first,
`中文 (English)`. Original formulas, expressions, identifiers, and citations
remain unchanged in page bodies.

## Gateway and Codex adapter

The gateway accepts the OpenAI Responses protocol that Codex uses, converts
requests and tool calls to the selected provider protocol, and returns Codex's
Responses SSE events. `anthropic/...` and Azure Anthropic use the vendored
LiteLLM Rust Messages path. Other prefixes use an OpenAI-compatible
`/v1/chat/completions` endpoint, including a full LiteLLM proxy, Ollama, and
vLLM.

```yaml
model_list:
  - model_name: ornith-1.5:9b
    litellm_params:
      model: ollama/ornith-1.5:9b
      api_base: http://127.0.0.1:11434/v1

chunking:
  default_max_characters: 10000
  model_max_characters:
    "ornith-1.5": 10000
    "qwen3": 12000
    "deepseek-chat": 24000
    "deepseek-v4-flash": 24000
    "deepseek-v4-pro": 24000
    "gpt-5.6": 64000
    "claude-sonnet-5": 64000
    "gemini-2.5": 64000

codex:
  binary: codex
  model: ornith-1.5:9b
  reasoning_effort: none
```

`reasoning_effort` accepts `none`, `minimal`, `low`, `medium`, `high`, or
`xhigh`. The checked-in Ornith profile uses `none`; the proxy consequently
sends Ollama `think: false`, avoiding unsupported reasoning work while leaving
higher efforts available for thinking-capable models.

Chunking keys may be exact deployment aliases, full provider model IDs, or
provider-model prefixes. Exact matches win, followed by the longest prefix and
then `default_max_characters`. The GUI's LLM wizard edits the selected model's
value. Ingest reports file conversion, Markdown chunk planning, all three LLM
preflight checks, and semantic chunk completion with live progress bars and
count-bearing terminal lines.

Run Codex interactively through a temporary gateway:

```powershell
cargo run -- codex -- exec "Summarize this repository"
```

Or run a persistent gateway and print the matching Codex provider block:

```powershell
cargo run --release -- serve
cargo run -- codex-config
```

The server defaults to loopback and refuses an unauthenticated non-loopback
bind. Supported secret references are `os.environ/NAME`, `env/NAME`, and
`${NAME}`. Non-interactive Codex jobs have an outer deadline equal to the
configured provider request timeout plus a 30-second shutdown grace period;
timed-out children are terminated so a slow or retrying provider cannot pin a
background UI job indefinitely.

## Python compatibility fixture

`tests/fixtures/python_wiki_root` is an unchanged copy of the supplied Python
wiki output. Its adjacent provenance file records the source path and copy
details. Compatibility tests exercise exact search ranking, page authority,
prompt compilation, query artifacts, maintenance, raw backup behavior, and the
486-factor XLSX catalog. The source Python repository itself is not modified.

## Verification

```powershell
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings

# Installed-Codex integration tests, backed by an offline mock LLM endpoint.
cargo test --test wiki_compat -- --ignored --nocapture

# Live local-model check through the in-process Rust Responses proxy.
cargo test --test ollama_live -- --ignored --nocapture
```

## Current boundaries

- PDF ingestion extracts text by page. The Python version's embedded-image
  extraction and Codex media selection are not yet ported.
- Exact duplicate bilingual catalog factors are consolidated with all formula,
  expression, and provenance variants. Semantic near-duplicate factor linking
  and automatic symbolic formula derivation remain future extensions.
- Provider calls are currently buffered before a short Responses SSE sequence;
  keep-alives prevent Codex from treating long calls as idle.
- Provider-specific hosted tools such as web search are not translated, and the
  gateway does not implement the Responses WebSocket transport.

See [docs/architecture.md](docs/architecture.md) for module boundaries and
upstream compatibility details.

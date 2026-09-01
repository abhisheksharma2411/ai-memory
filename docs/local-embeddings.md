# Local embeddings

*2.0 item 5.* `AI_MEMORY_EMBEDDING_PROVIDER=local` runs sentence
embeddings **in-process** — no API key, no external server, no GPU
required. Pure-Rust BERT inference (candle) with
`all-MiniLM-L6-v2` (384-dim), the sentence-transformers workhorse the
comparable memory servers ship by default.

```toml
# config.toml
embedding_provider = "local"     # model/dim default correctly
```

## The model files

The binary does not bundle the model (~87 MB). On the first start with
`local` configured, the server fetches three files into
`<data_dir>/models/all-MiniLM-L6-v2/` — each verified against a sha256
pinned in the source, so a drifted or tampered upstream file fails
loudly instead of silently changing every vector:

| file | sha256 (pinned 2026-09-01) |
|---|---|
| `model.safetensors` | `53aa5117…` |
| `tokenizer.json` | `be50c362…` |
| `config.json` | `953f9c0d…` |

**Offline installs**: download the three files from
`https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/`
on any machine and drop them into the directory above; the loader
verifies the same checksums and never touches the network. The model
is Apache-2.0.

## Coexistence and migration

Nothing is forced. `(provider, model, dim)` is stored on every
embedding row, and hybrid search ignores vectors whose triple does not
match the configured embedder — so local vectors sit beside any
provider vectors you already have, and switching back is a config
change. `ai-memory embed --force` re-embeds a project under the
current provider when you want one consistent set.

Existing installs keep their configured provider; `local` is opt-in.
Fresh-install defaults are decided by benchmark numbers, not vibes —
see `docs/benchmarks/` for the zero-LLM vs local-embeddings
LongMemEval rows, reproducible via:

```bash
cargo run --release -p ai-memory-eval -- retrieval --embeddings local
```

## Operational notes

- Inference is CPU, off the async runtime (`spawn_blocking`);
  first-load reads ~87 MB into memory once per server process.
- The `local-embeddings` cargo feature (default on) carries the ML
  dependency tree; a slim build can disable it and keep every other
  provider.
- Tokenization truncates at 512 tokens — the model's positional limit;
  page bodies beyond that contribute their head, same contract as the
  hosted embedders.
